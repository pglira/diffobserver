//! Background worker thread: owns the resolved baseline and the syntax
//! highlighter, and performs all scanning, diffing, and snapshotting so the
//! UI thread never blocks on IO or highlighting.
//!
//! The baseline is created and kept entirely on this thread (the
//! `gix::Repository` it may hold is `!Sync`, and thread confinement keeps the
//! design simple); only plain data crosses the channels.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result};

use crate::app::{Arrival, Event};
use crate::core::baseline::{BaselineRef, BaselineSource, GitHeadBaseline, SnapshotBaseline};
use crate::core::model::{ChangeKind, FileChange};
use crate::core::scan::{self, ScanConfig};
use crate::core::snapshot;
use crate::ui::diffview::Prepared;
use crate::ui::highlight::Highlighter;

/// Requests from the UI thread to the worker.
pub enum Req {
    SetBaseline(BaselineRef),
    Rescan,
    DiffFile {
        path: PathBuf,
        kind: ChangeKind,
        /// Opaque to the worker; echoed back with the result.
        arrival: Arrival,
    },
    SaveSnapshot { name: String },
    Shutdown,
}

/// Results from the worker back to the UI thread.
pub enum Evt {
    BaselineSet { label: String, reff: BaselineRef },
    Scanned(Vec<FileChange>),
    Diff(Box<Prepared>, Arrival),
    SnapshotSaved(String),
    Error(String),
}

/// Spawn the worker thread; returns the request sender.
pub fn spawn(
    root: PathBuf,
    cfg: ScanConfig,
    theme_name: String,
    syntax_highlight: bool,
    evt_tx: Sender<Event>,
) -> Sender<Req> {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    std::thread::Builder::new()
        .name("diffobserver-worker".into())
        .spawn(move || {
            let highlighter = Highlighter::new(&theme_name, syntax_highlight);
            run(root, cfg, highlighter, req_rx, evt_tx)
        })
        .expect("spawn worker thread");
    req_tx
}

fn run(
    root: PathBuf,
    cfg: ScanConfig,
    highlighter: Highlighter,
    req_rx: mpsc::Receiver<Req>,
    evt_tx: Sender<Event>,
) {
    let mut baseline: Option<Box<dyn BaselineSource>> = None;
    let mut current_ref: Option<BaselineRef> = None;
    let mut pending: VecDeque<Req> = VecDeque::new();

    loop {
        let req = match pending.pop_front() {
            Some(r) => r,
            None => match req_rx.recv() {
                Ok(r) => r,
                Err(_) => break,
            },
        };

        // Coalesce bursts: a Rescan is pointless if another request that will
        // itself produce a fresh scan is already queued behind it.
        if matches!(req, Req::Rescan) {
            while let Ok(next) = req_rx.try_recv() {
                pending.push_back(next);
            }
            let superseded = pending
                .iter()
                .any(|r| matches!(r, Req::Rescan | Req::SetBaseline(_)));
            if superseded {
                continue;
            }
        }

        match req {
            Req::SetBaseline(reff) => match resolve(&root, &reff) {
                Ok(b) => {
                    let _ = evt_tx.send(Event::Worker(Evt::BaselineSet {
                        label: b.label().to_string(),
                        reff: reff.clone(),
                    }));
                    baseline = Some(b);
                    current_ref = Some(reff);
                    scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx);
                }
                Err(e) => emit_err(&evt_tx, format!("baseline: {e:#}")),
            },
            Req::Rescan => scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx),
            Req::DiffFile { path, kind, arrival } => {
                if let Some(b) = baseline.as_deref() {
                    match scan::diff_file(&root, b, &path, kind, &cfg) {
                        Ok(fd) => {
                            let prepared = Prepared::build(&fd, &highlighter);
                            let _ = evt_tx
                                .send(Event::Worker(Evt::Diff(Box::new(prepared), arrival)));
                        }
                        Err(e) => emit_err(&evt_tx, format!("diff {}: {e:#}", path.display())),
                    }
                }
            }
            Req::SaveSnapshot { name } => match snapshot::save(&root, &name) {
                Ok(()) => {
                    let _ = evt_tx.send(Event::Worker(Evt::SnapshotSaved(name)));
                    // Advance the baseline to the new snapshot only when the
                    // user is already tracking `latest` (or nothing resolved
                    // yet) — never hijack an explicitly chosen baseline like
                    // HEAD or an older snapshot.
                    let tracking_latest =
                        matches!(current_ref, None | Some(BaselineRef::Latest));
                    if tracking_latest {
                        match resolve(&root, &BaselineRef::Latest) {
                            Ok(b) => {
                                let _ = evt_tx.send(Event::Worker(Evt::BaselineSet {
                                    label: b.label().to_string(),
                                    reff: BaselineRef::Latest,
                                }));
                                baseline = Some(b);
                                current_ref = Some(BaselineRef::Latest);
                                scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx);
                            }
                            Err(e) => emit_err(&evt_tx, format!("baseline: {e:#}")),
                        }
                    }
                }
                Err(e) => emit_err(&evt_tx, format!("snapshot: {e:#}")),
            },
            Req::Shutdown => break,
        }
    }
}

fn scan_and_send(
    root: &std::path::Path,
    baseline: Option<&dyn BaselineSource>,
    cfg: &ScanConfig,
    evt_tx: &Sender<Event>,
) {
    let Some(b) = baseline else { return };
    match scan::scan(root, b, cfg) {
        Ok(mut v) => {
            v.sort_by(|a, b| a.path.cmp(&b.path));
            let _ = evt_tx.send(Event::Worker(Evt::Scanned(v)));
        }
        Err(e) => emit_err(evt_tx, format!("scan: {e:#}")),
    }
}

fn resolve(root: &std::path::Path, reff: &BaselineRef) -> Result<Box<dyn BaselineSource>> {
    match reff {
        BaselineRef::Latest => {
            let dir = snapshot::latest_dir(root)
                .context("no snapshot yet — press S to take one")?;
            let name = snapshot::latest_name(root).unwrap_or_else(|| "latest".into());
            Ok(Box::new(SnapshotBaseline::new(dir, format!("snap:{name}"))))
        }
        BaselineRef::Snapshot(name) => {
            let dir = snapshot::snapshot_path(root, name);
            if !dir.is_dir() {
                anyhow::bail!("snapshot '{name}' not found");
            }
            Ok(Box::new(SnapshotBaseline::new(dir, format!("snap:{name}"))))
        }
        BaselineRef::GitHead => Ok(Box::new(GitHeadBaseline::open(root)?)),
    }
}

fn emit_err(evt_tx: &Sender<Event>, msg: String) {
    let _ = evt_tx.send(Event::Worker(Evt::Error(msg)));
}

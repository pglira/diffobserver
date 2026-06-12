//! Background worker thread: owns the resolved baseline and performs all
//! scanning, diffing, and snapshotting so the UI thread never blocks on IO.
//!
//! The baseline (which may hold a non-`Send` `gix::Repository`) is created and
//! kept entirely on this thread; only plain data crosses the channels.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result};

use crate::app::{Arrival, Event};
use crate::core::baseline::{BaselineRef, BaselineSource, GitHeadBaseline, SnapshotBaseline};
use crate::core::model::{ChangeKind, FileChange, FileDiff};
use crate::core::scan::{self, ScanConfig};
use crate::core::snapshot;

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
    Diff(FileDiff, Arrival),
    SnapshotSaved(String),
    Error(String),
}

/// Spawn the worker thread; returns the request sender.
pub fn spawn(root: PathBuf, cfg: ScanConfig, evt_tx: Sender<Event>) -> Sender<Req> {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    std::thread::Builder::new()
        .name("diffobserver-worker".into())
        .spawn(move || run(root, cfg, req_rx, evt_tx))
        .expect("spawn worker thread");
    req_tx
}

fn run(root: PathBuf, cfg: ScanConfig, req_rx: mpsc::Receiver<Req>, evt_tx: Sender<Event>) {
    let mut baseline: Option<Box<dyn BaselineSource>> = None;

    while let Ok(req) = req_rx.recv() {
        match req {
            Req::SetBaseline(reff) => match resolve(&root, &reff) {
                Ok(b) => {
                    let _ = evt_tx.send(Event::Worker(Evt::BaselineSet {
                        label: b.label().to_string(),
                        reff,
                    }));
                    baseline = Some(b);
                    scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx);
                }
                Err(e) => emit_err(&evt_tx, format!("baseline: {e:#}")),
            },
            Req::Rescan => scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx),
            Req::DiffFile { path, kind, arrival } => {
                if let Some(b) = baseline.as_deref() {
                    match scan::diff_file(&root, b, &path, kind, &cfg) {
                        Ok(fd) => {
                            let _ = evt_tx.send(Event::Worker(Evt::Diff(fd, arrival)));
                        }
                        Err(e) => emit_err(&evt_tx, format!("diff {}: {e:#}", path.display())),
                    }
                }
            }
            Req::SaveSnapshot { name } => match snapshot::save(&root, &name) {
                Ok(()) => {
                    let _ = evt_tx.send(Event::Worker(Evt::SnapshotSaved(name)));
                    // The new snapshot becomes the latest baseline.
                    if let Ok(b) = resolve(&root, &BaselineRef::Latest) {
                        let _ = evt_tx.send(Event::Worker(Evt::BaselineSet {
                            label: b.label().to_string(),
                            reff: BaselineRef::Latest,
                        }));
                        baseline = Some(b);
                        scan_and_send(&root, baseline.as_deref(), &cfg, &evt_tx);
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

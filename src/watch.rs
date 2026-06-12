//! Filesystem watcher: debounced inotify events forwarded to the UI thread.

use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};

use crate::app::Event;

/// The watcher handle; keep it alive for the duration of the app.
pub type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Start watching `root` recursively. The returned debouncer must be held for
/// as long as watching should continue (dropping it stops the watch).
pub fn spawn(root: &Path, evt_tx: Sender<Event>) -> Result<Watcher> {
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        match res {
            Ok(events) => {
                let mut paths = Vec::new();
                for ev in events {
                    paths.extend(ev.paths.iter().cloned());
                }
                if !paths.is_empty() {
                    let _ = evt_tx.send(Event::Fs(paths));
                }
            }
            // Surface watcher failures (inotify overflow etc.) instead of
            // letting a dead watcher masquerade as "no changes".
            Err(errors) => {
                let msg = errors
                    .first()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown watch error".into());
                let _ = evt_tx.send(Event::WatchError(msg));
            }
        }
    })?;
    debouncer.watch(root, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

//! Filesystem watcher: debounced inotify events forwarded to the UI thread.

use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};

use crate::app::Event;

/// The watcher handle; keep it alive for the duration of the app.
pub type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Start watching `root` recursively. The returned debouncer must be held for
/// as long as watching should continue (dropping it stops the watch).
pub fn spawn(root: &Path, evt_tx: Sender<Event>) -> Result<Watcher> {
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        if let Ok(events) = res {
            let mut paths = Vec::new();
            for ev in events {
                paths.extend(ev.paths.iter().cloned());
            }
            if !paths.is_empty() {
                let _ = evt_tx.send(Event::Fs(paths));
            }
        }
    })?;
    debouncer.watch(root, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

//! Central application state and event handling.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::config::Config;
use crate::core::baseline::BaselineRef;
use crate::core::model::FileChange;
use crate::core::snapshot;
use crate::ui::diffview::Prepared;
use crate::ui::theme::DiffPalette;
use crate::ui::tree::Tree;
use crate::worker::{self, Req};

/// How long a toast stays visible.
pub const TOAST_TTL: Duration = Duration::from_secs(5);

/// Everything that can wake the UI thread.
pub enum Event {
    Input(CtEvent),
    Worker(worker::Evt),
    Fs(Vec<PathBuf>),
    WatchError(String),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Focus {
    Tree,
    Diff,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Overlay {
    None,
    Help,
    Prompt,
    Picker,
}

/// How to position the diff when its computation arrives. Travels with the
/// request through the worker and is echoed back with the result, so that
/// overlapping in-flight requests each apply their own intent.
#[derive(Clone, Copy)]
pub enum Arrival {
    Fresh,
    Refresh,
    JumpFirst,
    JumpLast,
}

pub struct PickerItem {
    pub label: String,
    pub reff: BaselineRef,
}

pub struct App {
    pub root: PathBuf,
    pub repo_is_git: bool,
    pub cfg: Config,
    req_tx: Sender<Req>,

    pub changes: Vec<FileChange>,
    pub tree: Tree,
    pub baseline_label: String,
    baseline_ref: BaselineRef,

    pub current_path: Option<PathBuf>,
    pub prepared: Option<Prepared>,
    pub diff_scroll: usize,
    current_hunk: usize,
    hunk_offsets: Vec<usize>,

    pub focus: Focus,
    pub overlay: Overlay,
    pub split: bool,
    pub tree_width: u16,

    pub toast: Option<(String, Instant)>,
    pub help_scroll: u16,

    pub prompt_input: String,
    pub picker_items: Vec<PickerItem>,
    pub picker_state: ListState,
    /// While `Some(name)`, a delete of that snapshot is armed and one more
    /// `d` confirms it; any other key cancels.
    pub picker_pending_delete: Option<String>,

    /// Gitignore matcher used to drop fs events for ignored paths (e.g.
    /// `target/` churn during builds) before they trigger rescans.
    fs_ignore: Option<Gitignore>,
    pub palette: DiffPalette,

    pub diff_area_height: usize,
    pub diff_total_rows: usize,

    pub launch_editor: Option<(PathBuf, usize)>,
    pub should_quit: bool,
}

impl App {
    pub fn new(root: PathBuf, repo_is_git: bool, cfg: Config, req_tx: Sender<Req>) -> Self {
        let palette = DiffPalette::for_mode(cfg.mode);
        let tree_width = cfg.tree_width_percent();
        let fs_ignore = build_fs_ignore(&root);

        // Default baseline: latest snapshot if one exists, else HEAD.
        let baseline_ref = if snapshot::latest_dir(&root).is_some() {
            BaselineRef::Latest
        } else if repo_is_git {
            BaselineRef::GitHead
        } else {
            BaselineRef::Latest
        };

        App {
            root,
            repo_is_git,
            cfg,
            req_tx,
            changes: Vec::new(),
            tree: Tree::new(),
            baseline_label: "(resolving…)".into(),
            baseline_ref,
            current_path: None,
            prepared: None,
            diff_scroll: 0,
            current_hunk: 0,
            hunk_offsets: Vec::new(),
            focus: Focus::Tree,
            overlay: Overlay::None,
            split: false,
            tree_width,
            toast: None,
            help_scroll: 0,
            prompt_input: String::new(),
            picker_items: Vec::new(),
            picker_state: ListState::default(),
            picker_pending_delete: None,
            fs_ignore,
            palette,
            diff_area_height: 1,
            diff_total_rows: 0,
            launch_editor: None,
            should_quit: false,
        }
    }

    /// Kick off the initial baseline resolution + scan.
    pub fn start(&mut self) {
        self.send(Req::SetBaseline(self.baseline_ref.clone()));
    }

    /// Ask the worker to rescan (used after an external editor session).
    pub fn request_rescan(&self) {
        self.send(Req::Rescan);
    }

    fn send(&self, req: Req) {
        let _ = self.req_tx.send(req);
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Periodic housekeeping; returns true if a redraw is needed (e.g. an
    /// expired toast must disappear even when no events arrive).
    pub fn tick(&mut self) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
            true
        } else {
            false
        }
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Input(CtEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                self.handle_key(key)
            }
            Event::Input(_) => {}
            Event::Worker(evt) => self.handle_worker(evt),
            Event::Fs(paths) => self.handle_fs(paths),
            Event::WatchError(msg) => self.toast(format!("watcher: {msg}")),
        }
    }

    // ---- worker + fs events -------------------------------------------------

    fn handle_worker(&mut self, evt: worker::Evt) {
        use worker::Evt::*;
        match evt {
            BaselineSet { label, reff } => {
                self.baseline_label = label;
                self.baseline_ref = reff;
            }
            Scanned(changes) => {
                self.changes = changes;
                self.tree.rebuild(&self.changes);
                self.sync_current_after_scan();
            }
            Diff(prepared, arrival) => self.apply_diff(*prepared, arrival),
            SnapshotSaved(name) => self.toast(format!("saved snapshot: {name}")),
            Error(msg) => {
                // The initial label only resolves on success; give it a
                // useful resting state when resolution fails.
                if self.baseline_label == "(resolving…)" {
                    self.baseline_label = "(none — press S)".into();
                }
                self.toast(format!("error: {msg}"));
            }
        }
    }

    fn sync_current_after_scan(&mut self) {
        let still = self
            .current_path
            .as_ref()
            .is_some_and(|p| self.changes.iter().any(|c| &c.path == p));
        if still {
            // Refresh the displayed diff, but leave the tree selection where
            // the user put it — rebuild() already preserved it by path.
            let p = self.current_path.clone().unwrap();
            self.request_diff(p, Arrival::Refresh);
        } else if let Some(p) = self.tree.first_file() {
            self.tree.select_path(&p);
            self.request_diff(p, Arrival::Fresh);
        } else {
            self.current_path = None;
            self.prepared = None;
            self.hunk_offsets.clear();
            self.diff_total_rows = 0;
        }
    }

    fn handle_fs(&mut self, paths: Vec<PathBuf>) {
        let mut relevant = false;
        let mut gitignore_changed = false;
        let mut git_head_changed = false;
        for p in &paths {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            if rel.starts_with(".git") {
                // A commit/checkout/reset moves HEAD; when the baseline tracks
                // HEAD it must follow. Other .git churn (objects, index) is
                // ignored so ordinary git activity doesn't re-resolve.
                if self.baseline_ref == BaselineRef::GitHead && is_git_ref_change(rel) {
                    git_head_changed = true;
                }
                continue;
            }
            if rel.starts_with(".snapshots") {
                continue;
            }
            // Edits to ignore rules change what "ignored" means.
            if rel.file_name() == Some(OsStr::new(".gitignore")) {
                gitignore_changed = true;
                relevant = true;
                continue;
            }
            // Drop events for gitignored paths (target/ churn during builds).
            // The matcher covers the root .gitignore and .git/info/exclude;
            // nested .gitignore files aren't loaded, so their matches still
            // trigger (harmless, merely unnecessary) rescans.
            let ignored = self.fs_ignore.as_ref().is_some_and(|g| {
                g.matched_path_or_any_parents(rel, p.is_dir()).is_ignore()
            });
            if !ignored {
                relevant = true;
            }
        }
        if gitignore_changed {
            self.fs_ignore = build_fs_ignore(&self.root);
        }
        // A HEAD move re-resolves the baseline and rescans, so it subsumes a
        // plain rescan for the same batch.
        if git_head_changed {
            self.send(Req::RefreshGitHead);
        } else if relevant {
            self.send(Req::Rescan);
        }
    }

    fn apply_diff(&mut self, prepared: Prepared, arrival: Arrival) {
        // Only apply if it still matches the selected file.
        if self.current_path.as_deref() != Some(prepared.path.as_path()) {
            return;
        }
        self.hunk_offsets = prepared.hunk_offsets(self.split).to_vec();
        self.diff_total_rows = prepared.row_count(self.split);

        match arrival {
            Arrival::Fresh => {
                self.current_hunk = 0;
                self.diff_scroll = 0;
            }
            Arrival::Refresh => {
                self.current_hunk = self
                    .current_hunk
                    .min(self.hunk_offsets.len().saturating_sub(1));
            }
            Arrival::JumpFirst => {
                self.current_hunk = 0;
                self.diff_scroll = self.hunk_offsets.first().copied().unwrap_or(0);
            }
            Arrival::JumpLast => {
                self.current_hunk = self.hunk_offsets.len().saturating_sub(1);
                self.diff_scroll = self.hunk_offsets.last().copied().unwrap_or(0);
            }
        }
        self.prepared = Some(prepared);
        self.clamp_scroll();
    }

    fn request_diff(&mut self, path: PathBuf, arrival: Arrival) {
        match self.changes.iter().find(|c| c.path == path).map(|c| c.kind) {
            Some(kind) => {
                self.current_path = Some(path.clone());
                self.send(Req::DiffFile { path, kind, arrival });
            }
            None => {
                self.current_path = None;
                self.prepared = None;
            }
        }
    }

    // ---- key handling -------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.overlay {
            Overlay::Help => self.key_help(key),
            Overlay::Prompt => self.key_prompt(key),
            Overlay::Picker => self.key_picker(key),
            Overlay::None => self.key_normal(key),
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                self.help_scroll = 0;
            }
            KeyCode::Char('S') => self.open_prompt(),
            KeyCode::Char('s') => self.open_picker(),
            KeyCode::Char('r') => {
                self.send(Req::Rescan);
                self.toast("refreshing…");
            }
            KeyCode::Char('w') => self.toggle_split(),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Diff,
                    Focus::Diff => Focus::Tree,
                }
            }
            KeyCode::Char('<') => self.tree_width = self.tree_width.saturating_sub(3).max(10),
            KeyCode::Char('>') => self.tree_width = (self.tree_width + 3).min(80),
            KeyCode::Char('n') => self.next_change(),
            KeyCode::Char('N') => self.prev_change(),
            KeyCode::Char(']') => self.goto_adjacent_file(true),
            KeyCode::Char('[') => self.goto_adjacent_file(false),
            _ => match self.focus {
                Focus::Tree => self.key_tree(key),
                Focus::Diff => self.key_diff(key),
            },
        }
    }

    fn key_tree(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(p) = self.tree.move_sel(1) {
                    self.request_diff(p, Arrival::Fresh);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(p) = self.tree.move_sel(-1) {
                    self.request_diff(p, Arrival::Fresh);
                }
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                match self.tree.selected_row().map(|r| (r.is_dir(), r.path.clone())) {
                    Some((true, _)) => self.collapse_selected(),
                    // Enter opens the highlighted file in $EDITOR (the diff
                    // already tracks it via j/k); l/→ show the diff and focus
                    // the diff pane instead.
                    Some((false, _)) if key.code == KeyCode::Enter => self.launch_editor(),
                    Some((false, p)) => {
                        self.request_diff(p, Arrival::Fresh);
                        self.focus = Focus::Diff;
                    }
                    None => {}
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.collapse_selected(),
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(p) = self.tree.move_sel(isize::MIN / 2) {
                    self.request_diff(p, Arrival::Fresh);
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(p) = self.tree.move_sel(isize::MAX / 2) {
                    self.request_diff(p, Arrival::Fresh);
                }
            }
            _ => {}
        }
    }

    /// Toggle collapse on the selected tree directory and rebuild if needed.
    fn collapse_selected(&mut self) {
        if self.tree.toggle_collapse() {
            self.tree.rebuild(&self.changes);
        }
    }

    fn key_diff(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_diff(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_diff(-1),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_diff(self.diff_area_height as isize / 2)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_diff(-(self.diff_area_height as isize / 2))
            }
            KeyCode::PageDown => self.scroll_diff(self.diff_area_height as isize),
            KeyCode::PageUp => self.scroll_diff(-(self.diff_area_height as isize)),
            KeyCode::Char('g') | KeyCode::Home => {
                self.diff_scroll = 0;
                self.clamp_scroll();
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.diff_scroll = self.diff_total_rows;
                self.clamp_scroll();
            }
            KeyCode::Enter => self.launch_editor(),
            KeyCode::Esc => self.focus = Focus::Tree,
            _ => {}
        }
    }

    // ---- diff navigation ----------------------------------------------------

    fn next_change(&mut self) {
        if self.hunk_offsets.is_empty() {
            self.goto_adjacent_file(true);
            return;
        }
        if self.current_hunk + 1 < self.hunk_offsets.len() {
            self.current_hunk += 1;
            self.diff_scroll = self.hunk_offsets[self.current_hunk];
            self.clamp_scroll();
        } else {
            self.cross_file(true);
        }
    }

    fn prev_change(&mut self) {
        if !self.hunk_offsets.is_empty() && self.current_hunk > 0 {
            self.current_hunk -= 1;
            self.diff_scroll = self.hunk_offsets[self.current_hunk];
            self.clamp_scroll();
        } else {
            self.cross_file(false);
        }
    }

    fn cross_file(&mut self, forward: bool) {
        let Some(next) = self.adjacent_file(forward) else {
            self.toast(if forward {
                "no more changes"
            } else {
                "at first change"
            });
            return;
        };
        self.tree.reveal(&next);
        self.tree.rebuild(&self.changes);
        self.tree.select_path(&next);
        let arrival = if forward {
            Arrival::JumpFirst
        } else {
            Arrival::JumpLast
        };
        self.request_diff(next, arrival);
        self.focus = Focus::Diff;
    }

    fn goto_adjacent_file(&mut self, forward: bool) {
        if let Some(next) = self.adjacent_file(forward) {
            self.tree.reveal(&next);
            self.tree.rebuild(&self.changes);
            self.tree.select_path(&next);
            self.request_diff(next, Arrival::Fresh);
        }
    }

    fn adjacent_file(&self, forward: bool) -> Option<PathBuf> {
        if self.changes.is_empty() {
            return None;
        }
        let idx = self
            .current_path
            .as_ref()
            .and_then(|p| self.changes.iter().position(|c| &c.path == p));
        let next = match idx {
            Some(i) if forward => i.checked_add(1).filter(|&n| n < self.changes.len()),
            Some(i) => i.checked_sub(1),
            None => Some(0),
        };
        next.map(|i| self.changes[i].path.clone())
    }

    fn scroll_diff(&mut self, delta: isize) {
        let next = self.diff_scroll as isize + delta;
        self.diff_scroll = next.max(0) as usize;
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let max = self.diff_total_rows.saturating_sub(self.diff_area_height);
        self.diff_scroll = self.diff_scroll.min(max);
    }

    fn toggle_split(&mut self) {
        self.split = !self.split;
        if let Some(p) = &self.prepared {
            self.hunk_offsets = p.hunk_offsets(self.split).to_vec();
            self.diff_total_rows = p.row_count(self.split);
            self.current_hunk = self
                .current_hunk
                .min(self.hunk_offsets.len().saturating_sub(1));
        }
        self.clamp_scroll();
        self.toast(if self.split { "split view" } else { "unified view" });
    }

    // ---- snapshot prompt ----------------------------------------------------

    fn open_prompt(&mut self) {
        self.prompt_input = snapshot::default_name();
        self.overlay = Overlay::Prompt;
    }

    fn key_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let name = self.prompt_input.trim().to_string();
                self.overlay = Overlay::None;
                if name.is_empty() {
                    self.toast("snapshot name cannot be empty");
                } else {
                    self.send(Req::SaveSnapshot { name });
                }
            }
            KeyCode::Backspace => {
                self.prompt_input.pop();
            }
            KeyCode::Char(c) => self.prompt_input.push(c),
            _ => {}
        }
    }

    // ---- baseline picker ----------------------------------------------------

    /// Build the picker rows from the current snapshots plus a `HEAD` entry.
    fn build_picker_items(&self) -> Vec<PickerItem> {
        let mut items = Vec::new();
        if let Ok(snaps) = snapshot::list(&self.root) {
            for s in snaps {
                let label = if s.is_latest {
                    format!("snapshot: {}  (latest)", s.name)
                } else {
                    format!("snapshot: {}", s.name)
                };
                items.push(PickerItem {
                    label,
                    reff: BaselineRef::Snapshot(s.name),
                });
            }
        }
        if self.repo_is_git {
            items.push(PickerItem {
                label: "git: HEAD".into(),
                reff: BaselineRef::GitHead,
            });
        }
        items
    }

    fn selected_picker_item(&self) -> Option<&PickerItem> {
        self.picker_state
            .selected()
            .and_then(|i| self.picker_items.get(i))
    }

    fn open_picker(&mut self) {
        let items = self.build_picker_items();
        if items.is_empty() {
            self.toast("no snapshots yet — press S to take one");
            return;
        }
        self.picker_items = items;
        self.picker_state.select(Some(0));
        self.picker_pending_delete = None;
        self.overlay = Overlay::Picker;
    }

    /// The snapshot the baseline currently resolves to (the explicit one, or
    /// the `latest` target while tracking it), or `None` when diffing HEAD.
    fn active_snapshot_name(&self) -> Option<String> {
        match &self.baseline_ref {
            BaselineRef::Snapshot(n) => Some(n.clone()),
            BaselineRef::Latest => snapshot::latest_name(&self.root),
            BaselineRef::GitHead => None,
        }
    }

    /// Handle `d` in the picker: arm a delete on the first press, perform it on
    /// the second. The snapshot in use as the baseline is protected.
    fn picker_delete(&mut self) {
        let name = match self.selected_picker_item().map(|it| &it.reff) {
            Some(BaselineRef::Snapshot(n)) => n.clone(),
            _ => {
                self.picker_pending_delete = None;
                self.toast("only snapshots can be deleted");
                return;
            }
        };
        if self.active_snapshot_name().as_deref() == Some(name.as_str()) {
            self.picker_pending_delete = None;
            self.toast("can't delete the active baseline — switch first");
            return;
        }
        if self.picker_pending_delete.as_deref() == Some(name.as_str()) {
            self.picker_pending_delete = None;
            match snapshot::delete(&self.root, &name) {
                Ok(()) => {
                    self.toast(format!("deleted snapshot: {name}"));
                    self.refresh_picker_after_delete();
                }
                Err(e) => self.toast(format!("delete failed: {e:#}")),
            }
        } else {
            // First press: arm; the picker shows a confirmation prompt.
            self.picker_pending_delete = Some(name);
        }
    }

    /// Rebuild the picker after a deletion, closing it if nothing is left.
    fn refresh_picker_after_delete(&mut self) {
        self.picker_items = self.build_picker_items();
        if self.picker_items.is_empty() {
            self.picker_state.select(None);
            self.overlay = Overlay::None;
            return;
        }
        let sel = self
            .picker_state
            .selected()
            .unwrap_or(0)
            .min(self.picker_items.len() - 1);
        self.picker_state.select(Some(sel));
    }

    fn key_picker(&mut self, key: KeyEvent) {
        // Any keystroke other than a repeated `d` cancels a pending delete.
        if key.code != KeyCode::Char('d') {
            self.picker_pending_delete = None;
        }
        let len = self.picker_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => {
                let i = (self.picker_state.selected().unwrap_or(0) + 1).min(len - 1);
                self.picker_state.select(Some(i));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let i = self.picker_state.selected().unwrap_or(0).saturating_sub(1);
                self.picker_state.select(Some(i));
            }
            KeyCode::Char('d') => self.picker_delete(),
            KeyCode::Enter => {
                if let Some(item) = self.selected_picker_item() {
                    let reff = item.reff.clone();
                    // Clear all per-file view state so the status bar and
                    // n/N navigation can't act on ghosts of the old baseline
                    // while the new scan is in flight.
                    self.current_path = None;
                    self.prepared = None;
                    self.hunk_offsets.clear();
                    self.diff_total_rows = 0;
                    self.current_hunk = 0;
                    self.diff_scroll = 0;
                    self.send(Req::SetBaseline(reff));
                }
                self.overlay = Overlay::None;
            }
            _ => {}
        }
    }

    // ---- help ---------------------------------------------------------------

    fn key_help(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
            _ => self.overlay = Overlay::None,
        }
    }

    // ---- editor -------------------------------------------------------------

    fn launch_editor(&mut self) {
        let Some(path) = &self.current_path else {
            self.toast("no file selected");
            return;
        };
        let line = self
            .prepared
            .as_ref()
            .and_then(|p| p.hunk_lineno(self.current_hunk))
            .unwrap_or(1);
        self.launch_editor = Some((self.root.join(path), line));
    }

    /// Current hunk index, used by the renderer to indicate position.
    pub fn current_hunk(&self) -> usize {
        self.current_hunk
    }

    pub fn hunk_count(&self) -> usize {
        self.hunk_offsets.len()
    }
}

/// Build the gitignore matcher used to filter fs events. Only the root
/// `.gitignore` and `.git/info/exclude` are loaded; that is enough to drop
/// the high-volume churn (build dirs) without risking false skips for the
/// common layout.
fn build_fs_ignore(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git").join("info").join("exclude"));
    builder.build().ok()
}

/// True for `.git` paths whose write means HEAD may have moved: the `HEAD`
/// file, any ref, `packed-refs`, or a reflog. Object and index writes are
/// excluded so routine git activity doesn't re-resolve the HEAD baseline.
fn is_git_ref_change(rel: &Path) -> bool {
    let Ok(tail) = rel.strip_prefix(".git") else {
        return false;
    };
    tail == Path::new("HEAD")
        || tail == Path::new("packed-refs")
        || tail.starts_with("refs")
        || tail.starts_with("logs")
}

#[cfg(test)]
mod tests {
    use super::is_git_ref_change;
    use std::path::Path;

    #[test]
    fn head_moves_are_detected() {
        for p in [
            ".git/HEAD",
            ".git/packed-refs",
            ".git/refs/heads/main",
            ".git/refs/tags/v1",
            ".git/logs/HEAD",
            ".git/logs/refs/heads/main",
        ] {
            assert!(is_git_ref_change(Path::new(p)), "{p} should count");
        }
    }

    #[test]
    fn other_git_and_tree_writes_are_ignored() {
        for p in [
            ".git",
            ".git/index",
            ".git/COMMIT_EDITMSG",
            ".git/objects/ab/cdef",
            ".gitignore",
            "src/main.rs",
        ] {
            assert!(!is_git_ref_change(Path::new(p)), "{p} should not count");
        }
    }
}

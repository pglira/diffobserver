//! The left-pane file tree: a collapsible hierarchy of changed files.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState};

use crate::core::model::{ChangeKind, DiffKind, FileChange};
use crate::ui::theme::kind_color;

/// A transient node used only while (re)building the tree.
struct Node {
    children: BTreeMap<String, Node>,
    change: Option<FileChange>,
    added: usize,
    removed: usize,
}

impl Node {
    fn dir() -> Node {
        Node {
            children: BTreeMap::new(),
            change: None,
            added: 0,
            removed: 0,
        }
    }
}

/// A flattened, currently-visible tree row.
pub struct Row {
    depth: usize,
    is_dir: bool,
    name: String,
    pub path: PathBuf,
    expanded: bool,
    kind: Option<ChangeKind>,
    diff_kind: Option<DiffKind>,
    added: usize,
    removed: usize,
}

impl Row {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
}

pub struct Tree {
    rows: Vec<Row>,
    collapsed: HashSet<PathBuf>,
    pub state: ListState,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Self {
        Tree {
            rows: Vec::new(),
            collapsed: HashSet::new(),
            state: ListState::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn selected(&self) -> usize {
        self.state.selected().unwrap_or(0)
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected())
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_row().map(|r| r.path.clone())
    }

    /// Rebuild the flattened rows from the change list, preserving the selected
    /// path and collapse state.
    pub fn rebuild(&mut self, changes: &[FileChange]) {
        let prev = self.selected_path();

        let mut root = Node::dir();
        for c in changes {
            insert(&mut root, c);
        }
        let mut rows = Vec::new();
        flatten(&root, &PathBuf::new(), 0, &self.collapsed, &mut rows);
        self.rows = rows;

        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let idx = prev
            .and_then(|p| self.rows.iter().position(|r| r.path == p))
            .unwrap_or_else(|| self.selected().min(self.rows.len() - 1));
        self.state.select(Some(idx));
    }

    /// Move the selection by `delta`, clamped. Returns the path if the new
    /// selection is a file.
    pub fn move_sel(&mut self, delta: isize) -> Option<PathBuf> {
        if self.rows.is_empty() {
            return None;
        }
        let cur = self.selected() as isize;
        let next = (cur + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.state.select(Some(next));
        self.file_at(next)
    }

    /// Toggle collapse on the selected directory. Returns true if a rebuild is
    /// needed (i.e. the selection was a directory).
    pub fn toggle_collapse(&mut self) -> bool {
        match self.selected_row() {
            Some(r) if r.is_dir => {
                let path = r.path.clone();
                if !self.collapsed.remove(&path) {
                    self.collapsed.insert(path);
                }
                true
            }
            _ => false,
        }
    }

    /// Ensure all ancestor directories of `path` are expanded.
    pub fn reveal(&mut self, path: &Path) {
        let mut acc = PathBuf::new();
        for comp in path.components() {
            acc.push(comp);
            self.collapsed.remove(&acc);
        }
    }

    /// Select the row matching `path`, if present.
    pub fn select_path(&mut self, path: &Path) {
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.state.select(Some(i));
        }
    }

    pub fn first_file(&self) -> Option<PathBuf> {
        self.rows.iter().find(|r| !r.is_dir).map(|r| r.path.clone())
    }

    fn file_at(&self, i: usize) -> Option<PathBuf> {
        self.rows.get(i).filter(|r| !r.is_dir).map(|r| r.path.clone())
    }

    pub fn items(&self) -> Vec<ListItem<'static>> {
        self.rows.iter().map(render_row).collect()
    }
}

fn insert(root: &mut Node, c: &FileChange) {
    let comps: Vec<String> = c
        .path
        .components()
        .map(|p| p.as_os_str().to_string_lossy().into_owned())
        .collect();
    root.added += c.added;
    root.removed += c.removed;
    let mut node = root;
    for (i, comp) in comps.iter().enumerate() {
        node = node.children.entry(comp.clone()).or_insert_with(Node::dir);
        node.added += c.added;
        node.removed += c.removed;
        if i == comps.len() - 1 {
            node.change = Some(c.clone());
        }
    }
}

fn flatten(node: &Node, path: &Path, depth: usize, collapsed: &HashSet<PathBuf>, out: &mut Vec<Row>) {
    // Directories first (alphabetical), then files (alphabetical).
    let dirs = node.children.iter().filter(|(_, c)| c.change.is_none());
    let files = node.children.iter().filter(|(_, c)| c.change.is_some());

    for (name, child) in dirs {
        let cpath = path.join(name);
        let expanded = !collapsed.contains(&cpath);
        out.push(Row {
            depth,
            is_dir: true,
            name: name.clone(),
            path: cpath.clone(),
            expanded,
            kind: None,
            diff_kind: None,
            added: child.added,
            removed: child.removed,
        });
        if expanded {
            flatten(child, &cpath, depth + 1, collapsed, out);
        }
    }
    for (name, child) in files {
        let c = child.change.as_ref().unwrap();
        out.push(Row {
            depth,
            is_dir: false,
            name: name.clone(),
            path: path.join(name),
            expanded: false,
            kind: Some(c.kind),
            diff_kind: Some(c.diff_kind),
            added: c.added,
            removed: c.removed,
        });
    }
}

fn render_row(r: &Row) -> ListItem<'static> {
    let mut spans = vec![Span::raw("  ".repeat(r.depth))];
    if r.is_dir {
        let arrow = if r.expanded { "▾ " } else { "▸ " };
        spans.push(Span::styled(
            format!("{arrow}{}/", r.name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        if r.added > 0 || r.removed > 0 {
            spans.push(Span::styled(
                format!("  +{} -{}", r.added, r.removed),
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else {
        let kind = r.kind.unwrap();
        spans.push(Span::styled(
            format!("{} ", kind.glyph()),
            Style::default().fg(kind_color(kind)).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(r.name.clone()));
        let meta = match r.diff_kind {
            Some(DiffKind::Binary) => "  (bin)".to_string(),
            Some(DiffKind::TooLarge) => "  (large)".to_string(),
            Some(DiffKind::Unreadable) => "  (unreadable)".to_string(),
            _ => format!("  +{} -{}", r.added, r.removed),
        };
        spans.push(Span::styled(meta, Style::default().fg(Color::DarkGray)));
    }
    ListItem::new(Line::from(spans))
}

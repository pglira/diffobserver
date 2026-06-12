//! Core data types shared across the scan, diff, and UI layers.

use std::path::PathBuf;

/// How a file changed relative to the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    /// Single-character glyph shown in the tree.
    pub fn glyph(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        }
    }
}

/// Whether a textual diff could be produced for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// A normal line-based diff is available.
    Text,
    /// File looks binary (NUL byte found); no content diff.
    Binary,
    /// File exceeded the size cap; diff skipped.
    TooLarge,
}

/// A summary entry for one changed file, cheap to compute for every change.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path relative to the repo root.
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub diff_kind: DiffKind,
    /// Inserted line count (0 for binary/too-large).
    pub added: usize,
    /// Removed line count (0 for binary/too-large).
    pub removed: usize,
}

/// Tag for a single rendered diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTag {
    Context,
    Insert,
    Delete,
}

/// A run of text within a diff line, with word-level emphasis when the run is
/// part of the changed span.
#[derive(Debug, Clone)]
pub struct Segment {
    pub emph: bool,
    pub text: String,
}

/// One line in a unified (or split) diff.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: LineTag,
    /// 1-based line number in the old (baseline) file, if present.
    pub old_lineno: Option<usize>,
    /// 1-based line number in the new (live) file, if present.
    pub new_lineno: Option<usize>,
    /// Word-level segments making up the line text (without trailing newline).
    pub segments: Vec<Segment>,
}

impl DiffLine {
    /// Full line text, segments concatenated.
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A contiguous group of changed lines plus surrounding context.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The `@@ -a,b +c,d @@` header text.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

/// The full diff of a single file, computed lazily when the file is viewed.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub diff_kind: DiffKind,
    pub hunks: Vec<Hunk>,
    /// Full baseline text (for syntax highlighting); None when absent/binary.
    pub old_text: Option<String>,
    /// Full live text (for syntax highlighting); None when absent/binary.
    pub new_text: Option<String>,
}

impl FileDiff {
    /// A placeholder diff carrying only a status (binary, too large, or empty).
    pub fn placeholder(path: PathBuf, kind: ChangeKind, diff_kind: DiffKind) -> Self {
        FileDiff {
            path,
            kind,
            diff_kind,
            hunks: Vec::new(),
            old_text: None,
            new_text: None,
        }
    }

    /// Total number of change (non-context) lines across all hunks.
    pub fn change_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

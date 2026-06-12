//! Line- and word-level diffing via the `similar` crate.

use similar::{ChangeTag, TextDiff};

use crate::core::model::{DiffKind, DiffLine, Hunk, LineTag, Segment};

/// Number of context lines kept around each hunk.
const CONTEXT: usize = 3;
/// How many leading bytes to scan when sniffing for binary content.
const BINARY_SNIFF_LEN: usize = 8192;

/// A file is treated as binary if a NUL byte appears in its first chunk.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_LEN).any(|&b| b == 0)
}

/// Make text safe for terminal rendering. Ratatui must never see control
/// characters: a literal tab moves the real cursor to the next tab stop while
/// the render buffer assumes one cell, desynchronizing the two and leaving
/// ghost artifacts on screen. Tabs become spaces; other control characters
/// (except newline) are dropped, which also normalizes CRLF to LF.
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\t' => out.push_str("    "),
            '\n' => out.push(ch),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Decide how a pair of (optional) contents should be diffed.
pub fn classify(old: Option<&[u8]>, new: Option<&[u8]>, size_cap: u64) -> DiffKind {
    let over_cap = |b: Option<&[u8]>| b.is_some_and(|b| b.len() as u64 > size_cap);
    let binary = |b: Option<&[u8]>| b.is_some_and(is_binary);
    if over_cap(old) || over_cap(new) {
        DiffKind::TooLarge
    } else if binary(old) || binary(new) {
        DiffKind::Binary
    } else {
        DiffKind::Text
    }
}

/// Cheap insert/remove line counts for the tree summary.
pub fn count_lines(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// Strip a single trailing line terminator from a segment value.
fn trim_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .unwrap_or(s)
        .strip_suffix('\r')
        .unwrap_or_else(|| s.strip_suffix('\n').unwrap_or(s))
}

/// Build unified hunks (with word-level segments and line numbers) for a pair
/// of decoded texts.
pub fn compute_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();

    for group in diff.grouped_ops(CONTEXT) {
        let Some(first) = group.first() else { continue };
        let last = group.last().unwrap();
        let old_start = first.old_range().start;
        let old_end = last.old_range().end;
        let new_start = first.new_range().start;
        let new_end = last.new_range().end;

        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let (tag, old_no, new_no) = match change.tag() {
                    ChangeTag::Equal => {
                        (LineTag::Context, change.old_index(), change.new_index())
                    }
                    ChangeTag::Delete => (LineTag::Delete, change.old_index(), None),
                    ChangeTag::Insert => (LineTag::Insert, None, change.new_index()),
                };

                let mut segments = Vec::new();
                for (emph, value) in change.iter_strings_lossy() {
                    let text = trim_eol(&value);
                    if text.is_empty() {
                        continue;
                    }
                    segments.push(Segment {
                        emph,
                        text: text.to_string(),
                    });
                }

                lines.push(DiffLine {
                    tag,
                    old_lineno: old_no.map(|i| i + 1),
                    new_lineno: new_no.map(|i| i + 1),
                    segments,
                });
            }
        }

        hunks.push(Hunk {
            old_start: old_start + 1,
            old_len: old_end - old_start,
            new_start: new_start + 1,
            new_len: new_end - new_start,
            lines,
        });
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_sniff() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
    }

    #[test]
    fn classify_precedence() {
        assert_eq!(classify(Some(b"a"), Some(&[0u8; 4]), 1024), DiffKind::Binary);
        assert_eq!(classify(Some(b"a"), Some(b"b"), 0), DiffKind::TooLarge);
        assert_eq!(classify(Some(b"a"), Some(b"b"), 1024), DiffKind::Text);
    }

    #[test]
    fn counts_and_hunks() {
        let old = "one\ntwo\nthree\n";
        let new = "one\nTWO\nthree\nfour\n";
        let (added, removed) = count_lines(old, new);
        assert_eq!((added, removed), (2, 1));

        let hunks = compute_hunks(old, new);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        // The "two" -> "TWO" line should carry an emphasized segment.
        let has_emph = h
            .lines
            .iter()
            .flat_map(|l| &l.segments)
            .any(|s| s.emph);
        assert!(has_emph, "expected a word-level emphasized segment");
    }
}

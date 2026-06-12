//! Scan the working tree against a baseline to produce the changed-file set,
//! and compute the full diff for a single file on demand.
//!
//! Robustness rules: a scan must survive anything a single file can do —
//! vanish mid-scan, be unreadable, be huge. Per-file problems degrade to a
//! per-file status; they never abort the scan.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::core::baseline::BaselineSource;
use crate::core::diff;
use crate::core::excludes;
use crate::core::model::{ChangeKind, DiffKind, FileChange, FileDiff};

/// Tunables for scanning and diffing.
#[derive(Debug, Clone, Copy)]
pub struct ScanConfig {
    /// Files larger than this (bytes) are listed but not content-diffed.
    pub size_cap: u64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            size_cap: 2 * 1024 * 1024,
        }
    }
}

fn read_live(root: &Path, rel: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(root.join(rel)) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Compare the live tree at `root` to `base`, returning one entry per change.
pub fn scan(
    root: &Path,
    base: &dyn BaselineSource,
    cfg: &ScanConfig,
) -> Result<Vec<FileChange>> {
    let live: BTreeSet<PathBuf> = excludes::walk_live(root)?.into_iter().collect();
    let base_paths: BTreeSet<PathBuf> = base.paths()?.into_iter().collect();

    let mut changes = Vec::new();
    for rel in live.union(&base_paths) {
        let kind = match (base_paths.contains(rel), live.contains(rel)) {
            (false, true) => ChangeKind::Added,
            (true, false) => ChangeKind::Deleted,
            (true, true) => ChangeKind::Modified,
            (false, false) => continue,
        };
        if let Some(change) = classify(root, base, rel, kind, cfg) {
            changes.push(change);
        }
    }
    Ok(changes)
}

/// Build a `FileChange` summary, returning `None` if the file is unchanged
/// (or vanished). Per-file IO errors degrade to `DiffKind::Unreadable`.
fn classify(
    root: &Path,
    base: &dyn BaselineSource,
    rel: &Path,
    kind: ChangeKind,
    cfg: &ScanConfig,
) -> Option<FileChange> {
    let live_path = root.join(rel);

    // Cheap metadata quick-check (rsync semantics): skip content reads when
    // the baseline can prove the file is unchanged.
    if kind == ChangeKind::Modified && base.quick_unchanged(&live_path, rel) {
        return None;
    }

    // Size short-circuit BEFORE reading, so a multi-GB artifact is never
    // slurped into memory just to be declared too large.
    let live_len = (kind != ChangeKind::Deleted)
        .then(|| std::fs::metadata(&live_path).ok().map(|m| m.len()))
        .flatten();
    let base_len = (kind != ChangeKind::Added)
        .then(|| base.size(rel))
        .flatten();
    if live_len.is_some_and(|l| l > cfg.size_cap) || base_len.is_some_and(|l| l > cfg.size_cap) {
        return Some(FileChange {
            path: rel.to_path_buf(),
            kind,
            diff_kind: DiffKind::TooLarge,
            added: 0,
            removed: 0,
        });
    }

    let unreadable = || {
        Some(FileChange {
            path: rel.to_path_buf(),
            kind,
            diff_kind: DiffKind::Unreadable,
            added: 0,
            removed: 0,
        })
    };
    let old = if kind == ChangeKind::Added {
        None
    } else {
        match base.content(rel) {
            Ok(b) => b,
            Err(_) => return unreadable(),
        }
    };
    let new = if kind == ChangeKind::Deleted {
        None
    } else {
        match read_live(root, rel) {
            Ok(b) => b,
            Err(_) => return unreadable(),
        }
    };

    // Files that vanished between the walk and the read.
    let kind = match (kind, &old, &new) {
        (ChangeKind::Added, _, None) => return None,
        (ChangeKind::Deleted, None, _) => return None,
        (ChangeKind::Modified, _, None) => ChangeKind::Deleted,
        (ChangeKind::Modified, None, _) => ChangeKind::Added,
        (k, _, _) => k,
    };

    if kind == ChangeKind::Modified && old == new {
        return None;
    }

    let diff_kind = diff::classify(old.as_deref(), new.as_deref(), cfg.size_cap);
    let (added, removed) = if diff_kind == DiffKind::Text {
        // Sanitize before counting so the tree counts agree with the diff
        // pane (which renders sanitized text). A CRLF-only or control-char-
        // only change is not a textual change at all.
        let old_text = diff::sanitize_text(&String::from_utf8_lossy(old.as_deref().unwrap_or(b"")));
        let new_text = diff::sanitize_text(&String::from_utf8_lossy(new.as_deref().unwrap_or(b"")));
        if kind == ChangeKind::Modified && old_text == new_text {
            return None;
        }
        diff::count_lines(&old_text, &new_text)
    } else {
        (0, 0)
    };

    Some(FileChange {
        path: rel.to_path_buf(),
        kind,
        diff_kind,
        added,
        removed,
    })
}

/// Compute the full diff (hunks + texts) for a single file.
pub fn diff_file(
    root: &Path,
    base: &dyn BaselineSource,
    rel: &Path,
    kind: ChangeKind,
    cfg: &ScanConfig,
) -> Result<FileDiff> {
    let live_path = root.join(rel);

    // Same metadata short-circuit as the scan: never read past the cap.
    let live_len = (kind != ChangeKind::Deleted)
        .then(|| std::fs::metadata(&live_path).ok().map(|m| m.len()))
        .flatten();
    let base_len = (kind != ChangeKind::Added)
        .then(|| base.size(rel))
        .flatten();
    if live_len.is_some_and(|l| l > cfg.size_cap) || base_len.is_some_and(|l| l > cfg.size_cap) {
        return Ok(FileDiff::placeholder(
            rel.to_path_buf(),
            kind,
            DiffKind::TooLarge,
        ));
    }

    let old = if kind == ChangeKind::Added {
        None
    } else {
        match base.content(rel) {
            Ok(b) => b,
            Err(_) => {
                return Ok(FileDiff::placeholder(
                    rel.to_path_buf(),
                    kind,
                    DiffKind::Unreadable,
                ))
            }
        }
    };
    let new = if kind == ChangeKind::Deleted {
        None
    } else {
        match read_live(root, rel) {
            Ok(b) => b,
            Err(_) => {
                return Ok(FileDiff::placeholder(
                    rel.to_path_buf(),
                    kind,
                    DiffKind::Unreadable,
                ))
            }
        }
    };

    let diff_kind = diff::classify(old.as_deref(), new.as_deref(), cfg.size_cap);
    if diff_kind != DiffKind::Text {
        return Ok(FileDiff::placeholder(rel.to_path_buf(), kind, diff_kind));
    }

    let old_text = old
        .as_deref()
        .map(|b| diff::sanitize_text(&String::from_utf8_lossy(b)));
    let new_text = new
        .as_deref()
        .map(|b| diff::sanitize_text(&String::from_utf8_lossy(b)));
    let hunks = diff::compute_hunks(
        old_text.as_deref().unwrap_or_default(),
        new_text.as_deref().unwrap_or_default(),
    );

    Ok(FileDiff {
        path: rel.to_path_buf(),
        kind,
        diff_kind,
        hunks,
        old_text,
        new_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// In-memory baseline for tests.
    struct MapBaseline(HashMap<PathBuf, Vec<u8>>);
    impl BaselineSource for MapBaseline {
        fn paths(&self) -> Result<Vec<PathBuf>> {
            Ok(self.0.keys().cloned().collect())
        }
        fn content(&self, rel: &Path) -> Result<Option<Vec<u8>>> {
            Ok(self.0.get(rel).cloned())
        }
        fn label(&self) -> &str {
            "test"
        }
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "diffobserver-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn detects_added_modified_deleted_and_skips_unchanged() {
        let root = temp_root();
        write(&root, "a.txt", "hello\nWORLD\n"); // modified
        write(&root, "b.txt", "brand new\n"); // added
        write(&root, "c.txt", "same\n"); // unchanged

        let mut base = HashMap::new();
        base.insert(PathBuf::from("a.txt"), b"hello\nworld\n".to_vec());
        base.insert(PathBuf::from("c.txt"), b"same\n".to_vec());
        base.insert(PathBuf::from("d.txt"), b"gone\n".to_vec()); // deleted
        let base = MapBaseline(base);

        let changes = scan(&root, &base, &ScanConfig::default()).unwrap();
        let by: HashMap<_, _> = changes
            .iter()
            .map(|c| (c.path.to_string_lossy().to_string(), c.kind))
            .collect();

        assert_eq!(by.get("a.txt"), Some(&ChangeKind::Modified));
        assert_eq!(by.get("b.txt"), Some(&ChangeKind::Added));
        assert_eq!(by.get("d.txt"), Some(&ChangeKind::Deleted));
        assert!(!by.contains_key("c.txt"), "unchanged file must be skipped");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_file_degrades_instead_of_killing_the_scan() {
        let root = temp_root();
        write(&root, "good.txt", "fine\n");
        write(&root, "secret.txt", "locked\n");
        std::fs::set_permissions(
            root.join("secret.txt"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let base = MapBaseline(HashMap::new());
        let changes = scan(&root, &base, &ScanConfig::default()).unwrap();

        let secret = changes
            .iter()
            .find(|c| c.path == Path::new("secret.txt"))
            .expect("unreadable file still listed");
        assert_eq!(secret.diff_kind, DiffKind::Unreadable);
        assert!(
            changes.iter().any(|c| c.path == Path::new("good.txt")),
            "other files must still be scanned"
        );

        std::fs::set_permissions(
            root.join("secret.txt"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn crlf_only_change_is_not_a_change() {
        let root = temp_root();
        write(&root, "a.txt", "one\ntwo\n"); // LF live

        let mut base = HashMap::new();
        base.insert(PathBuf::from("a.txt"), b"one\r\ntwo\r\n".to_vec()); // CRLF baseline
        let base = MapBaseline(base);

        let changes = scan(&root, &base, &ScanConfig::default()).unwrap();
        assert!(
            changes.is_empty(),
            "CRLF-only difference must not be reported: {changes:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn size_cap_short_circuits_without_reading() {
        let root = temp_root();
        write(&root, "big.bin", &"x".repeat(4096));
        let base = MapBaseline(HashMap::new());
        let cfg = ScanConfig { size_cap: 1024 };

        let changes = scan(&root, &base, &cfg).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].diff_kind, DiffKind::TooLarge);

        let fd = diff_file(&root, &base, Path::new("big.bin"), ChangeKind::Added, &cfg).unwrap();
        assert_eq!(fd.diff_kind, DiffKind::TooLarge);
        assert!(fd.hunks.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn diff_file_sanitizes_tabs_and_control_chars() {
        let root = temp_root();
        // Tab-indented JSON with CRLF line endings, like a VS Code tasks.json.
        write(&root, "tasks.json", "{\r\n\t\"type\": \"shell\",\r\n\t\"group\": {\r\n\t\t\"kind\": \"build\"\r\n\t}\r\n}\r\n");
        let base = MapBaseline(HashMap::new());

        let fd = diff_file(
            &root,
            &base,
            Path::new("tasks.json"),
            ChangeKind::Added,
            &ScanConfig::default(),
        )
        .unwrap();

        let rendered: String = fd
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains('\t'), "tabs must be expanded: {rendered:?}");
        assert!(!rendered.contains('\r'), "CR must be stripped: {rendered:?}");
        assert!(rendered.contains("        \"kind\""), "tabs become spaces");
        // The highlight source texts must be sanitized identically.
        assert!(!fd.new_text.as_ref().unwrap().contains('\t'));
        assert!(!fd.new_text.as_ref().unwrap().contains('\r'));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn diff_file_added_is_all_inserts() {
        let root = temp_root();
        write(&root, "new.txt", "line1\nline2\n");
        let base = MapBaseline(HashMap::new());

        let fd = diff_file(
            &root,
            &base,
            Path::new("new.txt"),
            ChangeKind::Added,
            &ScanConfig::default(),
        )
        .unwrap();
        assert_eq!(fd.diff_kind, DiffKind::Text);
        assert_eq!(fd.hunks.len(), 1);
        let inserts = fd.hunks[0]
            .lines
            .iter()
            .filter(|l| l.tag == crate::core::model::LineTag::Insert)
            .count();
        assert_eq!(inserts, 2);

        std::fs::remove_dir_all(&root).ok();
    }
}

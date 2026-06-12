//! Scan the working tree against a baseline to produce the changed-file set,
//! and compute the full diff for a single file on demand.

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

fn to_text(bytes: Option<&[u8]>) -> String {
    bytes
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
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
        if let Some(change) = classify(root, base, rel, kind, cfg)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

/// Re-scan a specific set of paths (used for incremental updates). Returns the
/// change entry for each path, or `None` where the path is no longer changed.
pub fn scan_paths(
    root: &Path,
    base: &dyn BaselineSource,
    rels: &[PathBuf],
    cfg: &ScanConfig,
) -> Result<Vec<(PathBuf, Option<FileChange>)>> {
    let mut out = Vec::with_capacity(rels.len());
    for rel in rels {
        let in_live = root.join(rel).exists();
        let in_base = base.content(rel)?.is_some();
        let kind = match (in_base, in_live) {
            (false, true) => Some(ChangeKind::Added),
            (true, false) => Some(ChangeKind::Deleted),
            (true, true) => Some(ChangeKind::Modified),
            (false, false) => None,
        };
        let change = match kind {
            Some(k) => classify(root, base, rel, k, cfg)?,
            None => None,
        };
        out.push((rel.clone(), change));
    }
    Ok(out)
}

/// Build a `FileChange` summary, returning `None` if the file is unchanged.
fn classify(
    root: &Path,
    base: &dyn BaselineSource,
    rel: &Path,
    kind: ChangeKind,
    cfg: &ScanConfig,
) -> Result<Option<FileChange>> {
    let old = if kind == ChangeKind::Added {
        None
    } else {
        base.content(rel)?
    };
    let new = if kind == ChangeKind::Deleted {
        None
    } else {
        read_live(root, rel)?
    };

    if kind == ChangeKind::Modified && old.as_deref() == new.as_deref() {
        return Ok(None);
    }

    let diff_kind = diff::classify(old.as_deref(), new.as_deref(), cfg.size_cap);
    let (added, removed) = if diff_kind == DiffKind::Text {
        diff::count_lines(&to_text(old.as_deref()), &to_text(new.as_deref()))
    } else {
        (0, 0)
    };

    Ok(Some(FileChange {
        path: rel.to_path_buf(),
        kind,
        diff_kind,
        added,
        removed,
    }))
}

/// Compute the full diff (hunks + texts) for a single file.
pub fn diff_file(
    root: &Path,
    base: &dyn BaselineSource,
    rel: &Path,
    kind: ChangeKind,
    cfg: &ScanConfig,
) -> Result<FileDiff> {
    let old = if kind == ChangeKind::Added {
        None
    } else {
        base.content(rel)?
    };
    let new = if kind == ChangeKind::Deleted {
        None
    } else {
        read_live(root, rel)?
    };

    let diff_kind = diff::classify(old.as_deref(), new.as_deref(), cfg.size_cap);
    if diff_kind != DiffKind::Text {
        return Ok(FileDiff::placeholder(rel.to_path_buf(), kind, diff_kind));
    }

    let old_text = old.as_deref().map(|b| String::from_utf8_lossy(b).into_owned());
    let new_text = new.as_deref().map(|b| String::from_utf8_lossy(b).into_owned());
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

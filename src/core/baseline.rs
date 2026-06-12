//! Baseline sources the working tree is diffed against: a snapshot dir or HEAD.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::excludes;

/// A source of "old" file contents to diff the live tree against.
pub trait BaselineSource {
    /// Every file path present in the baseline, relative to the repo root.
    fn paths(&self) -> Result<Vec<PathBuf>>;
    /// Content of `rel` in the baseline, or `None` if it is absent there.
    fn content(&self, rel: &Path) -> Result<Option<Vec<u8>>>;
    /// Cheap metadata check: `true` means the live file at `live` is
    /// definitely unchanged vs the baseline's `rel` (rsync size+mtime
    /// semantics), so content reads can be skipped. `false` means unknown —
    /// do the full comparison.
    fn quick_unchanged(&self, live: &Path, rel: &Path) -> bool {
        let _ = (live, rel);
        false
    }
    /// Size in bytes of `rel` in the baseline, if cheaply available without
    /// loading the content.
    fn size(&self, rel: &Path) -> Option<u64> {
        let _ = rel;
        None
    }
    /// Human-readable label for the status bar.
    fn label(&self) -> &str;
}

/// Which baseline to use; resolved into a concrete source on the worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineRef {
    /// The `latest` snapshot symlink target.
    Latest,
    /// A named snapshot under `.snapshots/`.
    Snapshot(String),
    /// The git HEAD commit.
    GitHead,
}

/// A baseline backed by a `.snapshots/<name>` directory.
pub struct SnapshotBaseline {
    dir: PathBuf,
    label: String,
}

impl SnapshotBaseline {
    pub fn new(dir: PathBuf, label: String) -> Self {
        SnapshotBaseline { dir, label }
    }
}

impl BaselineSource for SnapshotBaseline {
    fn paths(&self) -> Result<Vec<PathBuf>> {
        excludes::walk_all(&self.dir)
    }
    fn content(&self, rel: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.dir.join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn quick_unchanged(&self, live: &Path, rel: &Path) -> bool {
        // Snapshot copies preserve the source mtime exactly so this check
        // works across saves; hardlinked files share it for free.
        crate::core::snapshot::files_match(live, &self.dir.join(rel))
    }
    fn size(&self, rel: &Path) -> Option<u64> {
        std::fs::metadata(self.dir.join(rel)).ok().map(|m| m.len())
    }
    fn label(&self) -> &str {
        &self.label
    }
}

/// A baseline backed by the git HEAD commit's tree.
pub struct GitHeadBaseline {
    repo: gix::Repository,
    /// Relative path -> blob object id in HEAD's tree.
    entries: HashMap<PathBuf, gix::ObjectId>,
    label: String,
}

impl GitHeadBaseline {
    pub fn open(repo_root: &Path) -> Result<Self> {
        let repo = gix::open(repo_root).context("opening git repository")?;

        // Scope `commit`/`tree` (which borrow `repo`) so they are dropped before
        // `repo` is moved into the returned struct.
        let (entries, short) = {
            let commit = repo.head_commit().context("resolving HEAD commit")?;
            let short = commit.id().to_hex_with_len(8).to_string();
            let tree = commit.tree().context("reading HEAD tree")?;

            let mut recorder = gix::traverse::tree::Recorder::default();
            tree.traverse()
                .breadthfirst(&mut recorder)
                .context("walking HEAD tree")?;

            let mut entries = HashMap::new();
            for entry in recorder.records {
                if entry.mode.is_blob() {
                    let bytes: &[u8] = entry.filepath.as_ref();
                    entries.insert(PathBuf::from(OsStr::from_bytes(bytes)), entry.oid);
                }
            }
            (entries, short)
        };

        Ok(GitHeadBaseline {
            repo,
            entries,
            label: format!("HEAD ({short})"),
        })
    }
}

impl BaselineSource for GitHeadBaseline {
    fn paths(&self) -> Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = self.entries.keys().cloned().collect();
        v.sort();
        Ok(v)
    }
    fn content(&self, rel: &Path) -> Result<Option<Vec<u8>>> {
        match self.entries.get(rel) {
            Some(oid) => {
                let obj = self.repo.find_object(*oid).context("reading blob")?;
                Ok(Some(obj.data.clone()))
            }
            None => Ok(None),
        }
    }
    fn size(&self, rel: &Path) -> Option<u64> {
        // Object header lookup decodes only the size, not the content.
        let oid = self.entries.get(rel)?;
        self.repo.find_header(*oid).ok().map(|h| h.size())
    }
    fn label(&self) -> &str {
        &self.label
    }
}

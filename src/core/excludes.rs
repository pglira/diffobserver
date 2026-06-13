//! Walking the working tree and snapshot dirs with the right exclude rules.
//!
//! The live tree is walked honoring `.gitignore` (and nested ignores, global
//! excludes, and `.git/info/exclude`) via the `ignore` crate. We always also
//! skip `.git/` and `.snapshots/` regardless of ignore rules. Dotfiles are
//! *not* hidden, so changes to e.g. `.gitignore` or `.github/` are visible.
//! Git LFS files (matched via `.gitattributes`, see [`LfsMatcher`]) are
//! excluded too, since HEAD holds only a pointer for them.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Matches files stored in Git LFS, derived from `filter=lfs` rules in
/// `.gitattributes`. LFS files are excluded from diffobserver entirely: a
/// committed LFS blob is just a ~130-byte pointer, so diffing it against the
/// smudged working-tree binary reports spurious changes that `git status`
/// never shows. Matching on the attribute rule (not on a committed pointer)
/// also hides brand-new LFS files that have not been committed yet.
pub struct LfsMatcher(Gitignore);

impl LfsMatcher {
    /// Build from the repo-root `.gitattributes` and `.git/info/attributes`.
    /// Nested per-directory `.gitattributes` are not consulted; LFS rules are
    /// conventionally declared at the repo root.
    pub fn build(root: &Path) -> Self {
        let mut b = GitignoreBuilder::new(root);
        for src in [
            root.join(".gitattributes"),
            root.join(".git/info/attributes"),
        ] {
            let Ok(text) = std::fs::read_to_string(&src) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                // Skip blanks, comments, and `[attr]` macro definitions.
                if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                    continue;
                }
                let mut toks = line.split_whitespace();
                let Some(pattern) = toks.next() else { continue };
                // gitattributes patterns share gitignore glob syntax, so the
                // ignore crate's matcher applies them faithfully.
                if toks.any(|a| a == "filter=lfs") {
                    let _ = b.add_line(None, pattern);
                }
            }
        }
        LfsMatcher(b.build().unwrap_or_else(|_| Gitignore::empty()))
    }

    /// True if `rel` (relative to the repo root) is an LFS-tracked file.
    pub fn is_lfs(&self, rel: &Path) -> bool {
        self.0.matched(rel, false).is_ignore()
    }
}

/// Walk the live working tree at `root`, returning every non-excluded file's
/// path relative to `root`. Files tracked by Git LFS (per `lfs`) are excluded.
pub fn walk_live(root: &Path, lfs: &LfsMatcher) -> Result<Vec<PathBuf>> {
    // Overrides use inverted gitignore semantics: a bare glob whitelists, a
    // `!`-prefixed glob blacklists. With only blacklist patterns, everything
    // else is included by default and these dirs are pruned from the walk.
    let mut ov = OverrideBuilder::new(root);
    ov.add("!.git")?;
    ov.add("!.snapshots")?;
    let overrides = ov.build()?;

    let mut paths = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Honor .gitignore files even outside a git repository — non-git
        // directories are a supported use case.
        .require_git(false)
        .parents(true)
        .overrides(overrides)
        .build();

    for result in walker {
        // Skip unreadable entries (e.g. permission-denied directories): one
        // inaccessible path must not abort the whole walk.
        let Ok(entry) = result else { continue };
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            if !rel.as_os_str().is_empty() && !lfs.is_lfs(rel) {
                paths.push(rel.to_path_buf());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Walk every file under `dir` with no ignore filtering at all (used for
/// snapshot directories, whose contents were already filtered at save time).
pub fn walk_all(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !dir.exists() {
        return Ok(paths);
    }
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(dir) {
            if !rel.as_os_str().is_empty() {
                paths.push(rel.to_path_buf());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

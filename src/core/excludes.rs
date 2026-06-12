//! Walking the working tree and snapshot dirs with the right exclude rules.
//!
//! The live tree is walked honoring `.gitignore` (and nested ignores, global
//! excludes, and `.git/info/exclude`) via the `ignore` crate. We always also
//! skip `.git/` and `.snapshots/` regardless of ignore rules. Dotfiles are
//! *not* hidden, so changes to e.g. `.gitignore` or `.github/` are visible.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Walk the live working tree at `root`, returning every non-excluded file's
/// path relative to `root`.
pub fn walk_live(root: &Path) -> Result<Vec<PathBuf>> {
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
        .parents(true)
        .overrides(overrides)
        .build();

    for result in walker {
        let entry = result?;
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            if !rel.as_os_str().is_empty() {
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
        let entry = result?;
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

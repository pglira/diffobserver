//! Locating the repo root and detecting whether it is a git work tree.

use std::path::{Path, PathBuf};

/// The root directory diffobserver operates on.
#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    /// True when `root` is inside a git work tree (enables the HEAD baseline).
    pub is_git: bool,
}

impl Repo {
    /// Discover the repo root starting at `start`. Uses the git work-tree root
    /// when inside a repo (like `git rev-parse --show-toplevel`); otherwise the
    /// start directory is used as-is.
    pub fn discover(start: &Path) -> Repo {
        match gix::discover(start) {
            // A bare repository has no working tree to diff; treat it like a
            // plain directory rather than offering a meaningless HEAD baseline.
            Ok(repo) => match repo.workdir() {
                Some(workdir) => Repo {
                    root: workdir.to_path_buf(),
                    is_git: true,
                },
                None => Repo {
                    root: start.to_path_buf(),
                    is_git: false,
                },
            },
            Err(_) => Repo {
                root: start.to_path_buf(),
                is_git: false,
            },
        }
    }
}

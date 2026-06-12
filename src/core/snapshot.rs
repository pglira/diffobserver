//! Taking and listing snapshots in snap.sh's `.snapshots/<name>` layout.
//!
//! Unchanged files are hard-linked to the previous snapshot so each new
//! snapshot only costs disk for files that actually changed. To make the
//! rsync-style size+mtime quick-check work across saves, copied files have
//! their mtime preserved from the source.
//!
//! Saves are atomic: files are copied into a dot-prefixed temp directory that
//! is renamed into place on success and removed on failure, so a failed or
//! interrupted save never leaves a half-written snapshot behind.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::excludes;

/// Metadata about a saved snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub name: String,
    pub is_latest: bool,
}

pub fn snapshots_dir(root: &Path) -> PathBuf {
    root.join(".snapshots")
}

/// The name the `latest` symlink points at, if any.
pub fn latest_name(root: &Path) -> Option<String> {
    let target = std::fs::read_link(snapshots_dir(root).join("latest")).ok()?;
    target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The directory the `latest` symlink resolves to, if it points to a real dir.
pub fn latest_dir(root: &Path) -> Option<PathBuf> {
    let snap = snapshots_dir(root);
    let target = std::fs::read_link(snap.join("latest")).ok()?;
    let dir = if target.is_absolute() {
        target
    } else {
        snap.join(target)
    };
    dir.is_dir().then_some(dir)
}

/// Resolve a snapshot name to its directory.
pub fn snapshot_path(root: &Path, name: &str) -> PathBuf {
    snapshots_dir(root).join(name)
}

/// List saved snapshots, newest first by mtime, marking the latest.
/// Dot-prefixed entries (temp dirs, `.gitignore`) are internal and skipped.
pub fn list(root: &Path) -> Result<Vec<SnapshotInfo>> {
    let dir = snapshots_dir(root);
    let latest = latest_name(root);
    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    if dir.exists() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "latest" || name.starts_with('.') || !e.file_type()?.is_dir() {
                continue;
            }
            entries.push((e.path(), name));
        }
    }
    entries.sort_by_key(|(p, _)| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    entries.reverse();
    Ok(entries
        .into_iter()
        .map(|(_, name)| SnapshotInfo {
            is_latest: Some(&name) == latest.as_ref(),
            name,
        })
        .collect())
}

/// Default snapshot name: `snap-YYYYMMDD-HHMMSS` in local time, falling back
/// to UTC if local time cannot be determined.
pub fn default_name() -> String {
    if let Ok(out) = std::process::Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
    {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let s = s.trim();
                if s.len() == 15 {
                    return format!("snap-{s}");
                }
            }
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    format!("snap-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Reject names that would collide with store internals or escape the dir.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "latest"
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
    {
        anyhow::bail!(
            "invalid snapshot name {name:?} (must not be empty, 'latest', dot-prefixed, or contain slashes)"
        );
    }
    Ok(())
}

/// Save the working tree at `root` into `.snapshots/<name>` and repoint
/// `latest`. Fails if a snapshot of that name already exists.
pub fn save(root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let snap = snapshots_dir(root);
    let dest = snap.join(name);
    if dest.exists() {
        anyhow::bail!("snapshot '{name}' already exists");
    }
    let prev = latest_dir(root);
    std::fs::create_dir_all(&snap).context("creating .snapshots")?;
    ensure_self_gitignore(&snap);

    // Copy into a temp dir first so a failed save leaves nothing behind.
    let tmp = snap.join(format!(".tmp-{name}"));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = copy_tree(root, &tmp, prev.as_deref()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest).context("moving snapshot into place")?;

    // Repoint `latest` atomically via a temp symlink + rename.
    let link = snap.join("latest");
    let tmp_link = snap.join(".latest.tmp");
    let _ = std::fs::remove_file(&tmp_link);
    symlink(name, &tmp_link).context("creating latest symlink")?;
    std::fs::rename(&tmp_link, &link).context("updating latest symlink")?;
    Ok(())
}

/// Copy the working tree into `dest`, hard-linking unchanged files to `prev`.
/// Files that vanish between the walk and the copy are skipped: the tool's
/// whole premise is a tree that is actively changing.
fn copy_tree(root: &Path, dest: &Path, prev: Option<&Path>) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for rel in excludes::walk_live(root)? {
        let src = root.join(&rel);
        let target = dest.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let linked = prev
            .map(|p| p.join(&rel))
            .filter(|prev_file| files_match(&src, prev_file))
            .is_some_and(|prev_file| std::fs::hard_link(&prev_file, &target).is_ok());

        if !linked {
            match copy_preserving_mtime(&src, &target) {
                Ok(()) => set_readonly(&target),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("copying {}", rel.display())),
            }
        }
    }
    Ok(())
}

/// Make the snapshot store ignore itself so `git add -A` in the target repo
/// can never commit it.
fn ensure_self_gitignore(snap: &Path) {
    let path = snap.join(".gitignore");
    if !path.exists() {
        let _ = std::fs::write(path, "*\n");
    }
}

/// rsync-style quick check: same size and same mtime means "unchanged".
pub(crate) fn files_match(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    ma.len() == mb.len() && ma.modified().ok() == mb.modified().ok() && ma.modified().is_ok()
}

/// Copy `src` to `dst`, stamping `dst` with `src`'s mtime as observed BEFORE
/// the copy. If the file is modified mid-copy the torn copy then carries the
/// old mtime, so the next save's quick-check sees a mismatch and re-copies it
/// instead of hard-linking the torn content forward.
fn copy_preserving_mtime(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mtime = std::fs::metadata(src)?.modified().ok();
    std::fs::copy(src, dst)?;
    if let Some(mtime) = mtime {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(dst) {
            let _ = f.set_modified(mtime);
        }
    }
    Ok(())
}

fn set_readonly(p: &Path) {
    if let Ok(meta) = std::fs::metadata(p) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(p, perms);
    }
}

/// Howard Hinnant's civil-from-days, extended with the time of day. Returns
/// (year, month, day, hour, minute, second) in UTC.
fn civil_from_epoch(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min, sec) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dob-snap-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ino(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().ino()
    }

    #[test]
    fn hardlinks_unchanged_copies_changed() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "A1").unwrap();
        std::fs::write(root.join("b.txt"), "B1").unwrap();

        save(&root, "s1").unwrap();
        // Change a.txt only; give it a clearly newer mtime.
        std::fs::write(root.join("a.txt"), "A2-longer").unwrap();
        save(&root, "s2").unwrap();

        let s1 = snapshot_path(&root, "s1");
        let s2 = snapshot_path(&root, "s2");

        // Unchanged b.txt is shared (same inode); changed a.txt is a fresh copy.
        assert_eq!(ino(&s1.join("b.txt")), ino(&s2.join("b.txt")));
        assert_ne!(ino(&s1.join("a.txt")), ino(&s2.join("a.txt")));

        assert_eq!(std::fs::read_to_string(s1.join("a.txt")).unwrap(), "A1");
        assert_eq!(std::fs::read_to_string(s2.join("a.txt")).unwrap(), "A2-longer");
        assert_eq!(latest_name(&root).as_deref(), Some("s2"));

        // The store ignores itself from git.
        assert_eq!(
            std::fs::read_to_string(snapshots_dir(&root).join(".gitignore")).unwrap(),
            "*\n"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reserved_and_invalid_names_rejected_without_side_effects() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "x").unwrap();

        for bad in ["latest", "", ".hidden", "a/b", "..", "."] {
            assert!(save(&root, bad).is_err(), "name {bad:?} must be rejected");
        }
        // The store must be untouched and fully functional afterwards.
        assert!(latest_dir(&root).is_none());
        save(&root, "good").unwrap();
        assert_eq!(latest_name(&root).as_deref(), Some("good"));
        assert_eq!(list(&root).unwrap().len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn failed_save_cleans_up_and_allows_retry() {
        let root = temp_root();
        std::fs::write(root.join("ok.txt"), "fine").unwrap();
        std::fs::write(root.join("secret.txt"), "locked").unwrap();
        std::fs::set_permissions(
            root.join("secret.txt"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let err = save(&root, "s1");
        assert!(err.is_err(), "unreadable file must fail the save");
        // No partial snapshot, no temp dir, no latest pointer.
        assert!(!snapshot_path(&root, "s1").exists());
        assert!(list(&root).unwrap().is_empty());
        assert!(latest_dir(&root).is_none());

        // After fixing the permission, the same name works.
        std::fs::set_permissions(
            root.join("secret.txt"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        save(&root, "s1").unwrap();
        assert_eq!(latest_name(&root).as_deref(), Some("s1"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn civil_epoch_known_value() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(civil_from_epoch(1_609_459_200), (2021, 1, 1, 0, 0, 0));
    }
}

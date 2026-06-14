//! End-to-end tests for the runtime paths not covered by unit tests:
//! the gix HEAD baseline, the snapshot→edit→scan→diff flow, and a headless
//! render of the full TUI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use diffobserver::app::{App, Event};
use diffobserver::config::Config;
use diffobserver::core::baseline::{BaselineSource, GitHeadBaseline};
use diffobserver::core::model::{ChangeKind, DiffKind};
use diffobserver::core::scan::{self, ScanConfig};
use diffobserver::core::snapshot;
use diffobserver::worker;

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dob-it-{tag}-{}-{}", std::process::id(), n));
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

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn git_head_baseline_detects_working_tree_changes() {
    let root = temp_dir("git");
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    write(&root, "keep.txt", "one\ntwo\nthree\n");
    write(&root, "gone.txt", "remove me\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);

    // Modify the working tree against HEAD.
    write(&root, "keep.txt", "one\nTWO\nthree\n"); // modified
    std::fs::remove_file(root.join("gone.txt")).unwrap(); // deleted
    write(&root, "fresh.txt", "brand new\n"); // added (untracked)

    let base = GitHeadBaseline::open(&root).expect("open HEAD baseline");
    assert!(base.label().starts_with("HEAD ("));

    let changes = scan::scan(&root, &base, &ScanConfig::default()).unwrap();
    let kinds: std::collections::HashMap<_, _> = changes
        .iter()
        .map(|c| (c.path.to_string_lossy().to_string(), c.kind))
        .collect();

    assert_eq!(kinds.get("keep.txt"), Some(&ChangeKind::Modified));
    assert_eq!(kinds.get("gone.txt"), Some(&ChangeKind::Deleted));
    assert_eq!(kinds.get("fresh.txt"), Some(&ChangeKind::Added));

    // The full diff of the modified file should have a word-level emphasis.
    let fd = scan::diff_file(
        &root,
        &base,
        Path::new("keep.txt"),
        ChangeKind::Modified,
        &ScanConfig::default(),
    )
    .unwrap();
    assert_eq!(fd.diff_kind, DiffKind::Text);
    assert!(fd
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .flat_map(|l| &l.segments)
        .any(|s| s.emph));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_head_baseline_follows_new_commit() {
    let root = temp_dir("githead-follow");
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    write(&root, "file.txt", "one\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);

    // No snapshots + a git repo => the baseline defaults to HEAD.
    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), true, Config::default(), req_tx.clone());
    app.start();
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    let label_before = app.baseline_label.clone();
    assert!(label_before.starts_with("HEAD ("), "expected HEAD label, got {label_before:?}");

    // Modify the working tree so there is a visible change against HEAD.
    write(&root, "file.txt", "two\n");
    app.on_event(Event::Fs(vec![root.join("file.txt")]));
    pump(&mut app, &evt_rx, Duration::from_secs(3));
    assert_eq!(app.changes.len(), 1, "modified file should diff against HEAD");

    // Commit that change: HEAD now moves and the working tree matches it.
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);

    // The .git write that a commit produces must re-resolve the HEAD baseline.
    app.on_event(Event::Fs(vec![root.join(".git/logs/HEAD")]));
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    assert_ne!(app.baseline_label, label_before, "HEAD label should follow the new commit");
    assert!(app.baseline_label.starts_with("HEAD ("));
    assert!(app.changes.is_empty(), "file now matches the new HEAD, so no diff remains");

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn snapshot_then_edit_shows_in_diff() {
    let root = temp_dir("snap");
    write(&root, "src/main.rs", "fn main() {}\n");
    snapshot::save(&root, "base").unwrap();

    // Edit after the snapshot.
    write(&root, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");

    let dir = snapshot::latest_dir(&root).unwrap();
    let base = diffobserver::core::baseline::SnapshotBaseline::new(dir, "snap:base".into());
    let changes = scan::scan(&root, &base, &ScanConfig::default()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Modified);
    assert!(changes[0].added >= 2);

    std::fs::remove_dir_all(&root).ok();
}

/// Drain worker/fs events for up to `budget`, applying them to the app.
fn pump(app: &mut App, rx: &mpsc::Receiver<Event>, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => app.on_event(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn key(app: &mut App, code: KeyCode) {
    app.on_event(Event::Input(CtEvent::Key(KeyEvent::new_with_kind(
        code,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    ))));
}

#[test]
fn picker_deletes_inactive_snapshot_with_confirmation() {
    let root = temp_dir("picker-del");
    write(&root, "a.txt", "1\n");
    snapshot::save(&root, "s1").unwrap();
    write(&root, "a.txt", "2\n");
    snapshot::save(&root, "s2").unwrap(); // newest => latest => the active baseline

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), false, Config::default(), req_tx.clone());
    app.start();
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    // Open the picker; rows are newest-first: [s2 (latest/active), s1].
    key(&mut app, KeyCode::Char('s'));

    // The active baseline (s2) is protected even after a double-d.
    key(&mut app, KeyCode::Char('d'));
    key(&mut app, KeyCode::Char('d'));
    assert!(snapshot::snapshot_path(&root, "s2").is_dir(), "active baseline must survive");

    // Move to the inactive s1; one `d` only arms the confirmation.
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('d'));
    assert!(snapshot::snapshot_path(&root, "s1").is_dir(), "single d must not delete");
    assert_eq!(app.picker_pending_delete.as_deref(), Some("s1"));

    // A second `d` confirms and removes it; latest is untouched.
    key(&mut app, KeyCode::Char('d'));
    assert!(!snapshot::snapshot_path(&root, "s1").is_dir(), "double d should delete s1");
    assert!(snapshot::snapshot_path(&root, "s2").is_dir());
    assert_eq!(snapshot::latest_name(&root).as_deref(), Some("s2"));
    assert_eq!(app.picker_pending_delete, None);

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rescan_preserves_tree_selection_on_dir_row() {
    let root = temp_dir("yank");
    write(&root, "ui/mod.rs", "fn a() {}\n");
    snapshot::save(&root, "base").unwrap();
    write(&root, "ui/mod.rs", "fn a() { let x = 1; }\n");

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), false, Config::default(), req_tx.clone());
    app.start();
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    // Selection starts on the changed file; move up to the "ui/" dir row.
    assert_eq!(app.tree.selected_path(), Some(PathBuf::from("ui/mod.rs")));
    key(&mut app, KeyCode::Char('k'));
    assert_eq!(app.tree.selected_path(), Some(PathBuf::from("ui")));

    // A filesystem event triggers a rescan; wait for it to complete and make
    // sure the selection was not yanked back to the displayed file.
    app.on_event(Event::Fs(vec![root.join("ui/mod.rs")]));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut rescanned = false;
    while !rescanned && Instant::now() < deadline {
        if let Ok(ev) = evt_rx.recv_timeout(Duration::from_millis(100)) {
            rescanned = matches!(&ev, Event::Worker(worker::Evt::Scanned(_)));
            app.on_event(ev);
        }
    }
    assert!(rescanned, "expected a rescan to complete");
    assert_eq!(app.tree.selected_path(), Some(PathBuf::from("ui")));

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rescan_churn_preserves_diff_scroll() {
    let root = temp_dir("scroll");
    let body: String = (1..=60).map(|i| format!("line {i}\n")).collect();
    write(&root, "long.txt", &body);
    snapshot::save(&root, "base").unwrap();
    let changed: String = (1..=60)
        .map(|i| {
            if i % 7 == 0 {
                format!("LINE {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    write(&root, "long.txt", &changed);

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), false, Config::default(), req_tx.clone());
    app.start();

    // Wait until the initial diff has been computed and applied.
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.diff_total_rows == 0 && Instant::now() < deadline {
        if let Ok(ev) = evt_rx.recv_timeout(Duration::from_millis(100)) {
            app.on_event(ev);
        }
    }
    assert!(app.diff_total_rows > 5, "expected a multi-row diff");

    // Focus the diff pane and scroll down a bit.
    app.diff_area_height = 10;
    key(&mut app, KeyCode::Tab);
    for _ in 0..3 {
        key(&mut app, KeyCode::Char('j'));
    }
    assert_eq!(app.diff_scroll, 3);

    // Filesystem events back-to-back trigger refresh cycles (the worker may
    // coalesce them); the refreshed diff must not reset the scroll position.
    app.on_event(Event::Fs(vec![root.join("long.txt")]));
    app.on_event(Event::Fs(vec![root.join("long.txt")]));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut diffs_seen = 0;
    while diffs_seen < 1 && Instant::now() < deadline {
        if let Ok(ev) = evt_rx.recv_timeout(Duration::from_millis(100)) {
            if matches!(&ev, Event::Worker(worker::Evt::Diff(_, _))) {
                diffs_seen += 1;
            }
            app.on_event(ev);
        }
    }
    // Drain any further events (a second non-coalesced refresh, stragglers).
    pump(&mut app, &evt_rx, Duration::from_millis(500));
    assert!(diffs_seen >= 1, "expected a refreshed diff to arrive");
    assert_eq!(app.diff_scroll, 3, "scroll must survive background refreshes");

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rescan_bursts_are_coalesced() {
    let root = temp_dir("coalesce");
    write(&root, "a.txt", "one\n");
    snapshot::save(&root, "base").unwrap();
    write(&root, "a.txt", "two\n");

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), false, Config::default(), req_tx.clone());
    app.start();
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    // A burst of fs events queues a burst of rescan requests; the worker must
    // coalesce them instead of executing five full scans.
    let mut scans = 0;
    for _ in 0..5 {
        app.on_event(Event::Fs(vec![root.join("a.txt")]));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match evt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                if matches!(&ev, Event::Worker(worker::Evt::Scanned(_))) {
                    scans += 1;
                }
                app.on_event(ev);
            }
            Err(_) => break,
        }
    }
    assert!(scans >= 1, "at least one rescan must run");
    assert!(scans < 5, "burst of 5 rescans must be coalesced, got {scans}");

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_render_does_not_panic() {
    let root = temp_dir("render");
    write(&root, "a/one.rs", "fn a() {}\n");
    write(&root, "b/two.txt", "hello\n");
    snapshot::save(&root, "base").unwrap();
    write(&root, "a/one.rs", "fn a() {\n    let x = 1;\n}\n");
    write(&root, "b/two.txt", "hello world\n");
    write(&root, "c/new.rs", "fn brand_new() {}\n");

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), "base16-ocean.dark".into(), false, evt_tx.clone());
    let mut app = App::new(root.clone(), false, Config::default(), req_tx.clone());
    app.start();

    // Let the worker resolve the baseline and scan.
    pump(&mut app, &evt_rx, Duration::from_secs(3));
    assert!(!app.changes.is_empty(), "expected detected changes");

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    // Exercise a sequence of interactions, rendering after each.
    let actions = [
        KeyCode::Char('j'),
        KeyCode::Enter,
        KeyCode::Char('n'),
        KeyCode::Char('n'),
        KeyCode::Char('N'),
        KeyCode::Char(']'),
        KeyCode::Char('['),
        KeyCode::Char('w'), // split
        KeyCode::Char('w'), // unified
        KeyCode::Tab,
        KeyCode::Char('?'), // help
        KeyCode::Esc,
        KeyCode::Char('s'), // picker
        KeyCode::Esc,
    ];
    terminal.draw(|f| diffobserver::ui::draw(f, &mut app)).unwrap();
    for code in actions {
        key(&mut app, code);
        pump(&mut app, &evt_rx, Duration::from_millis(300));
        terminal.draw(|f| diffobserver::ui::draw(f, &mut app)).unwrap();
    }

    let _ = req_tx.send(worker::Req::Shutdown);
    std::fs::remove_dir_all(&root).ok();
}

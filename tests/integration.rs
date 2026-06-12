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
fn rescan_preserves_tree_selection_on_dir_row() {
    let root = temp_dir("yank");
    write(&root, "ui/mod.rs", "fn a() {}\n");
    snapshot::save(&root, "base").unwrap();
    write(&root, "ui/mod.rs", "fn a() { let x = 1; }\n");

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), evt_tx.clone());
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
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), evt_tx.clone());
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

    // Two filesystem events back-to-back put two scan→diff cycles in flight;
    // the refreshed diffs must not reset the scroll position.
    app.on_event(Event::Fs(vec![root.join("long.txt")]));
    app.on_event(Event::Fs(vec![root.join("long.txt")]));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut diffs_seen = 0;
    while diffs_seen < 2 && Instant::now() < deadline {
        if let Ok(ev) = evt_rx.recv_timeout(Duration::from_millis(100)) {
            if matches!(&ev, Event::Worker(worker::Evt::Diff(_, _))) {
                diffs_seen += 1;
            }
            app.on_event(ev);
        }
    }
    assert_eq!(diffs_seen, 2, "expected two refreshed diffs to arrive");
    assert_eq!(app.diff_scroll, 3, "scroll must survive background refreshes");

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
    let req_tx = worker::spawn(root.clone(), ScanConfig::default(), evt_tx.clone());
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

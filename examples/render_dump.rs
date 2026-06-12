//! Headless render dump: builds the app against a repo and prints the TUI
//! buffer as plain text (no colors). Useful for eyeballing layout.
//!
//!   cargo run --example render_dump -- /path/to/repo [keys]
//!
//! `keys` is an optional string of single-char actions applied before the
//! final render, e.g. "jn nw" (spaces ignored).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use diffobserver::app::{App, Event};
use diffobserver::config::Config;
use diffobserver::core::repo::Repo;
use diffobserver::core::scan::ScanConfig;
use diffobserver::{ui, worker};

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;

fn pump(app: &mut App, rx: &mpsc::Receiver<Event>, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(ev) => app.on_event(ev),
            _ => break,
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let keys = std::env::args().nth(2).unwrap_or_default();
    let start = PathBuf::from(&path).canonicalize().expect("path");
    let repo = Repo::discover(&start);

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let req_tx = worker::spawn(repo.root.clone(), ScanConfig::default(), evt_tx.clone());
    let mut app = App::new(repo.root.clone(), repo.is_git, Config::default(), req_tx);
    app.start();
    pump(&mut app, &evt_rx, Duration::from_secs(3));

    let mut terminal = Terminal::new(TestBackend::new(120, 38)).unwrap();
    for ch in keys.chars().filter(|c| !c.is_whitespace()) {
        let code = match ch {
            '\\' => KeyCode::Enter,
            _ => KeyCode::Char(ch),
        };
        app.on_event(Event::Input(CtEvent::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))));
        pump(&mut app, &evt_rx, Duration::from_millis(300));
    }
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();

    let buf = terminal.backend().buffer().clone();
    let area = buf.area;
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            if let Some(c) = buf.cell((x, y)) {
                line.push_str(c.symbol());
            }
        }
        println!("{}", line.trim_end());
    }
}

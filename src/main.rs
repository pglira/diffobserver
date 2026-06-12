//! Entry point: terminal setup, threads, and the main event loop.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event as CtEvent};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;

use diffobserver::app::{App, Event};
use diffobserver::config::Config;
use diffobserver::core::repo::Repo;
use diffobserver::core::scan::ScanConfig;
use diffobserver::{ui, watch, worker};

type Tui = Terminal<CrosstermBackend<Stdout>>;

const POLL: Duration = Duration::from_millis(100);

const USAGE: &str = "\
diffobserver — snapshot a repo and live-monitor the diff against a baseline.

USAGE:
    diffobserver [PATH]      launch the TUI for PATH (default: current dir)

The repo root is discovered via git; outside a git repo, PATH is used as-is.
Press ? inside the app for the full key list. Config (optional):
    ~/.config/diffobserver/config.toml";

fn main() -> Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    match arg.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            return Ok(());
        }
        "-V" | "--version" => {
            println!("diffobserver {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    if !std::io::IsTerminal::is_terminal(&io::stdout()) {
        anyhow::bail!("diffobserver requires an interactive terminal");
    }

    let start = PathBuf::from(&arg)
        .canonicalize()
        .with_context(|| format!("cannot access path: {arg}"))?;

    let repo = Repo::discover(&start);
    let cfg = Config::load();

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();

    let scan_cfg = ScanConfig {
        size_cap: cfg.size_cap_bytes,
    };
    let req_tx = worker::spawn(repo.root.clone(), scan_cfg, evt_tx.clone());

    // Held for the lifetime of the app; dropping it stops watching.
    let _watcher = watch::spawn(&repo.root, evt_tx.clone())
        .context("starting filesystem watcher")?;

    let mut app = App::new(repo.root.clone(), repo.is_git, cfg, req_tx.clone());
    app.start();

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, &mut app, &evt_rx);
    restore_terminal(&mut terminal)?;

    let _ = req_tx.send(worker::Req::Shutdown);
    result
}

fn run_loop(terminal: &mut Tui, app: &mut App, evt_rx: &Receiver<Event>) -> Result<()> {
    let mut dirty = true;
    loop {
        // Suspend the TUI to run an external editor, if requested.
        if let Some((path, line)) = app.launch_editor.take() {
            run_editor(terminal, app, &path, line)?;
            app.request_rescan();
            dirty = true;
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        if app.should_quit {
            break;
        }

        // Read input directly on this thread so an external editor gets clean
        // stdin; multiplex with worker/fs events via the channel.
        if event::poll(POLL)? {
            match event::read()? {
                ev @ CtEvent::Key(_)
                | ev @ CtEvent::Resize(_, _)
                | ev @ CtEvent::Mouse(_)
                | ev @ CtEvent::Paste(_)
                | ev @ CtEvent::FocusGained
                | ev @ CtEvent::FocusLost => {
                    app.on_event(Event::Input(ev));
                    dirty = true;
                }
            }
        }

        loop {
            match evt_rx.try_recv() {
                Ok(ev) => {
                    app.on_event(ev);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }
    Ok(())
}

fn run_editor(terminal: &mut Tui, app: &mut App, path: &Path, line: usize) -> Result<()> {
    restore_terminal(terminal)?;

    let editor = app.cfg.editor_cmd();
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    let mut cmd = Command::new(&program);
    for arg in parts {
        cmd.arg(arg);
    }
    let base = Path::new(&program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&program);
    if matches!(base, "vi" | "vim" | "nvim" | "gvim" | "nano" | "emacs" | "emacsclient") {
        cmd.arg(format!("+{line}"));
    }
    cmd.arg(path);

    let status = cmd.status();
    *terminal = setup_terminal()?;
    terminal.clear()?;
    if let Err(e) = status {
        app.toast(format!("editor failed: {e}"));
    }
    Ok(())
}

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Make sure the terminal is restored if a panic unwinds through the UI.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

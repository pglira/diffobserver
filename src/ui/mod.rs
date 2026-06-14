//! Rendering: two-pane layout, status bar, and overlays.

pub mod diffview;
pub mod highlight;
pub mod theme;
pub mod tree;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Focus, Overlay};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(app.tree_width),
            Constraint::Min(10),
        ])
        .split(rows[0]);

    draw_tree(f, app, cols[0]);
    draw_diff(f, app, cols[1]);
    draw_status(f, app, rows[1]);

    match app.overlay {
        Overlay::Help => draw_help(f, app, area),
        Overlay::Prompt => draw_prompt(f, app, area),
        Overlay::Picker => draw_picker(f, app, area),
        Overlay::None => {}
    }
}

/// Accent color for the active pane's border (#0074a6).
const ACTIVE_BORDER: Color = Color::Rgb(0, 116, 166);

fn pane_border(focused: bool) -> Style {
    Style::default().fg(if focused { ACTIVE_BORDER } else { Color::DarkGray })
}

/// The active pane gets a heavier (thicker) border to stand out.
fn pane_border_type(focused: bool) -> BorderType {
    if focused {
        BorderType::Thick
    } else {
        BorderType::Plain
    }
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Tree && app.overlay == Overlay::None;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(pane_border_type(focused))
        .border_style(pane_border(focused))
        .title(format!(" Changes ({}) ", app.changes.len()));

    if app.tree.is_empty() {
        let p = Paragraph::new("No changes vs baseline.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, area);
        return;
    }

    let items = app.tree.items();
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::Rgb(40, 44, 60))
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, area, &mut app.tree.state);
}

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Diff && app.overlay == Overlay::None;
    let title = match &app.current_path {
        Some(p) => format!(" {} ", p.display()),
        None => " Diff ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(pane_border_type(focused))
        .border_style(pane_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    app.diff_area_height = inner.height as usize;
    let max = app.diff_total_rows.saturating_sub(app.diff_area_height);
    if app.diff_scroll > max {
        app.diff_scroll = max;
    }

    match &app.prepared {
        Some(prep) => {
            let lines = prep.render(
                app.split,
                app.diff_scroll,
                inner.height as usize,
                inner.width,
                &app.palette,
            );
            f.render_widget(Paragraph::new(lines), inner);
        }
        None => {
            let msg = if app.current_path.is_some() {
                "computing…"
            } else {
                "Select a file with j/k.  Press ? for help."
            };
            f.render_widget(
                Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (added, removed) = app
        .changes
        .iter()
        .fold((0usize, 0usize), |(a, r), c| (a + c.added, r + c.removed));
    let pos = if app.hunk_count() > 0 {
        format!("  ·  change {}/{}", app.current_hunk() + 1, app.hunk_count())
    } else {
        String::new()
    };
    let left = format!(
        " base: {}  ·  {} files  +{} -{}{} ",
        app.baseline_label,
        app.changes.len(),
        added,
        removed,
        pos
    );
    // White on the brand blue (#0074a6): high contrast on every terminal,
    // and cohesive with the active-pane border. ANSI Black-on-Cyan washed
    // out to an unreadable dark-on-bright combo on some themes.
    let bar = Style::default().fg(Color::Rgb(255, 255, 255)).bg(ACTIVE_BORDER);
    f.render_widget(Paragraph::new(left).style(bar), area);

    // Expiry is handled by App::tick(); anything still present is shown.
    let toast = app.toast.as_ref().map(|(m, _)| m.clone());
    let (right_text, right_style) = match toast {
        Some(m) => (
            format!(" {m} "),
            // Explicit RGB: ANSI Black + BOLD renders as bright black (grey)
            // in many terminals, which is unreadable on the yellow background.
            Style::default()
                .fg(Color::Rgb(0, 0, 0))
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        None => (
            " n/N change · s base · S snap · ? help · q quit ".to_string(),
            bar,
        ),
    };
    f.render_widget(
        Paragraph::new(right_text)
            .alignment(Alignment::Right)
            .style(right_style),
        area,
    );
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let rect = centered_rect(64, 80, area);
    f.render_widget(Clear, rect);
    let lines = vec![
        help_line("Navigation", ""),
        help_line("  j / k  ↑ / ↓", "move selection / scroll diff"),
        help_line("  Tab", "switch focus tree ↔ diff"),
        help_line("  Enter / l", "open file / expand dir"),
        help_line("  h", "collapse dir"),
        help_line("  ] / [", "next / previous file"),
        help_line("  n / N", "next / previous change (across files)"),
        help_line("  g / G", "top / bottom"),
        help_line("  Ctrl-d / Ctrl-u", "half-page down / up (diff)"),
        help_line("", ""),
        help_line("Actions", ""),
        help_line("  S", "take a snapshot (name prompt)"),
        help_line("  s", "switch baseline (snapshots / HEAD)"),
        help_line("  d", "delete highlighted snapshot (in the s menu)"),
        help_line("  e", "open $EDITOR at current change"),
        help_line("  r", "force refresh"),
        help_line("  w", "toggle unified / split view"),
        help_line("  < / >", "narrow / widen the tree pane"),
        help_line("", ""),
        help_line("  q / Ctrl-c", "quit"),
        help_line("", ""),
        Line::from(Span::styled(
            "  press any other key to close",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" diffobserver — keys ");
    let p = Paragraph::new(lines)
        .block(block)
        .scroll((app.help_scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(p, rect);
}

fn help_line(keys: &str, desc: &str) -> Line<'static> {
    if desc.is_empty() {
        Line::from(Span::styled(
            keys.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled(format!("{keys:<20}"), Style::default().fg(Color::Yellow)),
            Span::raw(desc.to_string()),
        ])
    }
}

fn draw_prompt(f: &mut Frame, app: &App, area: Rect) {
    let rect = centered_rect_abs(60, 3, area);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" snapshot name (Enter to save, Esc to cancel) ");
    let text = Line::from(vec![
        Span::raw(app.prompt_input.clone()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    f.render_widget(Paragraph::new(text).block(block), rect);
}

fn draw_picker(f: &mut Frame, app: &mut App, area: Rect) {
    let height = (app.picker_items.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let rect = centered_rect_abs(70, height, area);
    f.render_widget(Clear, rect);
    let (title, border) = match &app.picker_pending_delete {
        Some(name) => (
            format!(" delete '{name}'?  d = confirm · any other key = cancel "),
            Color::Red,
        ),
        None => (
            " baseline — Enter select · d delete snapshot · Esc cancel ".to_string(),
            Color::Cyan,
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title);
    let items: Vec<ListItem> = app
        .picker_items
        .iter()
        .map(|it| ListItem::new(it.label.clone()))
        .collect();
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::Rgb(40, 44, 60))
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, rect, &mut app.picker_state);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn centered_rect_abs(percent_x: u16, height: u16, area: Rect) -> Rect {
    let h = height.min(area.height);
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    // u32 intermediate: width * percent overflows u16 on ultrawide terminals.
    let w = (u32::from(area.width) * u32::from(percent_x) / 100) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

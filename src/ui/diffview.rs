//! Preparing and rendering a single file's diff (unified or split).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::core::model::{ChangeKind, DiffKind, DiffLine, FileDiff, LineTag};
use crate::ui::highlight::Highlighter;
use crate::ui::theme::DiffPalette;

/// One prepared diff line: text pieces already merged with syntax colors and
/// word-level emphasis flags. `(foreground, emphasized, text)`.
struct PLine {
    tag: LineTag,
    old_no: Option<usize>,
    new_no: Option<usize>,
    pieces: Vec<(Color, bool, String)>,
}

struct PHunk {
    header: String,
    /// Starting line number in the live file (or baseline file if deleted).
    start_line: usize,
    lines: Vec<PLine>,
}

/// A unified display row.
enum URow {
    Header(usize),
    Body(usize, usize),
}

/// A split display row pairing an optional left and right prepared line.
enum SRow {
    Header(usize),
    Pair(usize, Option<usize>, Option<usize>),
}

/// A fully prepared diff for one file, ready to render in either layout.
/// Built on the worker thread: highlighting can take long on big files and
/// must never block the UI.
pub struct Prepared {
    pub path: std::path::PathBuf,
    pub kind: ChangeKind,
    pub diff_kind: DiffKind,
    hunks: Vec<PHunk>,
    notice: Option<String>,
    urows: Vec<URow>,
    uoff: Vec<usize>,
    srows: Vec<SRow>,
    soff: Vec<usize>,
}

impl Prepared {
    pub fn build(fd: &FileDiff, hl: &Highlighter) -> Prepared {
        // Highlight only as far as the deepest line any hunk displays;
        // everything past it is never rendered.
        let max_old = fd
            .hunks
            .iter()
            .map(|h| h.old_start + h.old_len)
            .max()
            .unwrap_or(0);
        let max_new = fd
            .hunks
            .iter()
            .map(|h| h.new_start + h.new_len)
            .max()
            .unwrap_or(0);
        let old_hl = hl.highlight(&fd.path, fd.old_text.as_deref().unwrap_or(""), max_old);
        let new_hl = hl.highlight(&fd.path, fd.new_text.as_deref().unwrap_or(""), max_new);

        let notice = match fd.diff_kind {
            DiffKind::Binary => Some("Binary file differs (no content diff)".into()),
            DiffKind::TooLarge => Some("File too large — diff skipped".into()),
            DiffKind::Unreadable => {
                Some("File could not be read (permission denied?)".into())
            }
            DiffKind::Text if fd.hunks.is_empty() => Some("No textual changes".into()),
            DiffKind::Text => None,
        };

        let mut hunks = Vec::new();
        for h in &fd.hunks {
            let lines = h
                .lines
                .iter()
                .map(|dl| prepare_line(dl, &old_hl, &new_hl))
                .collect();
            let start_line = if fd.kind == ChangeKind::Deleted {
                h.old_start
            } else {
                h.new_start
            };
            hunks.push(PHunk {
                header: h.header(),
                start_line,
                lines,
            });
        }

        let (urows, uoff) = build_urows(&hunks);
        let (srows, soff) = build_srows(&hunks);

        Prepared {
            path: fd.path.clone(),
            kind: fd.kind,
            diff_kind: fd.diff_kind,
            hunks,
            notice,
            urows,
            uoff,
            srows,
            soff,
        }
    }

    pub fn row_count(&self, split: bool) -> usize {
        if self.notice.is_some() {
            return 1;
        }
        if split {
            self.srows.len()
        } else {
            self.urows.len()
        }
    }

    pub fn hunk_offsets(&self, split: bool) -> &[usize] {
        if split {
            &self.soff
        } else {
            &self.uoff
        }
    }

    /// Starting line number of hunk `idx` in the live file.
    pub fn hunk_lineno(&self, idx: usize) -> Option<usize> {
        self.hunks.get(idx).map(|h| h.start_line)
    }

    /// Render the visible window of rows starting at `scroll`.
    pub fn render(
        &self,
        split: bool,
        scroll: usize,
        height: usize,
        width: u16,
        pal: &DiffPalette,
    ) -> Vec<Line<'static>> {
        if let Some(notice) = &self.notice {
            return vec![Line::from(Span::styled(
                notice.clone(),
                Style::default()
                    .fg(pal.gutter_fg)
                    .add_modifier(Modifier::ITALIC),
            ))];
        }
        if split {
            self.render_split(scroll, height, width, pal)
        } else {
            self.render_unified(scroll, height, pal)
        }
    }

    fn render_unified(&self, scroll: usize, height: usize, pal: &DiffPalette) -> Vec<Line<'static>> {
        self.urows
            .iter()
            .skip(scroll)
            .take(height)
            .map(|r| match r {
                URow::Header(h) => header_line(&self.hunks[*h].header, pal),
                URow::Body(h, l) => unified_line(&self.hunks[*h].lines[*l], pal),
            })
            .collect()
    }

    fn render_split(
        &self,
        scroll: usize,
        height: usize,
        width: u16,
        pal: &DiffPalette,
    ) -> Vec<Line<'static>> {
        let total = width.max(3) as usize;
        let left_w = (total - 1) / 2;
        let right_w = total - 1 - left_w;
        self.srows
            .iter()
            .skip(scroll)
            .take(height)
            .map(|r| match r {
                SRow::Header(h) => header_line(&self.hunks[*h].header, pal),
                SRow::Pair(h, li, ri) => {
                    let mut spans =
                        side_spans(li.map(|i| &self.hunks[*h].lines[i]), Side::Left, left_w, pal);
                    spans.push(Span::styled("│", Style::default().fg(pal.gutter_fg)));
                    spans.extend(side_spans(
                        ri.map(|i| &self.hunks[*h].lines[i]),
                        Side::Right,
                        right_w,
                        pal,
                    ));
                    Line::from(spans)
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

fn build_urows(hunks: &[PHunk]) -> (Vec<URow>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut offsets = Vec::new();
    for (hi, h) in hunks.iter().enumerate() {
        offsets.push(rows.len());
        rows.push(URow::Header(hi));
        for li in 0..h.lines.len() {
            rows.push(URow::Body(hi, li));
        }
    }
    (rows, offsets)
}

fn build_srows(hunks: &[PHunk]) -> (Vec<SRow>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut offsets = Vec::new();
    for (hi, h) in hunks.iter().enumerate() {
        offsets.push(rows.len());
        rows.push(SRow::Header(hi));
        let (mut dels, mut inss): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        for (li, l) in h.lines.iter().enumerate() {
            match l.tag {
                LineTag::Delete => dels.push(li),
                LineTag::Insert => inss.push(li),
                LineTag::Context => {
                    pair_block(hi, &mut dels, &mut inss, &mut rows);
                    rows.push(SRow::Pair(hi, Some(li), Some(li)));
                }
            }
        }
        pair_block(hi, &mut dels, &mut inss, &mut rows);
    }
    (rows, offsets)
}

fn pair_block(hi: usize, dels: &mut Vec<usize>, inss: &mut Vec<usize>, rows: &mut Vec<SRow>) {
    let n = dels.len().max(inss.len());
    for i in 0..n {
        rows.push(SRow::Pair(hi, dels.get(i).copied(), inss.get(i).copied()));
    }
    dels.clear();
    inss.clear();
}

/// Merge a diff line's word-emphasis segments with the syntax-highlighted
/// spans of its source line.
fn prepare_line(
    dl: &DiffLine,
    old_hl: &[Vec<(Color, String)>],
    new_hl: &[Vec<(Color, String)>],
) -> PLine {
    let syn: &[(Color, String)] = match dl.tag {
        LineTag::Delete => dl
            .old_lineno
            .and_then(|n| old_hl.get(n - 1))
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => dl
            .new_lineno
            .and_then(|n| new_hl.get(n - 1))
            .map(Vec::as_slice)
            .unwrap_or(&[]),
    };

    let emph: Vec<bool> = dl
        .segments
        .iter()
        .flat_map(|s| std::iter::repeat_n(s.emph, s.text.chars().count()))
        .collect();

    let mut pieces: Vec<(Color, bool, String)> = Vec::new();
    let mut idx = 0usize;
    for (color, text) in syn {
        for ch in text.chars() {
            let e = emph.get(idx).copied().unwrap_or(false);
            match pieces.last_mut() {
                Some((c, ce, buf)) if *c == *color && *ce == e => buf.push(ch),
                _ => pieces.push((*color, e, ch.to_string())),
            }
            idx += 1;
        }
    }

    // Fall back to raw segment text if there were no syntax spans for the line.
    if pieces.is_empty() {
        for s in &dl.segments {
            pieces.push((Color::Reset, s.emph, s.text.clone()));
        }
    }

    PLine {
        tag: dl.tag,
        old_no: dl.old_lineno,
        new_no: dl.new_lineno,
        pieces,
    }
}

fn bg_for(tag: LineTag, emph: bool, pal: &DiffPalette) -> Color {
    match tag {
        LineTag::Context => pal.ctx_bg,
        LineTag::Insert if emph => pal.add_emph_bg,
        LineTag::Insert => pal.add_bg,
        LineTag::Delete if emph => pal.del_emph_bg,
        LineTag::Delete => pal.del_bg,
    }
}

fn num(n: Option<usize>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

fn sign_of(tag: LineTag) -> char {
    match tag {
        LineTag::Insert => '+',
        LineTag::Delete => '-',
        LineTag::Context => ' ',
    }
}

fn header_line(header: &str, pal: &DiffPalette) -> Line<'static> {
    Line::from(Span::styled(
        header.to_string(),
        Style::default()
            .fg(pal.hunk_fg)
            .add_modifier(Modifier::BOLD),
    ))
}

fn unified_line(pl: &PLine, pal: &DiffPalette) -> Line<'static> {
    let base_bg = bg_for(pl.tag, false, pal);
    let gutter = format!(
        "{:>4} {:>4} {} ",
        num(pl.old_no),
        num(pl.new_no),
        sign_of(pl.tag)
    );
    let mut spans = vec![Span::styled(
        gutter,
        Style::default().fg(pal.gutter_fg).bg(base_bg),
    )];
    for (fg, emph, text) in &pl.pieces {
        spans.push(Span::styled(
            text.clone(),
            Style::default().fg(*fg).bg(bg_for(pl.tag, *emph, pal)),
        ));
    }
    Line::from(spans)
}

/// Render one side of a split row, padded/truncated to exactly `width` columns.
fn side_spans(pl: Option<&PLine>, side: Side, width: usize, pal: &DiffPalette) -> Vec<Span<'static>> {
    let base_bg = pl.map_or(pal.ctx_bg, |p| bg_for(p.tag, false, pal));
    let (no, sign) = match pl {
        None => (None, ' '),
        Some(p) => (
            match side {
                Side::Left => p.old_no,
                Side::Right => p.new_no,
            },
            sign_of(p.tag),
        ),
    };

    let mut spans = Vec::new();
    let mut used = 0usize;
    let gutter = format!("{:>4} {} ", num(no), sign);
    push_trunc(&mut spans, &gutter, pal.gutter_fg, base_bg, width, &mut used);

    if let Some(p) = pl {
        for (fg, emph, text) in &p.pieces {
            if used >= width {
                break;
            }
            push_trunc(&mut spans, text, *fg, bg_for(p.tag, *emph, pal), width, &mut used);
        }
    }
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(base_bg),
        ));
    }
    spans
}

/// Append as much of `text` as fits in the remaining cells, accounting for
/// display width (CJK and emoji occupy two terminal cells, not one — counting
/// chars here desynchronizes the split columns).
fn push_trunc(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    fg: Color,
    bg: Color,
    width: usize,
    used: &mut usize,
) {
    if *used >= width {
        return;
    }
    let mut taken = String::new();
    let mut cells = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if *used + cells + w > width {
            break;
        }
        taken.push(ch);
        cells += w;
    }
    if !taken.is_empty() {
        spans.push(Span::styled(taken, Style::default().fg(fg).bg(bg)));
        *used += cells;
    }
}

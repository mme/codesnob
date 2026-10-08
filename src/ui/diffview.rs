//! The flat diff: files, hunks and lines as one scrollable list of rows,
//! with a sticky file header, line numbers, syntax and intra-line highlights.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::Palette;
use crate::diff::{self, FileDiff, Kind, Status};
use crate::git::FileChange;
use crate::highlight::Highlighter;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// A line of the description block above the files.
    Desc(usize),
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Note(&'static str),
    Blank,
}

/// Where the view is, expressed in terms that survive a reload.
pub struct Anchor {
    path: String,
    /// (side, line number): side 0 is the old file, 1 the new file.
    line: Option<(u8, usize)>,
    /// The view was on a hunk header, not on a line.
    at_hunk: bool,
}

#[derive(Default)]
pub struct DiffView {
    pub files: Vec<FileDiff>,
    rows: Vec<Row>,
    file_starts: Vec<usize>,
    hunk_starts: Vec<usize>,
    gutter_w: usize,
    /// Narrowed to this file index; `None` shows every file.
    only: Option<usize>,
    /// The description block: a commit message or PR body, already wrapped.
    desc: Vec<Line<'static>>,
    pub scroll: usize,
    pub hscroll: usize,
}

impl DiffView {
    pub fn set_files(&mut self, changes: Vec<FileChange>, context: usize) {
        self.files = changes
            .into_iter()
            .map(|c| diff::build(c, context))
            .collect();
        self.only = None;
        self.build_rows();
    }

    /// Narrow to one file, or show all with `None`. Returns false, and shows
    /// all, when the path is not part of this diff.
    pub fn narrow_to_path(&mut self, path: Option<&str>) -> bool {
        let fi = match path {
            Some(p) => match self.files.iter().position(|f| f.path == p) {
                Some(fi) => Some(fi),
                None => {
                    self.narrow_to(None);
                    return false;
                }
            },
            None => None,
        };
        self.narrow_to(fi);
        true
    }

    pub fn narrow_to(&mut self, fi: Option<usize>) {
        if self.only != fi {
            self.only = fi;
            self.build_rows();
        }
    }

    pub fn narrowed(&self) -> Option<usize> {
        self.only
    }

    /// Replace the description block, keeping the diff line in view.
    pub fn set_description(&mut self, lines: Vec<Line<'static>>) {
        // At the very top, stay there so a new description is what you see.
        let anchor = if self.scroll == 0 {
            None
        } else {
            self.anchor()
        };
        self.desc = lines;
        self.build_rows();
        match anchor {
            Some(a) => self.restore(a),
            None => self.scroll = 0,
        }
    }

    /// Scroll so file `fi` starts at the top.
    pub fn scroll_to_file(&mut self, fi: usize) {
        if let Some(r) = self.file_row(fi) {
            self.scroll = r;
        }
    }

    fn file_row(&self, fi: usize) -> Option<usize> {
        self.file_starts
            .iter()
            .copied()
            .find(|&r| self.rows[r] == Row::File(fi))
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn has_files(&self) -> bool {
        !self.files.is_empty()
    }

    pub fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    fn build_rows(&mut self) {
        self.rows.clear();
        self.file_starts.clear();
        self.hunk_starts.clear();
        let mut max_no = 1;
        for i in 0..self.desc.len() {
            self.rows.push(Row::Desc(i));
        }
        for (fi, f) in self.files.iter().enumerate() {
            if self.only.is_some_and(|o| o != fi) {
                continue;
            }
            if !self.rows.is_empty() {
                self.rows.push(Row::Blank);
            }
            self.file_starts.push(self.rows.len());
            self.rows.push(Row::File(fi));
            if f.binary {
                self.rows.push(Row::Note("binary file"));
            } else if f.hunks.is_empty() {
                self.rows.push(Row::Note("no textual changes"));
            }
            max_no = max_no.max(f.old_lines.len()).max(f.new_lines.len());
            for (hi, h) in f.hunks.iter().enumerate() {
                self.hunk_starts.push(self.rows.len());
                self.rows.push(Row::Hunk(fi, hi));
                for li in 0..h.lines.len() {
                    self.rows.push(Row::Line(fi, hi, li));
                }
            }
        }
        if self.files.is_empty() && !self.desc.is_empty() {
            self.rows.push(Row::Blank);
            self.rows.push(Row::Note("no files changed"));
        }
        self.gutter_w = max_no.to_string().len().max(3);
        self.clamp();
    }

    // ---- position --------------------------------------------------------

    pub fn anchor(&self) -> Option<Anchor> {
        let fi = self.current_file()?;
        let path = self.files[fi].path.clone();
        let (line, at_hunk) = match self.rows.get(self.scroll)? {
            Row::Line(fi, hi, li) => {
                let l = &self.files[*fi].hunks[*hi].lines[*li];
                (l.new_no.map(|n| (1, n)).or(l.old_no.map(|n| (0, n))), false)
            }
            Row::Hunk(fi, hi) => {
                let h = &self.files[*fi].hunks[*hi];
                let at = if h.new_count > 0 {
                    (1, h.new_start)
                } else {
                    (0, h.old_start)
                };
                (Some(at), true)
            }
            _ => (None, false),
        };
        Some(Anchor {
            path,
            line,
            at_hunk,
        })
    }

    pub fn restore(&mut self, a: Anchor) {
        let Some(fi) = self.files.iter().position(|f| f.path == a.path) else {
            self.clamp();
            return;
        };
        let Some(start) = self.file_row(fi) else {
            self.clamp();
            return;
        };
        let end = self
            .file_starts
            .iter()
            .copied()
            .find(|&r| r > start)
            .unwrap_or(self.rows.len());
        let mut best: Option<(usize, usize)> = None; // (distance, row)
        if let Some((side, target)) = a.line {
            for (idx, row) in self.rows[start..end].iter().enumerate() {
                let Row::Line(f, h, l) = row else { continue };
                let line = &self.files[*f].hunks[*h].lines[*l];
                let no = if side == 1 { line.new_no } else { line.old_no };
                let Some(no) = no else { continue };
                let dist = no.abs_diff(target);
                if best.is_none_or(|(d, _)| dist < d) {
                    best = Some((dist, start + idx));
                }
            }
        }
        self.scroll = match best {
            // Step back onto the hunk header when we were on one and landed on its first line.
            Some((_, r)) if a.at_hunk && matches!(self.rows[r], Row::Line(_, _, 0)) => r - 1,
            Some((_, r)) => r,
            None => start,
        };
        self.clamp();
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    pub fn clamp(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// First row index at which the last row lands on the bottom line,
    /// accounting for the sticky header that is drawn above the body.
    pub fn end_scroll(&self, body_h: usize) -> usize {
        let mut s = self.rows.len().saturating_sub(body_h);
        if s > 0 && !matches!(self.rows.get(s), Some(Row::File(_))) {
            s += 1;
        }
        s.min(self.max_scroll())
    }

    /// The index into `files` of the file at the top of the view.
    pub fn current_file(&self) -> Option<usize> {
        if self.file_starts.is_empty() {
            return None;
        }
        if self.scroll < self.file_starts[0] {
            // In the description block, above every file.
            return None;
        }
        let i = match self.file_starts.binary_search(&self.scroll) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        match self.rows[self.file_starts[i]] {
            Row::File(fi) => Some(fi),
            _ => None,
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, self.max_scroll() as isize) as usize;
    }

    pub fn next_hunk(&mut self) {
        if let Some(&s) = self.hunk_starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = s;
        }
    }

    pub fn prev_hunk(&mut self) {
        if let Some(&s) = self.hunk_starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = s;
        }
    }

    pub fn next_file(&mut self) {
        if let Some(&s) = self.file_starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = s;
        }
    }

    pub fn prev_file(&mut self) {
        if let Some(&s) = self.file_starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = s;
        }
    }

    // ---- rendering --------------------------------------------------------

    pub fn render(&self, area: Rect, buf: &mut Buffer, p: &Palette, hl: &mut Highlighter) {
        let mut y = area.y;
        let mut idx = self.scroll;
        // Sticky header unless the real header is the first visible row.
        if let Some(fi) = self.current_file()
            && !matches!(self.rows.get(self.scroll), Some(Row::File(_)))
        {
            self.render_file_header(fi, Rect::new(area.x, y, area.width, 1), buf, p);
            y += 1;
        }
        while y < area.bottom() {
            let Some(&row) = self.rows.get(idx) else {
                break;
            };
            self.render_row(row, Rect::new(area.x, y, area.width, 1), buf, p, hl);
            y += 1;
            idx += 1;
        }
    }

    fn render_row(
        &self,
        row: Row,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
    ) {
        match row {
            Row::Desc(i) => {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(self.desc[i].spans.iter().cloned());
                buf.set_line(area.x, area.y, &Line::from(spans), area.width);
            }
            Row::Blank => {}
            Row::File(fi) => self.render_file_header(fi, area, buf, p),
            Row::Note(text) => {
                let line = Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(p.note_fg),
                ));
                buf.set_line(area.x, area.y, &line, area.width);
            }
            Row::Hunk(fi, hi) => {
                let h = &self.files[fi].hunks[hi];
                let mut text = format!(
                    "@@ -{},{} +{},{} @@",
                    h.old_start, h.old_count, h.new_start, h.new_count
                );
                if let Some(f) = &h.func {
                    text.push(' ');
                    text.push_str(f);
                }
                let line = Line::from(Span::styled(text, Style::default().fg(p.hunk_fg)));
                buf.set_line(area.x, area.y, &line, area.width);
            }
            Row::Line(fi, hi, li) => self.render_line(fi, hi, li, area, buf, p, hl),
        }
    }

    fn render_file_header(&self, fi: usize, area: Rect, buf: &mut Buffer, p: &Palette) {
        let f = &self.files[fi];
        let (tag, color) = match f.status {
            Status::Added => ("A", Color::Green),
            Status::Deleted => ("D", Color::Red),
            Status::Modified => ("M", Color::Yellow),
            Status::Renamed => ("R", Color::Magenta),
        };
        let base = Style::default().bg(p.header_bg);
        let mut spans = vec![
            Span::styled(
                format!(" {tag} "),
                base.fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(f.display_path(), base.add_modifier(Modifier::BOLD)),
        ];
        if f.binary {
            spans.push(Span::styled("  binary", base.fg(p.note_fg)));
        } else {
            spans.push(Span::styled(
                format!("  +{}", f.added),
                base.fg(Color::Green),
            ));
            spans.push(Span::styled(
                format!(" −{}", f.removed),
                base.fg(Color::Red),
            ));
        }
        let pos = Span::styled(
            format!(" {}/{} ", fi + 1, self.files.len()),
            base.fg(p.note_fg),
        );
        buf.set_style(area, base);
        let line = Line::from(spans);
        let used = buf.set_line(area.x, area.y, &line, area.width).0;
        let pw = pos.width() as u16;
        if area.right() >= used + pw {
            buf.set_span(area.right() - pw, area.y, &pos, pw);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &self,
        fi: usize,
        hi: usize,
        li: usize,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
    ) {
        let l = &self.files[fi].hunks[hi].lines[li];
        let (line_bg, emph_bg, marker) = match l.kind {
            Kind::Add => (Some(p.add_bg), p.add_emph, "+"),
            Kind::Del => (Some(p.del_bg), p.del_emph, "-"),
            Kind::Context => (None, Color::Reset, " "),
        };
        let row_style = match line_bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        buf.set_style(area, row_style);

        let w = self.gutter_w;
        let gutter = format!(
            "{:>w$} {:>w$} ",
            l.old_no.map(|n| n.to_string()).unwrap_or_default(),
            l.new_no.map(|n| n.to_string()).unwrap_or_default(),
        );
        let marker_style = match l.kind {
            Kind::Add => row_style.fg(Color::Green),
            Kind::Del => row_style.fg(Color::Red),
            Kind::Context => row_style,
        };
        let mut spans = vec![
            Span::styled(gutter, row_style.fg(p.gutter_fg)),
            Span::styled(
                format!("{marker} "),
                marker_style.add_modifier(Modifier::BOLD),
            ),
        ];

        // Syntax spans for the line's source file side.
        let f = &self.files[fi];
        let (side, src_idx, src) = match l.kind {
            Kind::Del => (0u8, l.old_no.unwrap_or(1) - 1, &f.old_lines),
            _ => (1u8, l.new_no.unwrap_or(1) - 1, &f.new_lines),
        };
        let syntax: Vec<(Style, usize, usize)> = hl
            .lines((fi, side), &f.path, src)
            .get(src_idx)
            .cloned()
            .unwrap_or_default();
        let text = &l.text;

        // Merge syntax ranges with emphasis ranges into flat segments.
        let mut cuts: Vec<usize> = vec![0, text.len()];
        for &(_, s, e) in &syntax {
            cuts.push(s);
            cuts.push(e);
        }
        for &(s, e) in &l.emph {
            cuts.push(s);
            cuts.push(e);
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut skip = self.hscroll;
        for pair in cuts.windows(2) {
            let (s, e) = (pair[0], pair[1]);
            if e <= s || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                continue;
            }
            let mut seg = &text[s..e];
            if skip > 0 {
                let n = seg.chars().count();
                if n <= skip {
                    skip -= n;
                    continue;
                }
                let off = seg
                    .char_indices()
                    .nth(skip)
                    .map(|(i, _)| i)
                    .unwrap_or(seg.len());
                seg = &seg[off..];
                skip = 0;
            }
            let mut style = syntax
                .iter()
                .find(|&&(_, ss, se)| ss <= s && e <= se)
                .map(|&(st, _, _)| st)
                .unwrap_or_default();
            if let Some(bg) = line_bg {
                style = style.bg(bg);
            }
            if l.emph.iter().any(|&(es, ee)| es <= s && e <= ee) {
                style = style.bg(emph_bg);
            }
            spans.push(Span::styled(seg.to_string(), style));
        }
        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

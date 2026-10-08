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
    /// A highlight's number and note, above its lines.
    Mark(usize),
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Note(&'static str),
    Blank,
}

/// A highlight as drawn: its file, its lines, its number and note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub number: usize,
    pub note: Option<String>,
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
    /// The files shown, by index; empty shows every file.
    only: Vec<usize>,
    /// The description block: a commit message or PR body, already wrapped.
    desc: Vec<Line<'static>>,
    pub scroll: usize,
    pub hscroll: usize,
    /// A line to mark with ▶ in a plain file: (path, line number).
    pub target: Option<(String, usize)>,
    /// Scroll to `target` at the next draw, once the screen height is known.
    pub place_target: bool,
    /// Highlights, drawn over their lines.
    marks: Vec<Mark>,
    /// The row of each highlight's note, in screen order.
    mark_rows: Vec<usize>,
}

impl DiffView {
    /// Show a diff, also showing the lines `extra` names per path even where
    /// nothing changed.
    pub fn set_files_showing(
        &mut self,
        changes: Vec<FileChange>,
        context: usize,
        extra: &dyn Fn(&str) -> Vec<(usize, usize)>,
    ) {
        self.files = changes
            .into_iter()
            .map(|c| {
                let path = c
                    .new_path
                    .clone()
                    .or(c.old_path.clone())
                    .unwrap_or_default();
                diff::build_with(c, context, &extra(&path))
            })
            .collect();
        self.only.clear();
        self.target = None;
        self.build_rows();
    }

    /// Replace the highlights, keeping the line in view.
    pub fn set_marks(&mut self, marks: Vec<Mark>) {
        if self.marks != marks {
            let anchor = self.anchor();
            self.marks = marks;
            self.build_rows();
            if let Some(a) = anchor {
                self.restore(a);
            }
        }
    }

    pub fn has_marks(&self) -> bool {
        !self.mark_rows.is_empty()
    }

    /// The first highlight's row, to open at.
    pub fn first_mark_row(&self) -> Option<usize> {
        self.mark_rows.first().copied()
    }

    /// Highlights are read a third of the way down the screen, where the
    /// view places them.
    fn reading_row(&self, body_h: usize) -> usize {
        self.scroll + body_h / 3
    }

    /// Which highlight is being read, and how many there are.
    pub fn mark_position(&self, body_h: usize) -> Option<(usize, usize)> {
        if self.mark_rows.is_empty() {
            return None;
        }
        let reading = self.reading_row(body_h);
        let passed = self.mark_rows.iter().filter(|&&r| r <= reading).count();
        Some((passed.max(1), self.mark_rows.len()))
    }

    pub fn next_mark(&mut self, body_h: usize) {
        let reading = self.reading_row(body_h);
        if let Some(&r) = self.mark_rows.iter().find(|&&r| r > reading) {
            self.scroll = r.saturating_sub(body_h / 3);
        }
    }

    pub fn prev_mark(&mut self, body_h: usize) {
        let reading = self.reading_row(body_h);
        if let Some(&r) = self.mark_rows.iter().rev().find(|&&r| r < reading) {
            self.scroll = r.saturating_sub(body_h / 3);
        }
    }

    /// The highlight covering a line of file `fi`, if any.
    fn mark_on(&self, fi: usize, line: &crate::diff::DiffLine) -> Option<&Mark> {
        let f = &self.files[fi];
        let no = line_no(f, line)?;
        self.marks
            .iter()
            .find(|m| m.path == f.path && (m.start..=m.end).contains(&no))
    }

    /// Show files built elsewhere, such as plain files.
    pub fn set_built(&mut self, files: Vec<FileDiff>) {
        self.files = files;
        self.only.clear();
        self.target = None;
        self.build_rows();
    }

    /// The row showing `line` of `path`, if it is on screen.
    pub fn row_of_line(&self, path: &str, line: usize) -> Option<usize> {
        self.rows.iter().position(|r| match *r {
            Row::Line(fi, hi, li) => {
                self.files[fi].path == path
                    && self.files[fi].hunks[hi].lines[li].new_no == Some(line)
            }
            _ => false,
        })
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
        let only: Vec<usize> = fi.into_iter().collect();
        if self.only != only {
            self.only = only;
            self.build_rows();
        }
    }

    /// Show only the files at `paths`, in diff order. Returns how many of
    /// them are part of this diff; with none, every file shows.
    pub fn narrow_to_paths(&mut self, paths: &[String]) -> usize {
        let only: Vec<usize> = (0..self.files.len())
            .filter(|&fi| paths.iter().any(|p| *p == self.files[fi].path))
            .collect();
        let found = only.len();
        if self.only != only {
            self.only = only;
            self.build_rows();
        }
        found
    }

    /// The one file shown, when narrowed to a single file.
    pub fn narrowed(&self) -> Option<usize> {
        match self.only.as_slice() {
            [fi] => Some(*fi),
            _ => None,
        }
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

    /// Lines added and removed across the files shown.
    pub fn totals(&self) -> (usize, usize) {
        self.shown()
            .iter()
            .map(|&fi| &self.files[fi])
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    /// The indices of the files shown, in diff order.
    pub fn shown(&self) -> Vec<usize> {
        if self.only.is_empty() {
            (0..self.files.len()).collect()
        } else {
            self.only.clone()
        }
    }

    fn build_rows(&mut self) {
        self.rows.clear();
        self.file_starts.clear();
        self.hunk_starts.clear();
        self.mark_rows.clear();
        let mut placed = vec![false; self.marks.len()];
        let mut max_no = 1;
        for i in 0..self.desc.len() {
            self.rows.push(Row::Desc(i));
        }
        for (fi, f) in self.files.iter().enumerate() {
            if !self.only.is_empty() && !self.only.contains(&fi) {
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
            // A plain file shown whole needs no hunk header.
            let whole = f.plain
                && f.hunks.len() == 1
                && f.hunks[0].new_start == 1
                && f.hunks[0].new_count == f.new_lines.len();
            for (hi, h) in f.hunks.iter().enumerate() {
                self.hunk_starts.push(self.rows.len());
                if !whole {
                    self.rows.push(Row::Hunk(fi, hi));
                }
                for li in 0..h.lines.len() {
                    // A highlight's note goes above its first line shown.
                    if let Some(no) = line_no(f, &h.lines[li]) {
                        for (m, mark) in self.marks.iter().enumerate() {
                            if !placed[m]
                                && mark.path == f.path
                                && (mark.start..=mark.end).contains(&no)
                            {
                                placed[m] = true;
                                self.mark_rows.push(self.rows.len());
                                self.rows.push(Row::Mark(m));
                            }
                        }
                    }
                    self.rows.push(Row::Line(fi, hi, li));
                }
            }
        }
        if self.files.is_empty() && !self.desc.is_empty() {
            self.rows.push(Row::Blank);
            self.rows.push(Row::Note("no files changed"));
        }
        // With highlights, one more column so the bar never covers a digit.
        self.gutter_w = max_no.to_string().len().max(3) + usize::from(!self.marks.is_empty());
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
            Row::Mark(m) => {
                let mark = &self.marks[m];
                let tint = Style::default().bg(p.mark_bg);
                buf.set_style(area, tint);
                let mut spans = vec![Span::styled(
                    format!("  {} ", numbered(mark.number)),
                    tint.fg(p.mark_fg).add_modifier(Modifier::BOLD),
                )];
                if let Some(note) = &mark.note {
                    spans.push(Span::styled(note.clone(), tint.fg(p.mark_fg)));
                }
                buf.set_line(area.x, area.y, &Line::from(spans), area.width);
            }
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
                let mut text = if self.files[fi].plain {
                    // An excerpt of a plain file says which lines it shows.
                    let end = h.new_start + h.new_count.saturating_sub(1);
                    format!("  ⋯ lines {}–{end}", h.new_start)
                } else {
                    format!(
                        "@@ -{},{} +{},{} @@",
                        h.old_start, h.old_count, h.new_start, h.new_count
                    )
                };
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
        let mut spans = if f.plain {
            // A file as it is: no change to label.
            vec![Span::styled(
                format!(" {}", f.display_path()),
                base.add_modifier(Modifier::BOLD),
            )]
        } else {
            vec![
                Span::styled(
                    format!(" {tag} "),
                    base.fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(f.display_path(), base.add_modifier(Modifier::BOLD)),
            ]
        };
        if f.binary {
            spans.push(Span::styled("  binary", base.fg(p.note_fg)));
        } else if f.plain {
            let n = f.new_lines.len();
            spans.push(Span::styled(
                format!("  {n} line{}", if n == 1 { "" } else { "s" }),
                base.fg(p.note_fg),
            ));
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
        // Highlighted context lines take the highlight's tint; changed lines
        // keep their color and get the bar in the gutter only.
        let marked = self.mark_on(fi, l).is_some();
        let line_bg = match (line_bg, marked) {
            (None, true) => Some(p.mark_bg),
            (bg, _) => bg,
        };
        let row_style = match line_bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        buf.set_style(area, row_style);

        let w = self.gutter_w;
        let plain = self.files[fi].plain;
        let gutter = if plain {
            // A file as it is has one numbering.
            format!(
                "{:>w$} ",
                l.new_no.map(|n| n.to_string()).unwrap_or_default()
            )
        } else {
            format!(
                "{:>w$} {:>w$} ",
                l.old_no.map(|n| n.to_string()).unwrap_or_default(),
                l.new_no.map(|n| n.to_string()).unwrap_or_default(),
            )
        };
        let is_target = plain
            && self.target.as_ref().is_some_and(|(path, line)| {
                *path == self.files[fi].path && l.new_no == Some(*line)
            });
        let (marker, marker_style) = match l.kind {
            _ if is_target => ("▶", row_style.fg(p.hunk_fg)),
            Kind::Add => (marker, row_style.fg(Color::Green)),
            Kind::Del => (marker, row_style.fg(Color::Red)),
            Kind::Context => (marker, row_style),
        };
        let mut spans = Vec::new();
        let gutter = if marked {
            spans.push(Span::styled("▌", row_style.fg(p.mark_fg)));
            gutter.chars().skip(1).collect()
        } else {
            gutter
        };
        spans.push(Span::styled(gutter, row_style.fg(p.gutter_fg)));
        spans.push(Span::styled(
            format!("{marker} "),
            marker_style.add_modifier(Modifier::BOLD),
        ));

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

/// The number a highlight is addressed by: the new side's, or the old side's
/// in a deleted file.
fn line_no(f: &FileDiff, line: &crate::diff::DiffLine) -> Option<usize> {
    match f.status {
        Status::Deleted => line.old_no,
        _ => line.new_no,
    }
}

/// ① to ⑳, then (21) and on.
fn numbered(n: usize) -> String {
    match n {
        1..=20 => char::from_u32(0x2460 + n as u32 - 1)
            .map(String::from)
            .unwrap_or_else(|| format!("({n})")),
        _ => format!("({n})"),
    }
}

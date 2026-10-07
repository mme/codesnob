//! Application state, key handling and rendering.

use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::diff::{self, FileDiff, Kind, Status};
use crate::git;
use crate::highlight::Highlighter;
use crate::link::Link;
use crate::proto::Target;
use crate::watch::{self, Mode, Msg};

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Note(&'static str),
    Blank,
}

pub struct Palette {
    pub add_bg: Color,
    pub add_emph: Color,
    pub del_bg: Color,
    pub del_emph: Color,
    pub header_bg: Color,
    pub hunk_fg: Color,
    pub gutter_fg: Color,
    pub note_fg: Color,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            add_bg: Color::Rgb(0x12, 0x33, 0x1c),
            add_emph: Color::Rgb(0x1f, 0x6b, 0x2f),
            del_bg: Color::Rgb(0x3b, 0x12, 0x19),
            del_emph: Color::Rgb(0x7a, 0x1f, 0x2b),
            header_bg: Color::Rgb(0x2a, 0x2d, 0x3a),
            hunk_fg: Color::Cyan,
            gutter_fg: Color::DarkGray,
            note_fg: Color::DarkGray,
        }
    }

    pub fn light() -> Self {
        Self {
            add_bg: Color::Rgb(0xe2, 0xf5, 0xe6),
            add_emph: Color::Rgb(0xa8, 0xe8, 0xb5),
            del_bg: Color::Rgb(0xff, 0xe3, 0xe6),
            del_emph: Color::Rgb(0xff, 0xb5, 0xbd),
            header_bg: Color::Rgb(0xe4, 0xe6, 0xef),
            hunk_fg: Color::Blue,
            gutter_fg: Color::Gray,
            note_fg: Color::Gray,
        }
    }
}

pub struct Options {
    pub context: usize,
}

/// Where the view is, expressed in terms that survive a reload.
struct Anchor {
    path: String,
    /// (side, line number): side 0 is the old file, 1 the new file.
    line: Option<(u8, usize)>,
    /// The view was on a hunk header, not on a line.
    at_hunk: bool,
}

pub struct App {
    opts: Options,
    palette: Palette,
    hl: Highlighter,
    head_label: String,
    base_label: String,
    base_reason: String,
    files: Vec<FileDiff>,
    rows: Vec<Row>,
    file_starts: Vec<usize>,
    hunk_starts: Vec<usize>,
    gutter_w: usize,
    scroll: usize,
    hscroll: usize,
    body_h: usize,
    show_help: bool,
    message: Option<String>,
    tx: Sender<Msg>,
    link: Link,
    refresh: Sender<()>,
    mode: Mode,
    generation: u64,
    target: Option<Target>,
    linked: bool,
    fingerprint: u64,
    manual_pending: bool,
    idle: bool,
    pub quit: bool,
}

impl App {
    pub fn new(opts: Options, palette: Palette, hl: Highlighter, tx: Sender<Msg>, link: Link) -> Self {
        let (refresh, _) = std::sync::mpsc::channel();
        Self {
            opts,
            palette,
            hl,
            head_label: String::new(),
            base_label: String::new(),
            base_reason: String::new(),
            files: Vec::new(),
            rows: Vec::new(),
            file_starts: Vec::new(),
            hunk_starts: Vec::new(),
            gutter_w: 3,
            scroll: 0,
            hscroll: 0,
            body_h: 1,
            show_help: false,
            message: None,
            tx,
            link,
            refresh,
            mode: Mode::Off,
            generation: 0,
            target: None,
            linked: false,
            fingerprint: 0,
            manual_pending: false,
            idle: true,
            quit: false,
        }
    }

    /// Display a target: a repository, or a placeholder when `dir` is not in one.
    pub fn open(&mut self, target: Target) {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        self.generation += 1;
        self.scroll = 0;
        self.hscroll = 0;
        self.message = None;
        // Dropping the old sender ends the previous refresher thread.
        let (dummy, _) = std::sync::mpsc::channel();
        self.refresh = dummy;
        self.mode = Mode::Off;

        let snap = if target.repo { git::snapshot(&target.dir, target.base.as_deref()) } else { Err(anyhow::anyhow!("not a repository")) };
        match snap {
            Ok(snap) => {
                self.idle = false;
                self.load(snap);
                let (refresh, mode) = watch::spawn_refresher(target.dir.clone(), target.base.clone(), self.tx.clone(), self.generation);
                self.refresh = refresh;
                self.mode = mode;
            }
            Err(e) => {
                self.idle = true;
                self.load(git::Snapshot {
                    head_label: "no repository".to_string(),
                    base_label: target.dir.display().to_string(),
                    base_reason: if target.repo { format!("{e:#}") } else { "not inside a git repository".to_string() },
                    files: Vec::new(),
                });
            }
        }
        self.link.set_target(target.clone());
        self.target = Some(target);
    }

    pub fn set_linked(&mut self, up: bool) {
        self.linked = up;
    }

    pub fn summary(&self) -> String {
        format!("{} → {} ({})", self.head_label, self.base_label, self.base_reason)
    }

    fn mode_label(&self) -> String {
        let refresh = match self.mode {
            Mode::Watching => "auto-refresh on file changes",
            Mode::Polling => "auto-refresh by polling every 2s",
            Mode::Off => "auto-refresh off",
        };
        let link = if self.linked { "daemon connected" } else { "daemon not connected" };
        format!("{refresh} · {link}")
    }

    fn load(&mut self, snap: git::Snapshot) {
        self.fingerprint = snap.fingerprint();
        self.head_label = snap.head_label;
        self.base_label = snap.base_label;
        self.base_reason = snap.base_reason;
        self.files = snap.files.into_iter().map(|c| diff::build(c, self.opts.context)).collect();
        self.hl.clear();
        self.build_rows();
    }

    fn build_rows(&mut self) {
        self.rows.clear();
        self.file_starts.clear();
        self.hunk_starts.clear();
        let mut max_no = 1;
        for (fi, f) in self.files.iter().enumerate() {
            if fi > 0 {
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
        self.gutter_w = max_no.to_string().len().max(3);
        self.clamp();
    }

    fn request_reload(&mut self) {
        if self.idle {
            return;
        }
        self.manual_pending = true;
        self.message = Some("reloading…".to_string());
        self.refresh.send(()).ok();
    }

    /// A snapshot arrived from the refresh thread.
    pub fn apply_refresh(&mut self, generation: u64, result: Result<git::Snapshot, String>) {
        if generation != self.generation {
            return;
        }
        let manual = std::mem::take(&mut self.manual_pending);
        match result {
            Err(e) => self.message = Some(format!("refresh failed: {e}")),
            Ok(snap) if snap.fingerprint() == self.fingerprint => {
                self.message = manual.then(|| "no changes since last load".to_string());
            }
            Ok(snap) => {
                let anchor = self.anchor();
                self.load(snap);
                if let Some(a) = anchor {
                    self.restore(a);
                }
                self.message = manual.then(|| "reloaded".to_string());
            }
        }
    }

    fn anchor(&self) -> Option<Anchor> {
        let fi = self.current_file()?;
        let path = self.files[fi].path.clone();
        let (line, at_hunk) = match self.rows.get(self.scroll)? {
            Row::Line(fi, hi, li) => {
                let l = &self.files[*fi].hunks[*hi].lines[*li];
                (l.new_no.map(|n| (1, n)).or(l.old_no.map(|n| (0, n))), false)
            }
            Row::Hunk(fi, hi) => {
                let h = &self.files[*fi].hunks[*hi];
                (Some(if h.new_count > 0 { (1, h.new_start) } else { (0, h.old_start) }), true)
            }
            _ => (None, false),
        };
        Some(Anchor { path, line, at_hunk })
    }

    fn restore(&mut self, a: Anchor) {
        let Some(fi) = self.files.iter().position(|f| f.path == a.path) else {
            self.clamp();
            return;
        };
        let start = self.file_starts[fi];
        let end = self.file_starts.get(fi + 1).copied().unwrap_or(self.rows.len());
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

    fn clamp(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// First row index at which the last row lands on the bottom line,
    /// accounting for the sticky header that is drawn above the body.
    fn end_scroll(&self) -> usize {
        let mut s = self.rows.len().saturating_sub(self.body_h);
        if s > 0 && !matches!(self.rows.get(s), Some(Row::File(_))) {
            s += 1;
        }
        s.min(self.max_scroll())
    }

    fn current_file(&self) -> Option<usize> {
        if self.file_starts.is_empty() {
            return None;
        }
        Some(match self.file_starts.binary_search(&self.scroll) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        })
    }

    fn jump_next(&mut self, starts: &[usize]) {
        if let Some(&s) = starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = s;
        }
    }

    fn jump_prev(&mut self, starts: &[usize]) {
        if let Some(&s) = starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = s;
        }
    }

    // ---- input ----------------------------------------------------------

    pub fn dispatch(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => self.handle(ev),
            Msg::Refresh { generation, result } => self.apply_refresh(generation, result),
            Msg::Show(target) => self.open(target),
            Msg::Link(up) => self.set_linked(up),
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != crossterm::event::KeyEventKind::Release => self.key(k),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollDown => self.scroll_by(3),
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                _ => {}
            },
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, self.max_scroll() as isize) as usize;
    }

    fn key(&mut self, k: KeyEvent) {
        self.message = None;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let page = self.body_h.saturating_sub(2).max(1) as isize;
        let half = (self.body_h / 2).max(1) as isize;
        let file_starts = self.file_starts.clone();
        let hunk_starts = self.hunk_starts.clone();

        if self.show_help {
            match k.code {
                KeyCode::Char('c') | KeyCode::Char('q') if ctrl => self.quit = true,
                _ => self.show_help = false,
            }
            return;
        }

        match (k.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) | (KeyCode::Char('q'), _, _) | (KeyCode::Esc, _, _) => self.quit = true,

            (KeyCode::Down, _, _) | (KeyCode::Char('j'), false, false) | (KeyCode::Char('n'), true, false) => {
                self.scroll_by(1)
            }
            (KeyCode::Up, _, _) | (KeyCode::Char('k'), false, false) | (KeyCode::Char('p'), true, false) => {
                self.scroll_by(-1)
            }
            (KeyCode::PageDown, _, _)
            | (KeyCode::Char(' '), false, false)
            | (KeyCode::Char('f'), false, false)
            | (KeyCode::Char('v'), true, false) => self.scroll_by(page),
            (KeyCode::PageUp, _, _)
            | (KeyCode::Backspace, _, _)
            | (KeyCode::Char('b'), false, false)
            | (KeyCode::Char('v'), false, true) => self.scroll_by(-page),
            (KeyCode::Char('d'), true, false) => self.scroll_by(half),
            (KeyCode::Char('u'), true, false) => self.scroll_by(-half),

            (KeyCode::Home, _, _) | (KeyCode::Char('g'), false, false) | (KeyCode::Char('<'), false, true) => {
                self.scroll = 0
            }
            (KeyCode::End, _, _) | (KeyCode::Char('G'), false, false) | (KeyCode::Char('>'), false, true) => {
                self.scroll = self.end_scroll()
            }

            (KeyCode::Char('n'), false, false) | (KeyCode::Char(']'), false, false) | (KeyCode::Enter, _, _) => {
                self.jump_next(&hunk_starts)
            }
            (KeyCode::Char('p'), false, false) | (KeyCode::Char('['), false, false) => self.jump_prev(&hunk_starts),
            (KeyCode::Char('N'), false, false) | (KeyCode::Char('}'), false, false) | (KeyCode::Tab, _, _) => {
                self.jump_next(&file_starts)
            }
            (KeyCode::Char('P'), false, false) | (KeyCode::Char('{'), false, false) | (KeyCode::BackTab, _, _) => {
                self.jump_prev(&file_starts)
            }

            (KeyCode::Right, _, _) | (KeyCode::Char('f'), true, false) | (KeyCode::Char('l'), false, false) => {
                self.hscroll += 8
            }
            (KeyCode::Left, _, _) | (KeyCode::Char('b'), true, false) | (KeyCode::Char('h'), false, false) => {
                self.hscroll = self.hscroll.saturating_sub(8)
            }
            (KeyCode::Char('0'), false, false) | (KeyCode::Char('a'), true, false) => self.hscroll = 0,

            (KeyCode::Char('r'), false, false) | (KeyCode::F(5), _, _) => self.request_reload(),
            (KeyCode::Char('?'), _, _) => self.show_help = true,
            _ => {}
        }
    }

    // ---- rendering ------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height < 2 || area.width < 4 {
            return;
        }
        let body = Rect::new(area.x, area.y, area.width, area.height - 1);
        let status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        self.body_h = body.height as usize;
        self.clamp();

        let buf = frame.buffer_mut();
        let mut y = body.y;
        let mut idx = self.scroll;

        // Sticky header unless the real header is the first visible row.
        if let Some(fi) = self.current_file() {
            if !matches!(self.rows.get(self.scroll), Some(Row::File(_))) {
                self.render_file_header(fi, Rect::new(body.x, y, body.width, 1), buf);
                y += 1;
            }
        }
        while y < body.bottom() {
            let Some(&row) = self.rows.get(idx) else { break };
            self.render_row(row, Rect::new(body.x, y, body.width, 1), buf);
            y += 1;
            idx += 1;
        }
        if self.rows.is_empty() {
            let lines: Vec<String> = if self.idle {
                vec!["not inside a git repository".to_string(), self.base_label.clone()]
            } else {
                vec![format!("no changes between {} and {}", self.head_label, self.base_label)]
            };
            let top = body.y + (body.height.saturating_sub(lines.len() as u16)) / 2;
            for (i, text) in lines.iter().enumerate() {
                let line = Line::from(Span::styled(text.clone(), Style::default().fg(self.palette.note_fg)));
                let w = (line.width() as u16).min(body.width);
                buf.set_line(body.x + (body.width - w) / 2, top + i as u16, &line, w);
            }
        }

        self.render_status(status, buf);
        if self.show_help {
            self.render_help(area, buf);
        }
    }

    fn render_row(&mut self, row: Row, area: Rect, buf: &mut Buffer) {
        match row {
            Row::Blank => {}
            Row::File(fi) => self.render_file_header(fi, area, buf),
            Row::Note(text) => {
                let line = Line::from(Span::styled(format!("  {text}"), Style::default().fg(self.palette.note_fg)));
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
                let line = Line::from(Span::styled(text, Style::default().fg(self.palette.hunk_fg)));
                buf.set_line(area.x, area.y, &line, area.width);
            }
            Row::Line(fi, hi, li) => self.render_line(fi, hi, li, area, buf),
        }
    }

    fn render_file_header(&self, fi: usize, area: Rect, buf: &mut Buffer) {
        let f = &self.files[fi];
        let (tag, color) = match f.status {
            Status::Added => ("A", Color::Green),
            Status::Deleted => ("D", Color::Red),
            Status::Modified => ("M", Color::Yellow),
            Status::Renamed => ("R", Color::Magenta),
        };
        let base = Style::default().bg(self.palette.header_bg);
        let mut spans = vec![
            Span::styled(format!(" {tag} "), base.fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(f.display_path(), base.add_modifier(Modifier::BOLD)),
        ];
        if f.binary {
            spans.push(Span::styled("  binary", base.fg(self.palette.note_fg)));
        } else {
            spans.push(Span::styled(format!("  +{}", f.added), base.fg(Color::Green)));
            spans.push(Span::styled(format!(" −{}", f.removed), base.fg(Color::Red)));
        }
        let pos = Span::styled(
            format!(" {}/{} ", fi + 1, self.files.len()),
            base.fg(self.palette.note_fg),
        );
        buf.set_style(area, base);
        let line = Line::from(spans);
        let used = buf.set_line(area.x, area.y, &line, area.width).0;
        let pw = pos.width() as u16;
        if area.right() >= used + pw {
            buf.set_span(area.right() - pw, area.y, &pos, pw);
        }
    }

    fn render_line(&mut self, fi: usize, hi: usize, li: usize, area: Rect, buf: &mut Buffer) {
        let p = &self.palette;
        let (line_bg, emph_bg, marker) = match self.files[fi].hunks[hi].lines[li].kind {
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
        let (old_no, new_no, kind) = {
            let l = &self.files[fi].hunks[hi].lines[li];
            (l.old_no, l.new_no, l.kind)
        };
        let gutter = format!(
            "{:>w$} {:>w$} ",
            old_no.map(|n| n.to_string()).unwrap_or_default(),
            new_no.map(|n| n.to_string()).unwrap_or_default(),
        );
        let marker_style = match kind {
            Kind::Add => row_style.fg(Color::Green),
            Kind::Del => row_style.fg(Color::Red),
            Kind::Context => row_style,
        };
        let mut spans = vec![
            Span::styled(gutter, row_style.fg(p.gutter_fg)),
            Span::styled(format!("{marker} "), marker_style.add_modifier(Modifier::BOLD)),
        ];

        // Syntax spans for the line's source file side.
        let path = self.files[fi].path.clone();
        let (side, src_idx) = match kind {
            Kind::Del => (0u8, old_no.unwrap() - 1),
            _ => (1u8, new_no.unwrap() - 1),
        };
        let syntax: Vec<(Style, usize, usize)> = {
            let f = &self.files[fi];
            let src = if side == 0 { &f.old_lines } else { &f.new_lines };
            self.hl.lines((fi, side), &path, src).get(src_idx).cloned().unwrap_or_default()
        };
        let l = &self.files[fi].hunks[hi].lines[li];
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
                let off = seg.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(seg.len());
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

    fn render_status(&self, area: Rect, buf: &mut Buffer) {
        let style = Style::default().add_modifier(Modifier::REVERSED);
        buf.set_style(area, style);
        let left = match &self.message {
            Some(m) => format!(" {m}"),
            None => format!(" {}  vs  {}  ·  {}", self.head_label, self.base_label, self.base_reason),
        };
        let (added, removed) = self.files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let pos = self.current_file().map(|i| i + 1).unwrap_or(0);
        let end = self.end_scroll();
        let pct = if end == 0 { 100 } else { (self.scroll.min(end) * 100) / end };
        let right = format!(
            " {} files  +{added} −{removed}  ·  {pos}/{}  {pct:>3}%  ·  ? help ",
            self.files.len(),
            self.files.len()
        );
        let rw = right.chars().count() as u16;
        let lw = area.width.saturating_sub(rw);
        buf.set_span(area.x, area.y, &Span::styled(left, style), lw);
        if area.width > rw {
            buf.set_span(area.right() - rw, area.y, &Span::styled(right, style), rw);
        }
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer) {
        // (keys, description); an empty keys string starts a section with the description as title.
        const ROWS: &[(&str, &str)] = &[
            ("", "Scroll"),
            ("↓ ↑  j k  C-n C-p", "line"),
            ("PgDn PgUp  Space  C-v M-v", "page"),
            ("C-d C-u", "half page"),
            ("Home End  g G  M-< M->", "top / bottom"),
            ("← →  C-b C-f  h l", "sideways"),
            ("0  C-a", "back to column 0"),
            ("", "Jump"),
            ("n p  ] [  Enter", "next / previous hunk"),
            ("N P  } {  Tab S-Tab", "next / previous file"),
            ("", "Other"),
            ("r  F5", "reload from disk"),
            ("?", "this help"),
            ("q  Esc  C-c", "quit"),
        ];
        let key_w = ROWS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        let desc_w = ROWS.iter().map(|(_, d)| d.chars().count()).max().unwrap_or(0);

        let key_style = Style::default().fg(self.palette.hunk_fg).add_modifier(Modifier::BOLD);
        let title_style = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let dim = Style::default().fg(self.palette.note_fg);

        let mut lines: Vec<Line> = vec![Line::default()];
        for (i, (keys, desc)) in ROWS.iter().enumerate() {
            if keys.is_empty() {
                if i > 0 {
                    lines.push(Line::default());
                }
                lines.push(Line::from(Span::styled(format!("  {desc}"), title_style)));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{keys:<key_w$}"), key_style),
                    Span::raw("   "),
                    Span::raw(desc.to_string()),
                ]));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(format!("  {}", self.summary()), dim)));
        lines.push(Line::from(Span::styled(format!("  {}", self.mode_label()), dim)));
        lines.push(Line::default());

        let inner_w = (2 + key_w + 3 + desc_w + 2).max(self.summary().chars().count() + 4) as u16;
        let w = (inner_w + 2).min(area.width);
        let h = (lines.len() as u16 + 2).min(area.height);
        let rect = Rect::new(
            area.x + (area.width - w) / 2,
            area.y + (area.height - h) / 2,
            w,
            h,
        );
        Clear.render(rect, buf);
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(dim)
                    .title(Line::from(Span::styled(" codesnob ", Style::default().add_modifier(Modifier::BOLD))))
                    .title_bottom(Line::from(Span::styled(" any key to close ", dim)).right_aligned()),
            )
            .render(rect, buf);
    }
}

pub fn run(mut app: App, rx: Receiver<Msg>) -> Result<()> {
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).ok();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f))?;
            match rx.recv() {
                Ok(msg) => app.dispatch(msg),
                Err(_) => return Ok(()),
            }
            // Drain anything else that is already queued before redrawing.
            while let Ok(msg) = rx.try_recv() {
                app.dispatch(msg);
            }
            if app.quit {
                return Ok(());
            }
        }
    })();
    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture).ok();
    ratatui::restore();
    result
}

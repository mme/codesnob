//! The viewer: locations, ranges, keys and the frame around the diff.
//!
//! Every screen is a view of one location: its source (a directory), its
//! range (what two states are compared) and, later, its focus. Snapshots load
//! on a background thread; the screen only ever shows the newest result.

mod commands;
mod diffview;
mod panel;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::git::{self, Range};
use crate::highlight::Highlighter;
use crate::link::Link;
use crate::loader;
use crate::proto::Target;
use crate::watch::{self, Mode, Msg};
use commands::{Action, COMMANDS};
use diffview::DiffView;
use panel::{Outcome, Panel};

/// How long a load may take before the screen says it is loading.
const LOADING_DELAY: Duration = Duration::from_millis(150);

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

/// The parts of the last applied snapshot that the chrome needs.
struct Info {
    head_label: String,
    unborn: bool,
    base: git::Base,
    commits: Vec<git::CommitInfo>,
    commits_truncated: bool,
    dirty: bool,
    /// The range `view` currently shows.
    range: Range,
    commit: Option<git::CommitInfo>,
}

impl Info {
    fn split(snap: git::Snapshot) -> (Info, Vec<git::FileChange>) {
        (
            Info {
                head_label: snap.head_label,
                unborn: snap.unborn,
                base: snap.base,
                commits: snap.commits,
                commits_truncated: snap.commits_truncated,
                dirty: snap.dirty,
                range: snap.range,
                commit: snap.commit,
            },
            snap.files,
        )
    }
}

struct Location {
    id: u64,
    dir: PathBuf,
    base: Option<String>,
    repo: bool,
    /// The range asked for; `info.range` is the one on screen.
    range: Range,
    view: DiffView,
    info: Option<Info>,
    /// Held for its `Drop`, which stops the watcher.
    _watch: Option<watch::Handle>,
    mode: Mode,
    /// The newest load requested; older results are ignored.
    seq: u64,
    pending_since: Option<Instant>,
    error: Option<String>,
    fingerprint: u64,
    /// A reload by hand is in flight, so its result gets reported.
    manual: bool,
}

pub struct App {
    opts: Options,
    palette: Palette,
    hl: Highlighter,
    tx: Sender<Msg>,
    loader: Sender<loader::Request>,
    link: Link,
    linked: bool,
    /// Open locations; the first is the one on screen.
    locations: Vec<Location>,
    next_id: u64,
    panel: Option<Panel>,
    /// Recently chosen panel entries per list, newest first.
    recent: HashMap<panel::Kind, Vec<String>>,
    message: Option<String>,
    body_h: usize,
    pub quit: bool,
}

impl App {
    pub fn new(
        opts: Options,
        palette: Palette,
        hl: Highlighter,
        tx: Sender<Msg>,
        link: Link,
    ) -> Self {
        let loader = loader::spawn(tx.clone());
        Self {
            opts,
            palette,
            hl,
            tx,
            loader,
            link,
            linked: false,
            locations: Vec::new(),
            next_id: 1,
            panel: None,
            recent: HashMap::new(),
            message: None,
            body_h: 1,
            quit: false,
        }
    }

    /// Display a target: a repository, or a placeholder when `dir` is not in one.
    pub fn open(&mut self, target: Target) {
        if let Some(cur) = self.locations.first()
            && cur.dir == target.dir
            && cur.base == target.base
        {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let (watch, mode) = if target.repo {
            let (h, m) = watch::spawn_watcher(target.dir.clone(), self.tx.clone(), id);
            (Some(h), m)
        } else {
            (None, Mode::Off)
        };
        let loc = Location {
            id,
            dir: target.dir.clone(),
            base: target.base.clone(),
            repo: target.repo,
            range: Range::Everything,
            view: DiffView::default(),
            info: None,
            _watch: watch,
            mode,
            seq: 0,
            pending_since: None,
            error: None,
            fingerprint: 0,
            manual: false,
        };
        // One location for now; switching between several comes later.
        self.locations = vec![loc];
        self.panel = None;
        self.message = None;
        self.hl.clear();
        self.link.set_target(target);
        self.request();
    }

    fn cur(&self) -> Option<&Location> {
        self.locations.first()
    }

    fn cur_mut(&mut self) -> Option<&mut Location> {
        self.locations.first_mut()
    }

    /// Ask the loader for the current location's view.
    fn request(&mut self) {
        let Some(loc) = self.locations.first_mut() else {
            return;
        };
        if !loc.repo {
            return;
        }
        loc.seq += 1;
        loc.pending_since.get_or_insert_with(Instant::now);
        let _ = self.loader.send(loader::Request {
            loc: loc.id,
            seq: loc.seq,
            dir: loc.dir.clone(),
            base: loc.base.clone(),
            range: loc.range.clone(),
        });
    }

    fn set_range(&mut self, range: Range) {
        let Some(loc) = self.cur_mut() else { return };
        if loc.range == range {
            return;
        }
        loc.range = range;
        loc.error = None;
        self.request();
    }

    /// Whether a load is in flight, which needs timed redraws.
    pub fn loading(&self) -> bool {
        self.cur().is_some_and(|l| l.pending_since.is_some())
    }

    fn apply_loaded(&mut self, id: u64, seq: u64, result: Result<git::Snapshot, String>) {
        let context = self.opts.context;
        let Some(loc) = self.locations.iter_mut().find(|l| l.id == id) else {
            return;
        };
        if seq != loc.seq {
            return;
        }
        loc.pending_since = None;
        let manual = std::mem::take(&mut loc.manual);
        let snap = match result {
            Ok(snap) => snap,
            Err(e) => {
                let shown = loc.info.as_ref().is_some_and(|i| i.range == loc.range);
                if shown {
                    self.message = Some(format!("refresh failed: {e}"));
                } else {
                    loc.error = Some(e);
                }
                return;
            }
        };
        loc.error = None;
        let fingerprint = snap.fingerprint();
        if fingerprint == loc.fingerprint {
            if manual {
                self.message = Some("no changes since the last load".to_string());
            }
            return;
        }
        let same_range = loc.info.as_ref().is_some_and(|i| i.range == snap.range);
        let anchor = if same_range { loc.view.anchor() } else { None };
        let (info, files) = Info::split(snap);
        loc.fingerprint = fingerprint;
        loc.view.set_files(files, context);
        match anchor {
            Some(a) => loc.view.restore(a),
            None if !same_range => {
                loc.view.scroll = 0;
                loc.view.hscroll = 0;
            }
            None => {}
        }
        loc.info = Some(info);
        self.hl.clear();
        if manual {
            self.message = Some("reloaded".to_string());
        }
    }

    // ---- input -------------------------------------------------------------

    pub fn dispatch(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => self.handle(ev),
            Msg::Changed { loc } => {
                if self.cur().is_some_and(|l| l.id == loc) {
                    self.request();
                }
            }
            Msg::Loaded { loc, seq, result } => self.apply_loaded(loc, seq, *result),
            Msg::Show(target) => self.open(target),
            Msg::Link(up) => self.linked = up,
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != crossterm::event::KeyEventKind::Release => self.key(k),
            Event::Mouse(m) => {
                let delta: isize = match m.kind {
                    MouseEventKind::ScrollDown => 3,
                    MouseEventKind::ScrollUp => -3,
                    _ => return,
                };
                match self.panel.as_mut() {
                    Some(p) => p.move_by(delta.signum()),
                    None => {
                        if let Some(loc) = self.cur_mut() {
                            loc.view.scroll_by(delta);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn panel_rows(&self) -> usize {
        Panel::height_for(self.body_h as u16).saturating_sub(1) as usize
    }

    fn key(&mut self, k: KeyEvent) {
        if self.panel.is_some() {
            let rows = self.panel_rows();
            let outcome = self.panel.as_mut().map(|p| p.key(k, rows));
            match outcome {
                Some(Outcome::Close) => self.panel = None,
                Some(Outcome::Choose(action, key)) => {
                    let kind = self.panel.as_ref().map(|p| p.kind);
                    self.panel = None;
                    if let Some(kind) = kind {
                        let list = self.recent.entry(kind).or_default();
                        list.retain(|k| *k != key);
                        list.insert(0, key);
                    }
                    self.perform(action);
                }
                Some(Outcome::Quit) => self.quit = true,
                Some(Outcome::Stay) | None => {}
            }
            return;
        }

        self.message = None;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let page = self.body_h.saturating_sub(2).max(1) as isize;
        let half = (self.body_h / 2).max(1) as isize;
        let scroll = |app: &mut App, d: isize| {
            if let Some(loc) = app.cur_mut() {
                loc.view.scroll_by(d);
            }
        };
        let hscroll = |app: &mut App, f: &dyn Fn(usize) -> usize| {
            if let Some(loc) = app.cur_mut() {
                loc.view.hscroll = f(loc.view.hscroll);
            }
        };

        match (k.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) | (KeyCode::Char('q'), false, false) => {
                self.perform(Action::Quit)
            }
            (KeyCode::Esc, _, _) => self.back_out(),
            (KeyCode::Char(':'), false, _)
            | (KeyCode::Char('?'), false, _)
            | (KeyCode::Char('k'), true, false) => self.open_palette(),

            (KeyCode::Down, _, _)
            | (KeyCode::Char('j'), false, false)
            | (KeyCode::Char('n'), true, false) => scroll(self, 1),
            (KeyCode::Up, _, _)
            | (KeyCode::Char('k'), false, false)
            | (KeyCode::Char('p'), true, false) => scroll(self, -1),
            (KeyCode::PageDown, _, _)
            | (KeyCode::Char(' '), false, false)
            | (KeyCode::Char('v'), true, false) => scroll(self, page),
            (KeyCode::PageUp, _, _)
            | (KeyCode::Backspace, _, _)
            | (KeyCode::Char('v'), false, true) => scroll(self, -page),
            (KeyCode::Char('d'), true, false) => scroll(self, half),
            (KeyCode::Char('u'), true, false) => scroll(self, -half),

            (KeyCode::Home, _, _)
            | (KeyCode::Char('g'), false, false)
            | (KeyCode::Char('<'), false, true) => self.perform(Action::Top),
            (KeyCode::End, _, _)
            | (KeyCode::Char('G'), false, false)
            | (KeyCode::Char('>'), false, true) => self.perform(Action::Bottom),

            (KeyCode::Char('n'), false, false)
            | (KeyCode::Char(']'), false, false)
            | (KeyCode::Enter, _, _) => self.perform(Action::NextHunk),
            (KeyCode::Char('p'), false, false) | (KeyCode::Char('['), false, false) => {
                self.perform(Action::PrevHunk)
            }
            (KeyCode::Char('N'), false, false)
            | (KeyCode::Char('}'), false, false)
            | (KeyCode::Tab, _, _) => self.perform(Action::NextFile),
            (KeyCode::Char('P'), false, false)
            | (KeyCode::Char('{'), false, false)
            | (KeyCode::BackTab, _, _) => self.perform(Action::PrevFile),

            (KeyCode::Right, _, _)
            | (KeyCode::Char('f'), true, false)
            | (KeyCode::Char('l'), false, false) => hscroll(self, &|h| h + 8),
            (KeyCode::Left, _, _)
            | (KeyCode::Char('b'), true, false)
            | (KeyCode::Char('h'), false, false) => hscroll(self, &|h| h.saturating_sub(8)),
            (KeyCode::Char('0'), false, false) | (KeyCode::Char('a'), true, false) => {
                hscroll(self, &|_| 0)
            }

            (KeyCode::Char('r'), false, false) | (KeyCode::F(5), _, _) => {
                self.perform(Action::Reload)
            }
            _ => {}
        }
    }

    /// Esc: leave the narrowest thing first, and never quit.
    fn back_out(&mut self) {
        if self.cur().is_some_and(|l| l.range != Range::Everything) {
            self.set_range(Range::Everything);
        }
    }

    fn applicable(&self, action: Action) -> bool {
        let Some(loc) = self.cur() else {
            return action == Action::Quit;
        };
        let has_rows = !loc.view.is_empty();
        let has_base = loc
            .info
            .as_ref()
            .is_some_and(|i| i.base.name.is_some() && !i.unborn);
        match action {
            Action::NextHunk
            | Action::PrevHunk
            | Action::NextFile
            | Action::PrevFile
            | Action::Top
            | Action::Bottom => has_rows,
            Action::RangeEverything => loc.repo && loc.range != Range::Everything,
            Action::RangeUncommitted => loc.repo && has_base && loc.range != Range::Uncommitted,
            Action::ExplainBase => loc.info.is_some(),
            Action::Reload => loc.repo,
            Action::Quit => true,
        }
    }

    fn open_palette(&mut self) {
        let items = COMMANDS
            .iter()
            .filter(|c| self.applicable(c.action))
            .map(|c| panel::Item {
                text: c.name.to_string(),
                hint: c.keys.to_string(),
                action: c.action,
                key: c.name.to_string(),
            })
            .collect();
        let recent = self
            .recent
            .get(&panel::Kind::Commands)
            .cloned()
            .unwrap_or_default();
        self.panel = Some(Panel::new(panel::Kind::Commands, items, recent));
    }

    fn perform(&mut self, action: Action) {
        let body_h = self.body_h;
        match action {
            Action::Quit => self.quit = true,
            Action::RangeEverything => self.set_range(Range::Everything),
            Action::RangeUncommitted => self.set_range(Range::Uncommitted),
            Action::ExplainBase => {
                if let Some(info) = self.cur().and_then(|l| l.info.as_ref()) {
                    self.message = Some(format!("Base: {}.", info.base.reason));
                }
            }
            Action::Reload => {
                if let Some(loc) = self.cur_mut()
                    && loc.repo
                {
                    loc.manual = true;
                    self.message = Some("reloading…".to_string());
                    self.request();
                }
            }
            nav => {
                let Some(loc) = self.cur_mut() else { return };
                let v = &mut loc.view;
                match nav {
                    Action::NextHunk => v.next_hunk(),
                    Action::PrevHunk => v.prev_hunk(),
                    Action::NextFile => v.next_file(),
                    Action::PrevFile => v.prev_file(),
                    Action::Top => v.scroll = 0,
                    Action::Bottom => v.scroll = v.end_scroll(body_h),
                    _ => {}
                }
            }
        }
    }

    // ---- words ---------------------------------------------------------------

    /// What the screen shows, in words: the left side of the status bar.
    fn scope(&self) -> String {
        let Some(loc) = self.cur() else {
            return String::new();
        };
        if !loc.repo {
            return format!("no repository · {}", loc.dir.display());
        }
        let Some(info) = &loc.info else {
            return loc.dir.display().to_string();
        };
        let head = &info.head_label;
        if info.unborn {
            return format!("{head} · no commits yet");
        }
        match &loc.range {
            Range::Everything => match &info.base.name {
                None if info.dirty => format!("{head} · uncommitted changes"),
                None => format!("{head} · no changes"),
                Some(base) => {
                    let n = info.commits.len();
                    let mut parts = Vec::new();
                    if n > 0 {
                        let more = if info.commits_truncated { "+" } else { "" };
                        let noun = if n == 1 && more.is_empty() {
                            "commit"
                        } else {
                            "commits"
                        };
                        parts.push(format!("{n}{more} {noun}"));
                    }
                    if info.dirty {
                        parts.push("uncommitted work".to_string());
                    }
                    if parts.is_empty() {
                        format!("{head} · no changes since {base}")
                    } else {
                        format!("{head} · {} since {base}", parts.join(" and "))
                    }
                }
            },
            Range::Uncommitted => format!("{head} · uncommitted changes"),
            Range::Commit(id) => {
                let n = info.commits.len();
                match info.commits.iter().position(|c| c.id == *id) {
                    Some(i) => {
                        let c = &info.commits[i];
                        format!("{} {} · commit {} of {n}", c.short, c.subject, i + 1)
                    }
                    None => match &info.commit {
                        Some(c) if c.id == *id => format!("{} {}", c.short, c.subject),
                        _ => format!("{head} · commit"),
                    },
                }
            }
        }
    }

    /// The centered text shown instead of a diff, if any.
    fn notice(&self) -> Option<Vec<String>> {
        let loc = self.cur()?;
        if !loc.repo {
            return Some(vec![
                "not inside a git repository".to_string(),
                loc.dir.display().to_string(),
            ]);
        }
        if let Some(e) = &loc.error {
            return Some(vec![e.clone()]);
        }
        let stale = loc.info.as_ref().is_none_or(|i| i.range != loc.range);
        if stale {
            let slow = loc
                .pending_since
                .is_some_and(|t| t.elapsed() >= LOADING_DELAY);
            return slow.then(|| vec!["Loading…".to_string()]);
        }
        if !loc.view.is_empty() {
            return None;
        }
        let info = loc.info.as_ref()?;
        Some(vec![match (&loc.range, &info.base.name) {
            (Range::Everything, Some(base)) => format!("no changes since {base}"),
            (Range::Everything, None) => "no changes".to_string(),
            (Range::Uncommitted, _) => "no uncommitted changes".to_string(),
            (Range::Commit(_), _) => "this commit changes no files".to_string(),
        }])
    }

    fn mode_label(&self) -> String {
        let refresh = match self.cur().map(|l| l.mode) {
            Some(Mode::Watching) => "auto-refresh on",
            Some(Mode::Polling) => "polling every 2s",
            _ => "auto-refresh off",
        };
        let link = if self.linked {
            "daemon connected"
        } else {
            "daemon offline"
        };
        format!("{refresh} · {link}")
    }

    // ---- rendering -----------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height < 2 || area.width < 4 {
            return;
        }
        let status = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let mut body = Rect::new(area.x, area.y, area.width, area.height - 1);
        let panel_h = if self.panel.is_some() {
            Panel::height_for(body.height)
        } else {
            0
        };
        let panel_area = Rect::new(body.x, body.bottom() - panel_h, body.width, panel_h);
        body.height -= panel_h;
        self.body_h = body.height as usize;
        if let Some(loc) = self.locations.first_mut() {
            loc.view.clamp();
        }

        let info = self.mode_label();
        let buf = frame.buffer_mut();
        match self.notice() {
            Some(lines) => self.render_notice(&lines, body, buf),
            None => {
                if let Some(loc) = self.locations.first() {
                    loc.view.render(body, buf, &self.palette, &mut self.hl);
                }
            }
        }
        self.render_status(status, buf);
        let cursor = self
            .panel
            .as_mut()
            .map(|p| p.render(panel_area, buf, &self.palette, &info));
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }

    fn render_notice(&self, lines: &[String], body: Rect, buf: &mut Buffer) {
        let top = body.y + body.height.saturating_sub(lines.len() as u16) / 2;
        for (i, text) in lines.iter().enumerate() {
            let line = Line::from(Span::styled(
                text.clone(),
                Style::default().fg(self.palette.note_fg),
            ));
            let w = (line.width() as u16).min(body.width);
            buf.set_line(body.x + (body.width - w) / 2, top + i as u16, &line, w);
        }
    }

    fn render_status(&self, area: Rect, buf: &mut Buffer) {
        let style = Style::default().add_modifier(Modifier::REVERSED);
        buf.set_style(area, style);
        let left = format!(" {}", self.scope());
        let mut right = String::new();
        if let Some(m) = &self.message {
            // A message gets the whole line.
            buf.set_span(
                area.x,
                area.y,
                &Span::styled(format!(" {m}"), style),
                area.width,
            );
            return;
        }
        if let Some(loc) = self.cur()
            && !loc.view.is_empty()
        {
            let (added, removed) = loc.view.totals();
            let n = loc.view.files.len();
            let pos = loc.view.current_file().map(|i| i + 1).unwrap_or(0);
            let noun = if n == 1 { "file" } else { "files" };
            right = format!(" {n} {noun}  +{added} −{removed}  ·  {pos}/{n}  ·");
        }
        right.push_str(" ? commands ");
        let rw = right.chars().count() as u16;
        let lw = area.width.saturating_sub(rw);
        buf.set_span(area.x, area.y, &Span::styled(left, style), lw);
        if area.width > rw {
            buf.set_span(area.right() - rw, area.y, &Span::styled(right, style), rw);
        }
    }
}

pub fn run(mut app: App, rx: Receiver<Msg>) -> Result<()> {
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).ok();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f))?;
            // While a load is in flight, wake up to show "Loading…" in time.
            let msg = if app.loading() {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return Ok(()),
                }
            };
            if let Some(m) = msg {
                app.dispatch(m);
            }
            // Drain anything else that is already queued before redrawing.
            while let Ok(m) = rx.try_recv() {
                app.dispatch(m);
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

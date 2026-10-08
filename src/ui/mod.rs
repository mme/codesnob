//! The viewer: locations, ranges, keys and the frame around the diff.
//!
//! Every screen is a view of one location: its source (a directory), its
//! range (what two states are compared) and, later, its focus. Snapshots load
//! on a background thread; the screen only ever shows the newest result.

mod commands;
mod diffview;
mod markdown;
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
use diffview::{Anchor, DiffView};
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
    /// Where the recent-locations list is kept; `None` keeps nothing.
    pub state_dir: Option<PathBuf>,
}

/// How long the location strip stays after the last `-`.
const FLIP_WINDOW: Duration = Duration::from_millis(1000);

/// A run of `-` presses: each goes one further back in `order`, the
/// most-recent order as it was at the first press.
struct Flip {
    order: Vec<u64>,
    depth: usize,
    last: Instant,
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

/// What part of the range is on screen.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Focus {
    All,
    /// One file, by path.
    File(String),
}

/// Where the view was when a previewing list opened, to return to on Esc.
struct Origin {
    range: Range,
    focus: Focus,
    anchor: Option<Anchor>,
}

struct Location {
    id: u64,
    dir: PathBuf,
    base: Option<String>,
    repo: bool,
    /// The range asked for; `info.range` is the one on screen.
    range: Range,
    focus: Focus,
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
    /// Restore this position once the requested range has loaded.
    pending_anchor: Option<Anchor>,
    /// The description block must be rebuilt before the next draw.
    desc_dirty: bool,
    /// The width the description block was wrapped at.
    desc_width: u16,
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
    /// Where to return when a previewing list is closed with Esc.
    origin: Option<Origin>,
    /// Recently chosen panel entries per list, newest first.
    recent: HashMap<panel::Kind, Vec<String>>,
    message: Option<String>,
    body_h: usize,
    /// Commit messages are shown above a commit's diff unless turned off.
    show_commit_desc: bool,
    flip: Option<Flip>,
    /// How long the location strip stays after the last `-`.
    flip_window: Duration,
    /// The recent entries the Open list was built from.
    open_choices: Vec<crate::recent::Entry>,
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
            origin: None,
            recent: HashMap::new(),
            message: None,
            body_h: 1,
            show_commit_desc: true,
            flip: None,
            flip_window: FLIP_WINDOW,
            open_choices: Vec::new(),
            quit: false,
        }
    }

    /// Display a target: switch to it if this viewer has it open, else open
    /// it as a new location. Not being in a repository shows a placeholder.
    pub fn open(&mut self, target: Target) {
        if let Some(i) = self
            .locations
            .iter()
            .position(|l| l.dir == target.dir && l.base == target.base)
        {
            self.switch_to(i);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let loc = Location {
            id,
            dir: target.dir.clone(),
            base: target.base.clone(),
            repo: target.repo,
            range: Range::Everything,
            focus: Focus::All,
            view: DiffView::default(),
            info: None,
            _watch: None,
            mode: Mode::Off,
            seq: 0,
            pending_since: None,
            error: None,
            fingerprint: 0,
            manual: false,
            pending_anchor: None,
            desc_dirty: true,
            desc_width: 0,
        };
        if let Some(state) = &self.opts.state_dir {
            crate::recent::record(state, &target.dir, target.base.as_deref());
        }
        self.deactivate();
        self.locations.insert(0, loc);
        self.activate();
    }

    /// Bring location `i` to the front, as it was left.
    fn switch_to(&mut self, i: usize) {
        if i == 0 || i >= self.locations.len() {
            return;
        }
        self.deactivate();
        let loc = self.locations.remove(i);
        self.locations.insert(0, loc);
        self.activate();
    }

    /// Only the location on screen watches its files.
    fn deactivate(&mut self) {
        if let Some(loc) = self.locations.first_mut() {
            loc._watch = None;
            loc.mode = Mode::Off;
        }
    }

    /// Start watching the front location and bring it up to date.
    fn activate(&mut self) {
        self.panel = None;
        self.origin = None;
        self.hl.clear();
        let tx = self.tx.clone();
        let Some(loc) = self.locations.first_mut() else {
            return;
        };
        loc.desc_dirty = true;
        if loc.repo {
            loc._watch = Some(watch::spawn_watcher(loc.dir.clone(), tx, loc.id));
        }
        let target = Target {
            dir: loc.dir.clone(),
            base: loc.base.clone(),
            repo: loc.repo,
        };
        self.link.set_target(target);
        self.request();
    }

    fn close_location(&mut self) {
        if self.locations.len() < 2 {
            return;
        }
        self.deactivate();
        self.locations.remove(0);
        self.activate();
    }

    /// `-`: flip to the previous location; quick repeats go further back.
    fn flip(&mut self) {
        if self.locations.len() < 2 {
            self.message = Some("no other location open; o opens one".to_string());
            return;
        }
        let flip = match self.flip.take() {
            Some(f) if f.last.elapsed() < self.flip_window => Flip {
                depth: f.depth + 1,
                last: Instant::now(),
                order: f.order,
            },
            _ => Flip {
                order: self.locations.iter().map(|l| l.id).collect(),
                depth: 1,
                last: Instant::now(),
            },
        };
        let target = flip.order[flip.depth % flip.order.len()];
        if let Some(i) = self.locations.iter().position(|l| l.id == target) {
            self.switch_to(i);
        }
        self.flip = Some(flip);
    }

    fn flipping(&self) -> bool {
        self.flip
            .as_ref()
            .is_some_and(|f| f.last.elapsed() < self.flip_window)
    }

    /// Whether the screen must be redrawn on a timer: a load is in flight,
    /// or the location strip is waiting to disappear.
    pub fn needs_tick(&self) -> bool {
        self.loading() || self.flipping()
    }

    /// Open what the Open prompt resolved to.
    fn open_dir(&mut self, dir: PathBuf, base: Option<String>) {
        let repo = gix::discover(&dir).is_ok();
        self.open(Target { dir, base, repo });
    }

    /// A typed path: `~` expanded, relative to the current location.
    fn resolve_path(&self, input: &str) -> Result<PathBuf, String> {
        let path = self.expand(input);
        let canon =
            std::fs::canonicalize(&path).map_err(|_| format!("no such directory: {input}"))?;
        if canon.is_dir() {
            Ok(canon)
        } else {
            canon
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| format!("no such directory: {input}"))
        }
    }

    fn expand(&self, input: &str) -> PathBuf {
        let input = input.trim();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = match (input.strip_prefix('~'), &home) {
            (Some(rest), Some(home)) => home.join(rest.trim_start_matches('/')),
            _ => PathBuf::from(input),
        };
        if path.is_absolute() {
            return path;
        }
        let base = self
            .cur()
            .map(|l| l.dir.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        base.join(path)
    }

    /// Tab in the Open prompt: complete the directory name being typed.
    fn complete(&self, input: &str) -> Option<String> {
        let (dir_part, prefix) = match input.rfind('/') {
            Some(i) => (&input[..=i], &input[i + 1..]),
            None => ("", input),
        };
        let base = if dir_part.is_empty() {
            self.expand(".")
        } else {
            self.expand(dir_part)
        };
        let mut names: Vec<String> = std::fs::read_dir(base)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.')))
            .collect();
        names.sort();
        let first = names.first()?;
        if names.len() == 1 {
            return Some(format!("{dir_part}{first}/"));
        }
        let common = names.iter().fold(first.clone(), |acc, n| {
            acc.chars()
                .zip(n.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect()
        });
        (common.len() > prefix.len()).then(|| format!("{dir_part}{common}"))
    }

    fn open_prompt(&mut self) {
        let list = self
            .opts
            .state_dir
            .as_deref()
            .map(crate::recent::load)
            .unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let items = list
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let open = self
                    .locations
                    .iter()
                    .any(|l| l.dir == e.dir && l.base == e.base);
                panel::Item {
                    tag: None,
                    text: tilde(&e.dir, home.as_deref()),
                    hint: if open {
                        "open".to_string()
                    } else {
                        ago(e.used as i64)
                    },
                    action: Action::OpenRecent(i),
                    key: e.dir.display().to_string(),
                }
            })
            .collect();
        self.open_choices = list;
        self.panel = Some(Panel::new(panel::Kind::Open, items, Vec::new(), None));
    }

    fn open_locations(&mut self) {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let items = self
            .locations
            .iter()
            .map(|l| panel::Item {
                tag: None,
                text: format!("{}  {}", name_of(&l.dir), tilde(&l.dir, home.as_deref())),
                hint: self.location_summary(l),
                action: Action::SwitchLocation(l.id),
                key: l.id.to_string(),
            })
            .collect();
        let previous = self.locations.get(1).map(|l| l.id.to_string());
        self.panel = Some(Panel::new(
            panel::Kind::Locations,
            items,
            Vec::new(),
            previous.as_deref(),
        ));
    }

    /// "feature · commit 2 of 3": a location's branch and range.
    fn location_summary(&self, l: &Location) -> String {
        if !l.repo {
            return "no repository".to_string();
        }
        let Some(info) = &l.info else {
            return String::new();
        };
        let range = match l.range {
            Range::Everything => "everything".to_string(),
            Range::Uncommitted => "uncommitted".to_string(),
            Range::Commit(id) => match info.commits.iter().position(|c| c.id == id) {
                Some(i) => format!("commit {} of {}", i + 1, info.commits.len()),
                None => id.to_hex_with_len(7).to_string(),
            },
        };
        format!("{} · {range}", info.head_label)
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
            range: loc.range,
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
        // The commit being viewed left the branch (amend, rebase): fall back.
        if let Range::Commit(id) = snap.range
            && !snap.commits.iter().any(|c| c.id == id)
            && loc
                .info
                .as_ref()
                .is_some_and(|i| i.commits.iter().any(|c| c.id == id))
        {
            let short = id.to_hex_with_len(7).to_string();
            self.message = Some(format!("{short} is no longer on this branch"));
            loc.range = Range::Everything;
            self.request();
            return;
        }
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
        if let Focus::File(path) = &loc.focus
            && !loc.view.narrow_to_path(Some(path))
        {
            // While a list previews, a file missing from one commit is not news.
            if self.panel.is_none() {
                self.message = Some(format!("{path} has no changes here; showing all files"));
                loc.focus = Focus::All;
            }
        }
        match anchor.or(loc.pending_anchor.take()) {
            Some(a) => loc.view.restore(a),
            None if !same_range => {
                loc.view.scroll = 0;
                loc.view.hscroll = 0;
            }
            None => {}
        }
        loc.info = Some(info);
        loc.desc_dirty = true;
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
            Msg::Watching { loc, mode } => {
                if let Some(l) = self.locations.iter_mut().find(|l| l.id == loc)
                    && l._watch.is_some()
                {
                    l.mode = mode;
                }
            }
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != crossterm::event::KeyEventKind::Release => self.key(k),
            Event::Paste(text) => {
                if let Some(p) = self.panel.as_mut() {
                    p.paste(&text);
                }
            }
            Event::Mouse(m) => {
                let delta: isize = match m.kind {
                    MouseEventKind::ScrollDown => 3,
                    MouseEventKind::ScrollUp => -3,
                    _ => return,
                };
                match self.panel.as_mut() {
                    Some(p) => {
                        let before = p.selected_action();
                        p.move_by(delta.signum());
                        self.preview(before);
                    }
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
            let before = self.panel.as_ref().and_then(|p| p.selected_action());
            let outcome = self.panel.as_mut().map(|p| p.key(k, rows));
            match outcome {
                Some(Outcome::Close) => self.close_panel(true),
                Some(Outcome::Choose(action, key)) => {
                    let kind = self.panel.as_ref().map(|p| p.kind);
                    self.close_panel(false);
                    if let Some(kind) = kind {
                        let list = self.recent.entry(kind).or_default();
                        list.retain(|k| *k != key);
                        list.insert(0, key);
                    }
                    self.perform(action);
                }
                Some(Outcome::Submit { input, selected }) => self.submit_open(&input, selected),
                Some(Outcome::Complete) => {
                    let input = self.panel.as_ref().map(|p| p.input().to_string());
                    if let Some(done) = input.and_then(|i| self.complete(&i))
                        && let Some(p) = self.panel.as_mut()
                    {
                        p.set_input(done);
                    }
                }
                Some(Outcome::Quit) => self.quit = true,
                Some(Outcome::Stay) | None => self.preview(before),
            }
            return;
        }

        self.message = None;
        if k.code != KeyCode::Char('-') {
            self.flip = None;
        }
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
            (KeyCode::Char('f'), false, false) => self.perform(Action::Files),
            (KeyCode::Char('o'), false, false) => self.perform(Action::Open),
            (KeyCode::Char('-'), false, false) => self.perform(Action::PreviousLocation),
            (KeyCode::Char('c'), false, false) => self.perform(Action::Commits),
            (KeyCode::Char('<'), false, false) => self.perform(Action::OlderCommit),
            (KeyCode::Char('>'), false, false) => self.perform(Action::NewerCommit),
            (KeyCode::Char('d'), false, false) => self.perform(Action::ToggleDescription),
            (KeyCode::Char('z'), false, false) => self.perform(Action::Narrow),

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
        let Some(loc) = self.cur() else { return };
        if loc.focus != Focus::All {
            self.narrow(None);
        } else if loc.range != Range::Everything {
            self.set_range(Range::Everything);
        }
    }

    /// Narrow to `path`, or show all files with `None`, keeping the line in view.
    fn narrow(&mut self, path: Option<String>) {
        let Some(loc) = self.cur_mut() else { return };
        let anchor = loc.view.anchor();
        // Stay on the same line when it is still on screen afterwards: always
        // when showing all again, and when narrowing to the file already in view.
        let current = loc.view.current_file().map(|fi| &loc.view.files[fi].path);
        let keep = path.is_none() || current == path.as_ref();
        if !loc.view.narrow_to_path(path.as_deref()) {
            return;
        }
        loc.focus = match path {
            Some(p) => Focus::File(p),
            None => Focus::All,
        };
        loc.desc_dirty = true;
        match anchor {
            Some(a) if keep => loc.view.restore(a),
            _ => loc.view.scroll = 0,
        }
    }

    /// Move to file `fi`: jump in the full view, switch file while narrowed.
    fn go_to_file(&mut self, fi: usize) {
        let Some(loc) = self.cur_mut() else { return };
        let Some(path) = loc.view.files.get(fi).map(|f| f.path.clone()) else {
            return;
        };
        if loc.focus == Focus::All {
            loc.view.scroll_to_file(fi);
        } else {
            self.narrow(Some(path));
        }
    }

    /// Show what a previewing list has highlighted, if it changed.
    fn preview(&mut self, before: Option<Action>) {
        let Some(panel) = &self.panel else { return };
        if !panel.kind.previews() {
            return;
        }
        let now = panel.selected_action();
        if now == before {
            return;
        }
        match now {
            Some(Action::GoToFile(fi)) => self.go_to_file(fi),
            Some(Action::RangeEverything) => self.set_range(Range::Everything),
            Some(Action::RangeUncommitted) => self.set_range(Range::Uncommitted),
            Some(Action::ShowCommit(id)) => self.set_range(Range::Commit(id)),
            _ => {}
        }
    }

    /// Close the panel; with `revert`, undo whatever it previewed.
    fn close_panel(&mut self, revert: bool) {
        self.panel = None;
        let Some(origin) = self.origin.take() else {
            return;
        };
        if !revert {
            return;
        }
        let Some(loc) = self.cur_mut() else { return };
        loc.focus = origin.focus;
        loc.desc_dirty = true;
        if loc.range != origin.range {
            // The old range reloads; its position comes back when it lands.
            loc.range = origin.range;
            loc.pending_anchor = origin.anchor;
            self.request();
            return;
        }
        let path = match &loc.focus {
            Focus::File(p) => Some(p.clone()),
            Focus::All => None,
        };
        loc.view.narrow_to_path(path.as_deref());
        if let Some(a) = origin.anchor {
            loc.view.restore(a);
        }
    }

    /// Enter in the Open prompt: a typed path that exists wins, else the
    /// highlighted recent location.
    fn submit_open(&mut self, input: &str, selected: Option<Action>) {
        let input = input.trim();
        let typed = (!input.is_empty()).then(|| self.resolve_path(input));
        match (typed, selected) {
            (Some(Ok(dir)), _) => {
                self.close_panel(false);
                self.open_dir(dir, None);
            }
            (_, Some(action)) => {
                self.close_panel(false);
                self.perform(action);
            }
            (Some(Err(e)), None) => {
                if let Some(p) = self.panel.as_mut() {
                    p.note = Some(e);
                }
            }
            (None, None) => self.close_panel(false),
        }
    }

    fn open_commits(&mut self) {
        let Some(loc) = self.cur() else { return };
        let Some(info) = &loc.info else { return };
        let mut items = Vec::new();
        if let Some(base) = &info.base.name {
            let n = info.commits.len();
            items.push(panel::Item {
                tag: None,
                text: format!("Everything since {base}"),
                hint: format!(
                    "{n}{} commit{}",
                    if info.commits_truncated { "+" } else { "" },
                    if n == 1 { "" } else { "s" }
                ),
                action: Action::RangeEverything,
                key: "everything".to_string(),
            });
        }
        for c in &info.commits {
            items.push(panel::Item {
                tag: Some((c.short.clone(), self.palette.hunk_fg)),
                text: c.subject.clone(),
                hint: ago(c.time),
                action: Action::ShowCommit(c.id),
                key: c.id.to_string(),
            });
        }
        if info.dirty {
            items.push(panel::Item {
                tag: None,
                text: "Uncommitted changes".to_string(),
                hint: String::new(),
                action: Action::RangeUncommitted,
                key: "uncommitted".to_string(),
            });
        }
        let note = if info.commits_truncated {
            Some(format!("newest {} shown", git::MAX_COMMITS))
        } else if info.base.name.is_none() {
            Some("no base branch, so only uncommitted changes".to_string())
        } else if info.commits.is_empty() {
            Some("no commits since the branch point".to_string())
        } else {
            None
        };
        let current = match loc.range {
            Range::Everything if info.base.name.is_some() => "everything".to_string(),
            Range::Everything | Range::Uncommitted => "uncommitted".to_string(),
            Range::Commit(id) => id.to_string(),
        };
        self.origin = Some(Origin {
            range: loc.range,
            focus: loc.focus.clone(),
            anchor: loc.view.anchor(),
        });
        let mut panel = Panel::new(panel::Kind::Commits, items, Vec::new(), Some(&current));
        panel.note = note;
        self.panel = Some(panel);
    }

    /// The steps `<` and `>` walk: the commits oldest first, then the
    /// uncommitted changes when there are any.
    fn steps(&self) -> Vec<Range> {
        let Some(info) = self.cur().and_then(|l| l.info.as_ref()) else {
            return Vec::new();
        };
        let mut steps: Vec<Range> = info.commits.iter().map(|c| Range::Commit(c.id)).collect();
        if info.dirty {
            steps.push(Range::Uncommitted);
        }
        steps
    }

    fn step(&mut self, older: bool) {
        let steps = self.steps();
        let Some(loc) = self.cur() else { return };
        if steps.is_empty() {
            self.message = Some("no commits since the branch point".to_string());
            return;
        }
        let at = steps.iter().position(|r| *r == loc.range);
        let next = match (at, older) {
            (None, true) => steps.len() - 1,
            (None, false) => 0,
            (Some(0), true) => {
                self.message = Some("this is the oldest commit".to_string());
                return;
            }
            (Some(i), true) => i - 1,
            (Some(i), false) if i + 1 == steps.len() => {
                self.message = Some("nothing newer".to_string());
                return;
            }
            (Some(i), false) => i + 1,
        };
        self.set_range(steps[next]);
    }

    /// The description block for the current view, wrapped to `width`.
    fn description(&self, width: u16) -> Vec<Line<'static>> {
        let Some(loc) = self.cur() else {
            return Vec::new();
        };
        if loc.focus != Focus::All || !self.show_commit_desc {
            return Vec::new();
        }
        let Some(info) = &loc.info else {
            return Vec::new();
        };
        let (Range::Commit(id), Some(c)) = (info.range, &info.commit) else {
            return Vec::new();
        };
        if c.id != id {
            return Vec::new();
        }
        let width = width.saturating_sub(6) as usize;
        let dim = Style::default().fg(self.palette.note_fg);
        let mut out = vec![Line::from(Span::styled(
            format!("{} · {} · {}", c.short, c.author, ago(c.time)),
            dim,
        ))];
        let mut title = markdown::render(
            &format!("**{}**", c.subject.replace('*', "\\*")),
            width,
            &self.palette,
        );
        out.append(&mut title);
        let body = c
            .message
            .split_once('\n')
            .map(|(_, b)| b.trim())
            .unwrap_or("");
        if !body.is_empty() {
            out.push(Line::default());
            out.extend(markdown::render(body, width, &self.palette));
        }
        out
    }

    fn open_files(&mut self) {
        let Some(loc) = self.cur() else { return };
        let items = loc
            .view
            .files
            .iter()
            .enumerate()
            .map(|(fi, f)| {
                let (tag, color) = match f.status {
                    crate::diff::Status::Added => ("A", Color::Green),
                    crate::diff::Status::Deleted => ("D", Color::Red),
                    crate::diff::Status::Modified => ("M", Color::Yellow),
                    crate::diff::Status::Renamed => ("R", Color::Magenta),
                };
                panel::Item {
                    tag: Some((tag.to_string(), color)),
                    text: f.display_path(),
                    hint: if f.binary {
                        "binary".to_string()
                    } else {
                        format!("+{} −{}", f.added, f.removed)
                    },
                    action: Action::GoToFile(fi),
                    key: f.path.clone(),
                }
            })
            .collect();
        let current = loc
            .view
            .current_file()
            .map(|fi| loc.view.files[fi].path.clone());
        self.origin = Some(Origin {
            range: loc.range,
            focus: loc.focus.clone(),
            anchor: loc.view.anchor(),
        });
        self.panel = Some(Panel::new(
            panel::Kind::Files,
            items,
            Vec::new(),
            current.as_deref(),
        ));
    }

    fn applicable(&self, action: Action) -> bool {
        let Some(loc) = self.cur() else {
            return matches!(action, Action::Quit | Action::Open);
        };
        let has_rows = loc.view.has_files();
        let has_base = loc
            .info
            .as_ref()
            .is_some_and(|i| i.base.name.is_some() && !i.unborn);
        let has_info = loc.info.is_some();
        let several = self.locations.len() > 1;
        match action {
            Action::Open | Action::OpenRecent(_) => true,
            Action::Locations | Action::SwitchLocation(_) => several,
            Action::PreviousLocation | Action::CloseLocation => several,
            Action::Commits => has_info,
            Action::OlderCommit | Action::NewerCommit => loc
                .info
                .as_ref()
                .is_some_and(|i| !i.commits.is_empty() || i.dirty),
            Action::ShowCommit(_) => loc.repo,
            Action::ToggleDescription => matches!(loc.range, Range::Commit(_)),
            Action::Files | Action::Narrow | Action::GoToFile(_) => has_rows,
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
            .map(|c| {
                let narrowed = self.cur().is_some_and(|l| l.focus != Focus::All);
                let name = match c.action {
                    Action::Narrow if narrowed => "Show all files",
                    _ => c.name,
                };
                panel::Item {
                    tag: None,
                    text: name.to_string(),
                    hint: c.keys.to_string(),
                    action: c.action,
                    key: c.name.to_string(),
                }
            })
            .collect();
        let recent = self
            .recent
            .get(&panel::Kind::Commands)
            .cloned()
            .unwrap_or_default();
        self.panel = Some(Panel::new(panel::Kind::Commands, items, recent, None));
    }

    fn perform(&mut self, action: Action) {
        let body_h = self.body_h;
        match action {
            Action::Quit => self.quit = true,
            Action::Files => self.open_files(),
            Action::Open => self.open_prompt(),
            Action::OpenRecent(i) => {
                if let Some(e) = self.open_choices.get(i).cloned() {
                    if e.dir.is_dir() {
                        self.open_dir(e.dir, e.base);
                    } else {
                        self.message = Some(format!("{} no longer exists", e.dir.display()));
                    }
                }
            }
            Action::Locations => self.open_locations(),
            Action::SwitchLocation(id) => {
                if let Some(i) = self.locations.iter().position(|l| l.id == id) {
                    self.switch_to(i);
                }
            }
            Action::PreviousLocation => self.flip(),
            Action::CloseLocation => self.close_location(),
            Action::Commits => self.open_commits(),
            Action::OlderCommit => self.step(true),
            Action::NewerCommit => self.step(false),
            Action::ShowCommit(id) => self.set_range(Range::Commit(id)),
            Action::ToggleDescription => {
                self.show_commit_desc = !self.show_commit_desc;
                if let Some(loc) = self.cur_mut() {
                    loc.desc_dirty = true;
                }
            }
            Action::GoToFile(fi) => self.go_to_file(fi),
            Action::Narrow => {
                let Some(loc) = self.cur() else { return };
                if loc.focus != Focus::All {
                    self.narrow(None);
                } else if let Some(fi) = loc.view.current_file() {
                    let path = loc.view.files[fi].path.clone();
                    self.narrow(Some(path));
                }
            }
            Action::NextFile | Action::PrevFile
                if self.cur().is_some_and(|l| l.focus != Focus::All) =>
            {
                // While narrowed, next and previous file move the narrowing.
                let Some(loc) = self.cur() else { return };
                let Some(fi) = loc.view.current_file() else {
                    return;
                };
                let next = if action == Action::NextFile {
                    fi + 1
                } else {
                    fi.wrapping_sub(1)
                };
                if next < loc.view.files.len() {
                    self.go_to_file(next);
                }
            }
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
        let scope = self.range_scope();
        match self.cur().map(|l| &l.focus) {
            // Narrowed first: on a narrow terminal the scope is what gets cut.
            Some(Focus::File(path)) => format!("{path} only · Esc for all · {scope}"),
            _ => scope,
        }
    }

    fn range_scope(&self) -> String {
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
        let rebuild = self
            .cur()
            .is_some_and(|l| l.desc_dirty || l.desc_width != body.width);
        if rebuild {
            let lines = self.description(body.width);
            if let Some(loc) = self.locations.first_mut() {
                loc.view.set_description(lines);
                loc.desc_dirty = false;
                loc.desc_width = body.width;
            }
        }
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
        if let Some(flip) = self.flip.as_ref().filter(|_| self.flipping()) {
            // The location strip: where `-` has taken you, among the others.
            let target = flip.order[flip.depth % flip.order.len()];
            let mut spans = vec![Span::styled(" ", style)];
            for (n, id) in flip.order.iter().enumerate() {
                let Some(l) = self.locations.iter().find(|l| l.id == *id) else {
                    continue;
                };
                if n > 0 {
                    spans.push(Span::styled(" · ", style));
                }
                let name = format!(" {} ", name_of(&l.dir));
                spans.push(if *id == target {
                    Span::styled(name, Style::default().add_modifier(Modifier::BOLD))
                } else {
                    Span::styled(name, style)
                });
            }
            buf.set_line(area.x, area.y, &Line::from(spans), area.width);
            return;
        }
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
            && loc.view.has_files()
        {
            let n = loc.view.files.len();
            let cur = loc.view.current_file();
            let pos = cur.map(|i| i + 1).unwrap_or(0);
            right = match (loc.view.narrowed(), cur) {
                (Some(_), Some(fi)) => {
                    let f = &loc.view.files[fi];
                    format!(" file {pos} of {n}  +{} −{}  ·", f.added, f.removed)
                }
                (_, cur) => {
                    let (added, removed) = loc.view.totals();
                    let noun = if n == 1 { "file" } else { "files" };
                    match cur {
                        Some(_) => format!(" {n} {noun}  +{added} −{removed}  ·  {pos}/{n}  ·"),
                        // Above the first file, in the description.
                        None => format!(" {n} {noun}  +{added} −{removed}  ·"),
                    }
                }
            };
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

/// A location's short name: its directory's.
fn name_of(dir: &std::path::Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// `dir` with the home directory shown as `~`.
fn tilde(dir: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// "3 hours ago", for commit lists.
fn ago(time: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(time);
    let s = (now - time).max(0);
    let (n, unit) = match s {
        0..60 => return "just now".to_string(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..604_800 => (s / 86_400, "day"),
        604_800..2_592_000 => (s / 604_800, "week"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn run(mut app: App, rx: Receiver<Msg>) -> Result<()> {
    let mut terminal = ratatui::init();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )
    .ok();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f))?;
            // Wake up on a timer while something on screen is time-based.
            let msg = if app.needs_tick() {
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
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    )
    .ok();
    ratatui::restore();
    result
}

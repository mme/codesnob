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
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
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

/// Nerd Font `md-source_commit`: a ring on a vertical line, matching how
/// Alt-Up and Alt-Down walk the branch.
const COMMIT_ICON: &str = "\u{f0718}";

/// Lines shown above and below the target line of an excerpt.
const EXCERPT: usize = 8;

/// How long a load may take before the screen says it is loading.
const LOADING_DELAY: Duration = Duration::from_millis(150);

pub struct Palette {
    /// Shortcuts in the palette.
    pub key_fg: Color,
    /// An agent's highlights: their tint and their letters and notes.
    pub mark_bg: Color,
    pub mark_fg: Color,
    /// The user's comments, likewise.
    pub comment_bg: Color,
    pub comment_fg: Color,
    pub add_bg: Color,
    pub add_emph: Color,
    pub del_bg: Color,
    pub del_emph: Color,
    pub header_bg: Color,
    pub hunk_fg: Color,
    pub gutter_fg: Color,
    pub note_fg: Color,
    /// The cursor's row: its background on a plain row, and how far a
    /// tinted row's background shifts (lighter on dark, darker on light).
    pub cursor_bg: Color,
    cursor_shift: i16,
    /// The cursor row's line numbers.
    pub cursor_fg: Color,
}

impl Palette {
    /// The tint and text color of marks by `author`.
    pub fn mark_colors(&self, author: diffview::Author) -> (Color, Color) {
        match author {
            diffview::Author::Agent => (self.mark_bg, self.mark_fg),
            diffview::Author::User => (self.comment_bg, self.comment_fg),
        }
    }

    /// The background the cursor's row gets over `bg`.
    pub fn cursor_over(&self, bg: Color) -> Color {
        let Color::Rgb(r, g, b) = bg else {
            return self.cursor_bg;
        };
        let shift = |c: u8| (i16::from(c) + self.cursor_shift).clamp(0, 255) as u8;
        Color::Rgb(shift(r), shift(g), shift(b))
    }

    pub fn dark() -> Self {
        Self {
            key_fg: Color::Cyan,
            mark_bg: Color::Rgb(0x2d, 0x2a, 0x50),
            mark_fg: Color::Rgb(0xc4, 0xa7, 0xff),
            comment_bg: Color::Rgb(0x3d, 0x2f, 0x12),
            comment_fg: Color::Rgb(0xff, 0xcc, 0x66),
            add_bg: Color::Rgb(0x12, 0x33, 0x1c),
            add_emph: Color::Rgb(0x1f, 0x6b, 0x2f),
            del_bg: Color::Rgb(0x3b, 0x12, 0x19),
            del_emph: Color::Rgb(0x7a, 0x1f, 0x2b),
            header_bg: Color::Rgb(0x2a, 0x2d, 0x3a),
            hunk_fg: Color::Cyan,
            gutter_fg: Color::DarkGray,
            note_fg: Color::DarkGray,
            cursor_bg: Color::Rgb(0x2a, 0x2e, 0x38),
            cursor_shift: 0x18,
            cursor_fg: Color::White,
        }
    }

    pub fn light() -> Self {
        Self {
            key_fg: Color::Blue,
            mark_bg: Color::Rgb(0xec, 0xe6, 0xff),
            mark_fg: Color::Rgb(0x6a, 0x3d, 0xd1),
            comment_bg: Color::Rgb(0xff, 0xf3, 0xd6),
            comment_fg: Color::Rgb(0x8a, 0x5a, 0x00),
            add_bg: Color::Rgb(0xe2, 0xf5, 0xe6),
            add_emph: Color::Rgb(0xa8, 0xe8, 0xb5),
            del_bg: Color::Rgb(0xff, 0xe3, 0xe6),
            del_emph: Color::Rgb(0xff, 0xb5, 0xbd),
            header_bg: Color::Rgb(0xe4, 0xe6, 0xef),
            hunk_fg: Color::Blue,
            gutter_fg: Color::Gray,
            note_fg: Color::Gray,
            cursor_bg: Color::Rgb(0xe6, 0xe8, 0xee),
            cursor_shift: -0x18,
            cursor_fg: Color::Black,
        }
    }
}

pub struct Options {
    pub context: usize,
    /// Where the recent-locations list is kept; `None` keeps nothing.
    pub state_dir: Option<PathBuf>,
    /// Where PR checkouts live; `None` turns PRs off.
    pub cache_dir: Option<PathBuf>,
    /// Where PR details come from.
    pub pr_source: std::sync::Arc<dyn crate::pr::Source>,
}

/// A location that shows a pull request.
struct PrState {
    spec: crate::pr::Spec,
    info: Option<crate::pr::Info>,
    /// A fetch is running.
    fetching: bool,
    /// Checked out at least once, so `dir` is real.
    ready: bool,
    /// Keeps the checkout from being pruned while this viewer has it open.
    _hold: Option<crate::pr::Hold>,
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
    /// Several files, by path, as an agent asked for.
    Files(Vec<String>),
}

/// A show request held back until the user is done reviewing.
struct PendingShow {
    target: Target,
    view: Option<crate::proto::ViewSpec>,
    /// What it would show, in words.
    words: String,
}

/// Something the user is asked before it happens.
enum Confirm {
    /// Remove the user's comments on the location on screen.
    ClearComments,
}

/// What Esc returns to after an agent changed the view.
enum AgentReturn {
    /// The view this location had before the agent's call.
    View(Box<SavedView>),
    /// The location that was on screen before the agent's call.
    Switch(u64),
}

/// A mark on a location's code: an agent's highlight or the user's
/// comment, with the text it was placed on so it can follow that text.
struct LocMark {
    /// Its letter, fixed when it was made: A to Z, then AA on.
    label: String,
    /// Its place in the order marks were made on this location.
    seq: usize,
    author: diffview::Author,
    path: String,
    start: usize,
    end: usize,
    /// An agent's note, or the user's comment.
    note: Option<String>,
    /// The marked lines as first seen; `None` until then.
    text: Option<Vec<String>>,
    /// Its lines are nowhere to be found since the last load.
    stale: bool,
    /// Its lines are the old side's: a comment on deleted lines.
    old_side: bool,
    /// Deleted lines were among those commented on, with others.
    with_deletions: bool,
    /// An agent has read it since it was made or last edited. Highlights
    /// are the agent's own, so they start read.
    seen: bool,
    /// The letter of the mark this comment answers.
    re: Option<String>,
}

impl LocMark {
    /// As reported to an agent.
    fn info(&self) -> crate::proto::MarkInfo {
        let user = self.author == diffview::Author::User;
        crate::proto::MarkInfo {
            label: self.label.clone(),
            author: if user { "user" } else { "agent" }.to_string(),
            path: self.path.clone(),
            start: self.start,
            end: self.end,
            old_side: self.old_side,
            with_deletions: self.with_deletions,
            stale: self.stale,
            new: user && !self.seen,
            re: self.re.clone(),
            text: self.note.clone(),
            lines: if user {
                self.text.clone().unwrap_or_default()
            } else {
                Vec::new()
            },
        }
    }

    fn drawn(&self) -> diffview::Mark {
        diffview::Mark {
            path: self.path.clone(),
            start: self.start,
            end: self.end,
            label: self.label.clone(),
            seq: self.seq,
            author: self.author,
            note: self.note.clone(),
            stale: self.stale,
            old_side: self.old_side,
            re: self.re.clone(),
        }
    }
}

/// Find a highlight's text in `lines`: where it was, else the nearest exact
/// match. On first sight, record the text. False when it cannot be placed.
fn anchor_mark(m: &mut LocMark, lines: &[String]) -> bool {
    if m.start == 0 || m.start > m.end {
        return false;
    }
    let Some(text) = &m.text else {
        if m.end > lines.len() {
            return false;
        }
        m.text = Some(lines[m.start - 1..m.end].to_vec());
        return true;
    };
    let len = text.len();
    if lines.get(m.start - 1..m.start - 1 + len) == Some(&text[..]) {
        return true;
    }
    if len > lines.len() {
        return false;
    }
    let best = (0..=lines.len() - len)
        .filter(|&i| lines[i..i + len] == text[..])
        .min_by_key(|&i| i.abs_diff(m.start - 1));
    match best {
        Some(i) => {
            m.start = i + 1;
            m.end = i + len;
            true
        }
        None => false,
    }
}

/// Follow every mark to its text; returns the letters of those that just
/// went stale. Marks on files `lines_of` does not know stay as they are.
fn reanchor(
    marks: &mut [LocMark],
    lines_of: &dyn Fn(&str, bool) -> Option<Vec<String>>,
) -> Vec<String> {
    let mut gone = Vec::new();
    for m in marks.iter_mut() {
        let Some(lines) = lines_of(&m.path, m.old_side) else {
            continue;
        };
        let was = m.stale;
        m.stale = !anchor_mark(m, &lines);
        if m.stale && !was {
            gone.push(m.label.clone());
        }
    }
    gone
}

fn gone_message(gone: Vec<String>) -> Option<String> {
    match gone.as_slice() {
        [] => None,
        [one] => Some(format!("[{one}] no longer matches its code")),
        many => Some(format!(
            "{} no longer match their code",
            many.iter()
                .map(|l| format!("[{l}]"))
                .collect::<Vec<_>>()
                .join(" ")
        )),
    }
}

/// A location's view, kept to return to.
struct SavedView {
    range: Range,
    base: Option<String>,
    label: Option<String>,
    focus: Focus,
    anchor: Option<Anchor>,
    code: Option<crate::proto::CodeSpec>,
}

/// Where the view was when a previewing list opened, to return to on Esc.
struct Origin {
    range: Range,
    focus: Focus,
    anchor: Option<Anchor>,
}

impl Location {
    /// Make a mark with the next letter.
    fn add_mark(
        &mut self,
        author: diffview::Author,
        path: String,
        start: usize,
        end: usize,
        note: Option<String>,
    ) -> &mut LocMark {
        let seq = self.marks_made;
        self.marks_made += 1;
        self.marks.push(LocMark {
            label: crate::proto::letter(seq),
            seq,
            author,
            path,
            start,
            end,
            note,
            text: None,
            stale: false,
            old_side: false,
            with_deletions: false,
            seen: author == diffview::Author::Agent,
            re: None,
        });
        self.marks.last_mut().expect("just pushed")
    }

    /// "L3": how agents name this location.
    fn label(&self) -> String {
        format!("L{}", self.id)
    }

    /// Comments no agent has read.
    fn unread(&self) -> usize {
        self.marks
            .iter()
            .filter(|m| m.author == diffview::Author::User && !m.seen)
            .count()
    }

    /// The user's comments here.
    fn comments(&self) -> usize {
        self.marks
            .iter()
            .filter(|m| m.author == diffview::Author::User)
            .count()
    }

    /// Has only ever shown a message: not a repository, a pull request
    /// that could not be fetched, or a first load that failed.
    fn invalid(&self) -> bool {
        if self.pr.as_ref().is_some_and(|p| !p.ready) {
            return self.error.is_some();
        }
        !self.repo || (self.error.is_some() && self.info.is_none() && self.code_shown.is_none())
    }

    /// The short name: "#142" for a PR, else the directory's name.
    fn name(&self) -> String {
        match &self.pr {
            Some(p) => format!("#{}", p.spec.number),
            None => dir_name(&self.dir),
        }
    }
}

struct Location {
    id: u64,
    dir: PathBuf,
    base: Option<String>,
    repo: bool,
    /// The range asked for; `info.range` is the one on screen.
    range: Range,
    /// Words for a comparison range, as it was asked for: "main...feature".
    range_label: Option<String>,
    /// Set while an agent's view is on screen: where Esc goes back to.
    agent_return: Option<AgentReturn>,
    /// Files to show as they are instead of the range's changes.
    code: Option<crate::proto::CodeSpec>,
    /// The code view currently on screen, if the screen shows one.
    code_shown: Option<crate::proto::CodeSpec>,
    /// The revision the code on screen was read at, as a short id.
    code_rev: Option<String>,
    code_fp: u64,
    /// Marks on this location's code, following their text through edits.
    marks: Vec<LocMark>,
    /// How many marks were ever made here; the next one's letter.
    marks_made: usize,
    /// Open at the target line or first highlight once the view has loaded.
    place_pending: bool,
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
    pr: Option<PrState>,
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
    /// Lines being marked for a comment: the row the marking started on.
    marking: Option<usize>,
    /// The comment the prompt is for: its lines, and the comment being
    /// edited, by its place in the order marks were made, if not new.
    composing: Option<(diffview::Region, Option<usize>)>,
    /// A question on the status bar, answered with y or n.
    confirm: Option<Confirm>,
    /// An agent's request to show something else, held while the user is
    /// reviewing: applied with y, or once the agent reads the comments.
    pending_show: Option<PendingShow>,
    /// Where to return when a previewing list is closed with Esc.
    origin: Option<Origin>,
    /// Recently chosen panel entries per list, newest first.
    recent: HashMap<panel::Kind, Vec<String>>,
    message: Option<String>,
    body_h: usize,
    /// Where the body starts on screen; mouse rows are relative to it.
    body_y: u16,
    /// Commit messages are shown above a commit's diff unless turned off.
    show_commit_desc: bool,
    /// PR descriptions are hidden until turned on.
    show_pr_desc: bool,
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
            marking: None,
            composing: None,
            confirm: None,
            pending_show: None,
            origin: None,
            recent: HashMap::new(),
            message: None,
            body_h: 1,
            body_y: 0,
            show_commit_desc: true,
            show_pr_desc: false,
            flip: None,
            flip_window: FLIP_WINDOW,
            open_choices: Vec::new(),
            quit: false,
        }
    }

    /// Display a target: switch to it if this viewer has it open, else open
    /// it as a new location. Not being in a repository shows a placeholder.
    pub fn open(&mut self, target: Target) {
        // One location per checkout, however it is named.
        let target = Target {
            dir: if target.repo {
                checkout_root(&target.dir)
            } else {
                target.dir
            },
            ..target
        };
        if let Some(i) = self.locations.iter().position(|l| l.dir == target.dir) {
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
            range_label: None,
            agent_return: None,
            code: None,
            code_shown: None,
            code_rev: None,
            code_fp: 0,
            marks: Vec::new(),
            marks_made: 0,
            place_pending: false,
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
            pr: None,
        };
        if let Some(state) = &self.opts.state_dir {
            crate::recent::record(state, &target.dir, target.base.as_deref(), None);
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
        // A list previewing on the location being left puts it back first.
        if self.panel.is_some() {
            self.close_panel(true);
        }
        if let Some(loc) = self.locations.first_mut() {
            loc._watch = None;
            loc.mode = Mode::Off;
        }
    }

    /// Once something valid is open, locations that only ever showed a
    /// message go, except the one on screen.
    fn drop_invalid(&mut self) {
        if !self.locations.iter().any(|l| !l.invalid()) {
            return;
        }
        let gone: Vec<u64> = self
            .locations
            .iter()
            .skip(1)
            .filter(|l| l.invalid())
            .map(|l| l.id)
            .collect();
        if gone.is_empty() {
            return;
        }
        self.locations.retain(|l| !gone.contains(&l.id));
        // A walk in progress forgets them too, keeping its place.
        if let Some(f) = self.flip.as_mut() {
            let before = f.order[..f.depth.min(f.order.len())]
                .iter()
                .filter(|id| gone.contains(id))
                .count();
            f.order.retain(|id| !gone.contains(id));
            f.depth -= before;
            if f.order.is_empty() {
                self.flip = None;
            }
        }
    }

    /// Start watching the front location and bring it up to date.
    fn activate(&mut self) {
        self.drop_invalid();
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
        if loc.pr.as_ref().is_some_and(|p| !p.ready) {
            // Still being fetched: nothing to watch or load yet.
            return;
        }
        let target = Target {
            dir: loc.dir.clone(),
            base: loc.base.clone(),
            repo: loc.repo,
        };
        self.link.set_target(target);
        self.request();
    }

    /// Open a PR as a new location, or switch to it when it is open here.
    fn open_pr(&mut self, spec: crate::pr::Spec) {
        let cwd = self.cur().filter(|l| l.repo).map(|l| l.dir.clone());
        self.open_pr_from(spec, cwd);
    }

    /// Open a PR; `#123` is resolved in the repository at `cwd`.
    fn open_pr_from(&mut self, spec: crate::pr::Spec, cwd: Option<PathBuf>) {
        let Some(cache) = self.opts.cache_dir.clone() else {
            self.message = Some("pull requests need a cache directory".to_string());
            return;
        };
        if let Some(i) = self.locations.iter().position(|l| {
            let Some(p) = &l.pr else { return false };
            let repo = p.info.as_ref().and_then(|i| i.repo());
            p.spec.number == spec.number
                && spec.repo.is_some()
                && repo.as_ref().map(|r| r.slug()) == spec.repo.as_ref().map(|r| r.slug())
        }) {
            self.switch_to(i);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let loc = Location {
            id,
            dir: PathBuf::new(),
            base: None,
            repo: false,
            range: Range::Everything,
            range_label: None,
            agent_return: None,
            code: None,
            code_shown: None,
            code_rev: None,
            code_fp: 0,
            marks: Vec::new(),
            marks_made: 0,
            place_pending: false,
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
            pr: Some(PrState {
                spec: spec.clone(),
                info: None,
                fetching: true,
                ready: false,
                _hold: None,
            }),
        };
        self.deactivate();
        self.locations.insert(0, loc);
        self.activate();
        self.fetch_pr(id, spec, cwd, cache);
    }

    fn fetch_pr(&mut self, id: u64, spec: crate::pr::Spec, cwd: Option<PathBuf>, cache: PathBuf) {
        // Clones this viewer has open, to fetch the PR into rather than cloning.
        let clones = self
            .locations
            .iter()
            .filter(|l| l.repo)
            .map(|l| l.dir.clone())
            .collect();
        crate::pr::spawn_fetch(
            spec,
            cwd,
            clones,
            cache,
            self.opts.pr_source.clone(),
            self.tx.clone(),
            id,
        );
    }

    /// `r` on a PR: fetch it again, then reload.
    fn refetch_pr(&mut self) {
        let Some(cache) = self.opts.cache_dir.clone() else {
            return;
        };
        let Some(loc) = self.cur_mut() else { return };
        let id = loc.id;
        let dir = loc.dir.clone();
        let Some(p) = loc.pr.as_mut() else { return };
        if p.fetching {
            return;
        }
        p.fetching = true;
        let spec = p
            .info
            .as_ref()
            .and_then(|i| crate::pr::parse(&i.url))
            .unwrap_or_else(|| p.spec.clone());
        let number = spec.number;
        self.message = Some(format!("fetching #{number}…"));
        let cwd = (!dir.as_os_str().is_empty()).then_some(dir);
        self.fetch_pr(id, spec, cwd, cache);
    }

    fn apply_pr(&mut self, id: u64, progress: crate::pr::Progress) {
        use crate::pr::Progress;
        let Some(idx) = self.locations.iter().position(|l| l.id == id) else {
            return;
        };
        match progress {
            Progress::Info(info) => {
                // Opened twice by different names: keep the one already here.
                let dup = self.locations.iter().position(|l| {
                    l.id != id
                        && l.pr.as_ref().is_some_and(|p| {
                            p.ready && p.info.as_ref().is_some_and(|i| i.url == info.url)
                        })
                });
                if let Some(other) = dup {
                    if idx == 0 {
                        self.locations.remove(0);
                        let other = if other > 0 { other - 1 } else { other };
                        let loc = self.locations.remove(other);
                        self.locations.insert(0, loc);
                        self.activate();
                    } else {
                        self.locations.remove(idx);
                    }
                    return;
                }
                if let Some(p) = self.locations[idx].pr.as_mut() {
                    p.info = Some(info);
                }
            }
            Progress::Ready {
                info,
                dir,
                base,
                note,
            } => {
                if let Some(state) = &self.opts.state_dir {
                    let pr = crate::recent::PrRef {
                        url: info.url.clone(),
                        title: info.title.clone(),
                    };
                    crate::recent::record(state, &dir, Some(&base), Some(pr));
                }
                let loc = &mut self.locations[idx];
                let first = loc.pr.as_ref().is_some_and(|p| !p.ready);
                if let Some(p) = loc.pr.as_mut() {
                    p.info = Some(info);
                    p.fetching = false;
                    p.ready = true;
                    if p._hold.is_none() {
                        p._hold = crate::pr::hold(&dir);
                    }
                }
                loc.dir = dir;
                loc.base = Some(base);
                loc.repo = true;
                loc.error = None;
                loc.desc_dirty = true;
                // A fetch by hand ("fetching #N…") ends in a report: the note,
                // or what the reload that follows finds.
                if note.is_some() {
                    self.message = note;
                } else if !first && idx == 0 {
                    self.message = None;
                    loc.manual = true;
                }
                if idx == 0 {
                    if first {
                        self.activate();
                    } else {
                        self.request();
                    }
                }
            }
            Progress::Failed(e) => {
                let loc = &mut self.locations[idx];
                let ready = loc.pr.as_ref().is_some_and(|p| p.ready);
                if let Some(p) = loc.pr.as_mut() {
                    p.fetching = false;
                }
                if ready {
                    self.message = Some(format!("fetch failed: {e}"));
                } else if idx == 0 {
                    loc.error = Some(e);
                } else {
                    // Failed out of sight: say so, as it may not stay.
                    let n = loc.pr.as_ref().map_or(0, |p| p.spec.number);
                    self.message = Some(format!("fetching #{n} failed: {e}"));
                    loc.error = Some(e);
                    self.drop_invalid();
                }
            }
        }
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
    /// Walk the locations in most-recently-used order: `back` toward older
    /// ones (Alt-Left, `-`), otherwise the other way round (Alt-Right). Quick
    /// repeats keep walking the same strip, so either key undoes the other.
    fn flip(&mut self, back: bool) {
        if self.locations.len() < 2 {
            self.message = Some("no other location open; o opens one".to_string());
            return;
        }
        let (order, depth) = match self.flip.take() {
            Some(f) if f.last.elapsed() < self.flip_window => (f.order, f.depth),
            _ => (self.locations.iter().map(|l| l.id).collect::<Vec<_>>(), 0),
        };
        let len = order.len();
        let depth = if back {
            (depth + 1) % len
        } else {
            (depth + len - 1) % len
        };
        let flip = Flip {
            order,
            depth,
            last: Instant::now(),
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
    /// Tab in the Open prompt: a directory name, else a branch or tag name.
    fn complete(&self, input: &str) -> Option<String> {
        self.complete_path(input)
            .or_else(|| self.complete_ref(input))
    }

    /// Complete the ref being typed, also after `..` or `...`.
    fn complete_ref(&self, input: &str) -> Option<String> {
        let loc = self.cur().filter(|l| l.repo)?;
        let cut = input
            .rfind("..")
            .map(|i| {
                if input[i + 2..].starts_with('.') {
                    i + 3
                } else {
                    i + 2
                }
            })
            .unwrap_or(0);
        let (head, prefix) = input.split_at(cut);
        let names: Vec<String> = git::ref_names(&loc.dir)
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .collect();
        let first = names.first()?;
        if names.len() == 1 {
            return Some(format!("{head}{first}"));
        }
        let common = names.iter().fold(first.clone(), |acc, n| {
            acc.chars()
                .zip(n.chars())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect()
        });
        (common.len() > prefix.len()).then(|| format!("{head}{common}"))
    }

    fn complete_path(&self, input: &str) -> Option<String> {
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
                let open = self.locations.iter().any(|l| l.dir == e.dir && l.repo);
                let text = match &e.pr {
                    Some(p) => match crate::pr::parse(&p.url) {
                        Some(spec) => format!(
                            "{}#{} {}",
                            spec.repo.map(|r| r.slug()).unwrap_or_default(),
                            spec.number,
                            p.title
                        ),
                        None => p.title.clone(),
                    },
                    None => tilde(&e.dir, home.as_deref()),
                };
                panel::Item {
                    tag: None,
                    text,
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
                text: match l.pr.as_ref().and_then(|p| p.info.as_ref()) {
                    Some(i) => format!("{}  {}", l.name(), i.title),
                    None => format!("{}  {}", l.name(), tilde(&l.dir, home.as_deref())),
                },
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
        if let Some(p) = l.pr.as_ref().filter(|p| !p.ready) {
            return if p.fetching {
                "fetching".to_string()
            } else {
                "failed".to_string()
            };
        }
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
            Range::Compare(..) => l
                .range_label
                .clone()
                .unwrap_or_else(|| "comparison".to_string()),
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
            code: loc.code.as_ref().map(|c| loader::Code {
                rev: c.rev.clone(),
                paths: c.files.iter().map(|f| f.path.clone()).collect(),
            }),
        });
    }

    fn set_range(&mut self, range: Range) {
        let Some(loc) = self.cur_mut() else { return };
        if loc.range == range {
            return;
        }
        loc.range = range;
        loc.error = None;
        loc.pending_anchor = None;
        self.request();
    }

    /// Whether a load is in flight, which needs timed redraws.
    pub fn loading(&self) -> bool {
        self.cur()
            .is_some_and(|l| l.pending_since.is_some() || l.pr.as_ref().is_some_and(|p| p.fetching))
    }

    fn apply_loaded(&mut self, id: u64, seq: u64, result: Result<loader::Load, String>) {
        match result {
            Ok(loader::Load::Code(code)) => self.apply_code(id, seq, code),
            Ok(loader::Load::Diff(snap)) => self.apply_diff(id, seq, Ok(*snap)),
            Err(e) => self.apply_diff(id, seq, Err(e)),
        }
    }

    /// Files to show as they are have loaded.
    fn apply_code(&mut self, id: u64, seq: u64, code: git::CodeSnapshot) {
        let current = self.locations.first().is_some_and(|l| l.id == id);
        let Some(loc) = self.locations.iter_mut().find(|l| l.id == id) else {
            return;
        };
        if seq != loc.seq {
            return;
        }
        loc.pending_since = None;
        let manual = std::mem::take(&mut loc.manual);
        let pending = loc.pending_anchor.take();
        // The code view was left while this loaded.
        let Some(spec) = loc.code.clone() else {
            return;
        };
        loc.error = None;
        let fingerprint = code.fingerprint();
        let same_view = loc.code_shown.as_ref() == Some(&spec);
        if same_view && fingerprint == loc.code_fp {
            if manual && current {
                self.message = Some("no changes since the last load".to_string());
            }
            return;
        }
        let anchor = if same_view { loc.view.anchor() } else { None };
        let gone = reanchor(&mut loc.marks, &|path, _| {
            code.files
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, d)| crate::diff::text_lines(d))
        });
        // A file shows whole or as excerpts around its line and highlights,
        // as asked; unasked, one file shows whole and several as excerpts.
        // Highlights get a diff's context around them.
        let several = spec.files.len() > 1;
        let context = self.opts.context;
        let marks = &loc.marks;
        let files = code
            .files
            .into_iter()
            .map(|(path, data)| {
                let asked = spec.files.iter().find(|f| f.path == path);
                let mut windows = Vec::new();
                if asked.and_then(|f| f.excerpts).unwrap_or(several) {
                    if let Some(l) = asked.and_then(|f| f.line) {
                        windows.push((l.saturating_sub(EXCERPT).max(1), l + EXCERPT));
                    }
                    for m in marks.iter().filter(|m| m.path == path) {
                        windows.push((m.start.saturating_sub(context).max(1), m.end + context));
                    }
                }
                crate::diff::plain(path, &data, &windows)
            })
            .collect();
        loc.view.set_built(files);
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        if current && let Some(text) = gone_message(gone) {
            self.message = Some(text);
        }
        loc.view.target = spec
            .files
            .iter()
            .find_map(|f| f.line.map(|l| (f.path.clone(), l)));
        narrow_view(&mut loc.view, &loc.focus);
        let place = std::mem::take(&mut loc.place_pending);
        match anchor.or(pending) {
            Some(a) if !place => loc.view.restore(a),
            _ => {
                // Placed at the next draw, when the screen height is known.
                loc.view.go_top();
                loc.view.hscroll = 0;
                loc.view.place_target = loc.view.target.is_some() || loc.view.has_marks();
            }
        }
        loc.code_shown = Some(spec);
        loc.code_rev = code.rev;
        loc.code_fp = fingerprint;
        loc.desc_dirty = true;
        if current {
            self.hl.clear();
            if manual {
                self.message = Some("reloaded".to_string());
            }
        }
    }

    fn apply_diff(&mut self, id: u64, seq: u64, result: Result<git::Snapshot, String>) {
        let context = self.opts.context;
        // A location loaded in the background speaks only once it is shown.
        let current = self.locations.first().is_some_and(|l| l.id == id);
        let Some(loc) = self.locations.iter_mut().find(|l| l.id == id) else {
            return;
        };
        if seq != loc.seq {
            return;
        }
        loc.pending_since = None;
        let manual = std::mem::take(&mut loc.manual);
        // Only ever for this load; never carried to a later range.
        let pending = loc.pending_anchor.take();
        let snap = match result {
            Ok(snap) => snap,
            Err(e) => {
                let shown = match &loc.code {
                    Some(code) => loc.code_shown.as_ref() == Some(code),
                    None => {
                        loc.code_shown.is_none()
                            && loc.info.as_ref().is_some_and(|i| i.range == loc.range)
                    }
                };
                if !shown {
                    loc.error = Some(e);
                } else if current {
                    self.message = Some(format!("refresh failed: {e}"));
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
            loc.range = Range::Everything;
            if current {
                let short = id.to_hex_with_len(7).to_string();
                self.message = Some(format!("{short} is no longer on this branch"));
                self.request();
            }
            return;
        }
        // The code view was entered while this diff loaded.
        if loc.code.is_some() {
            return;
        }
        let leaving_code = loc.code_shown.take().is_some();
        let fingerprint = snap.fingerprint();
        if fingerprint == loc.fingerprint && !leaving_code {
            if manual && current {
                self.message = Some("no changes since the last load".to_string());
            }
            return;
        }
        let same_range = !leaving_code && loc.info.as_ref().is_some_and(|i| i.range == snap.range);
        let anchor = if same_range { loc.view.anchor() } else { None };
        let (info, files) = Info::split(snap);
        loc.fingerprint = fingerprint;
        let gone = reanchor(&mut loc.marks, &|path, old_side| {
            files
                .iter()
                .find(|f| f.new_path.as_deref().or(f.old_path.as_deref()) == Some(path))
                .and_then(|f| {
                    if old_side {
                        f.old.as_deref()
                    } else {
                        f.new.as_deref().or(f.old.as_deref())
                    }
                })
                .map(crate::diff::text_lines)
        });
        let ranges: Vec<(String, usize, usize)> = loc
            .marks
            .iter()
            .map(|m| (m.path.clone(), m.start, m.end))
            .collect();
        loc.view.set_files_showing(files, context, &|path| {
            ranges
                .iter()
                .filter(|(p, ..)| p == path)
                .map(|&(_, a, b)| (a, b))
                .collect()
        });
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        if current && let Some(text) = gone_message(gone) {
            self.message = Some(text);
        }
        if !narrow_view(&mut loc.view, &loc.focus) {
            // While a list previews, a file missing from one commit is not news.
            if self.panel.is_none() || !current {
                self.message = Some(focus_lost(&loc.focus));
                loc.focus = Focus::All;
            }
        }
        match anchor.or(pending) {
            Some(a) if !loc.place_pending => loc.view.restore(a),
            _ if std::mem::take(&mut loc.place_pending) => {
                loc.view.go_top();
                loc.view.hscroll = 0;
                loc.view.place_target = loc.view.has_marks();
            }
            None if !same_range => {
                loc.view.go_top();
                loc.view.hscroll = 0;
            }
            _ => {}
        }
        loc.info = Some(info);
        loc.desc_dirty = true;
        if current {
            self.hl.clear();
            if manual {
                self.message = Some("reloaded".to_string());
            }
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
            Msg::Show(target, reply) => {
                let ack = if self.held(&target, None) {
                    self.hold(target, None)
                } else {
                    self.open(target);
                    self.ack(Vec::new())
                };
                if let Some(id) = reply {
                    self.link.reply(id, serde_json::json!(ack));
                }
            }
            Msg::View(target, view, reply) => {
                let ack = if self.held(&target, Some(&view)) {
                    self.hold(target, Some(*view))
                } else {
                    self.show_view(target, *view)
                };
                if let Some(id) = reply {
                    self.link.reply(id, serde_json::json!(ack));
                }
            }
            Msg::Query { id, query } => {
                let result = match self.answer(query) {
                    Ok(v) => v,
                    Err(e) => serde_json::json!({ "error": e }),
                };
                self.link.reply(id, result);
            }
            Msg::Link(up) => self.linked = up,
            Msg::Pr { loc, progress } => self.apply_pr(loc, *progress),
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
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.panel.is_none()
                            && let Some(y) = m.row.checked_sub(self.body_y)
                            && let Some(loc) = self.cur_mut()
                        {
                            loc.view.click(y as usize);
                        }
                        return;
                    }
                    _ => return,
                };
                match self.panel.as_mut() {
                    Some(p) => {
                        let before = p.selected_key();
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
            let before = self.panel.as_ref().and_then(|p| p.selected_key());
            let outcome = self.panel.as_mut().map(|p| p.key(k, rows));
            let composing = self
                .panel
                .as_ref()
                .is_some_and(|p| p.kind == panel::Kind::Comment);
            match outcome {
                Some(Outcome::Close) if composing => {
                    self.panel = None;
                    self.composing = None;
                }
                Some(Outcome::Submit { input, .. }) if composing => {
                    self.panel = None;
                    self.save_comment(input);
                }
                Some(Outcome::Close) => self.close_panel(true),
                Some(Outcome::Choose(action, key)) => {
                    let kind = self.panel.as_ref().map(|p| p.kind);
                    self.close_panel(false);
                    if let Some(kind) = kind {
                        let list = self.recent.entry(kind).or_default();
                        list.retain(|k| *k != key);
                        list.insert(0, key.clone());
                    }
                    match action {
                        // Files are chosen by path: the list may predate a reload.
                        Action::GoToFile => self.go_to_path(&key),
                        _ => self.perform(action),
                    }
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
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let flips =
            k.code == KeyCode::Char('-') || alt && matches!(k.code, KeyCode::Left | KeyCode::Right);
        if !flips {
            self.flip = None;
        }
        if let Some(confirm) = self.confirm.take() {
            match (k.code, confirm) {
                (KeyCode::Char('y') | KeyCode::Char('Y'), Confirm::ClearComments) => {
                    self.clear_comments();
                }
                _ => self.message = Some("kept".to_string()),
            }
            return;
        }
        let ctrl_space = matches!(k.code, KeyCode::Null)
            || k.code == KeyCode::Char(' ') && ctrl
            || k.code == KeyCode::Char('@') && ctrl;
        if self.marking.is_some() {
            match k.code {
                KeyCode::Esc => {
                    self.marking = None;
                    self.message = Some("marking cancelled".to_string());
                    return;
                }
                KeyCode::Enter => {
                    self.end_region();
                    return;
                }
                _ if ctrl_space => {
                    self.end_region();
                    return;
                }
                _ => {}
            }
        } else if ctrl_space {
            self.perform(Action::MarkRegion);
            return;
        }
        if self.pending_show.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.apply_pending();
                    return;
                }
                KeyCode::Esc => {
                    self.pending_show = None;
                    self.message = Some("the agent's request was dropped".to_string());
                    return;
                }
                _ => {}
            }
        }
        let page = self.body_h.saturating_sub(2).max(1) as isize;
        let half = (self.body_h / 2).max(1) as isize;
        let scroll = |app: &mut App, d: isize| {
            if let Some(loc) = app.cur_mut() {
                loc.view.scroll_by(d);
            }
        };
        let step = |app: &mut App, d: isize| {
            if let Some(loc) = app.cur_mut() {
                loc.view.move_cursor(d);
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
            (KeyCode::Char(':'), false, _) | (KeyCode::Char('k'), true, false) => {
                self.open_palette()
            }
            (KeyCode::Char('f'), false, false) => self.perform(Action::Files),
            (KeyCode::Char('o'), false, false) => self.perform(Action::Open),
            (KeyCode::Char('-'), false, false) | (KeyCode::Left, false, true) => {
                self.perform(Action::PreviousLocation)
            }
            (KeyCode::Right, false, true) => self.perform(Action::NextLocation),
            (KeyCode::Char('c'), false, false) => self.perform(Action::Commits),
            (KeyCode::Char('<'), false, false) => self.perform(Action::PrevMark),
            (KeyCode::Char('>'), false, false) => self.perform(Action::NextMark),
            (KeyCode::Enter, false, false) => self.perform(Action::Comment),
            (KeyCode::Char('v'), false, false) => self.perform(Action::MarkRegion),
            (KeyCode::Char('e'), false, false) => self.perform(Action::EditComment),
            (KeyCode::Char('x'), false, false) => self.perform(Action::DeleteComment),
            (KeyCode::Char('X'), false, false) => self.perform(Action::ClearComments),
            (KeyCode::Char('d'), false, false) => self.perform(Action::ToggleDescription),
            (KeyCode::Char('u'), false, false) => self.toggle_uncommitted(),
            (KeyCode::Char('L'), false, false) => {
                if self.applicable(Action::Locations) {
                    self.perform(Action::Locations);
                } else {
                    self.message = Some("no other location open; o opens one".to_string());
                }
            }
            (KeyCode::Char('z'), false, false) => self.perform(Action::Narrow),

            // Up the commit list is older, down is newer, uncommitted last.
            (KeyCode::Up, false, true) => self.perform(Action::OlderCommit),
            (KeyCode::Down, false, true) => self.perform(Action::NewerCommit),
            (KeyCode::Down, _, _)
            | (KeyCode::Char('j'), false, false)
            | (KeyCode::Char('n'), true, false) => step(self, 1),
            (KeyCode::Up, _, _)
            | (KeyCode::Char('k'), false, false)
            | (KeyCode::Char('p'), true, false) => step(self, -1),
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

            (KeyCode::Char('n'), false, false) | (KeyCode::Char(']'), false, false) => {
                self.perform(Action::NextHunk)
            }
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

    /// `u`: the uncommitted changes alone, and back to everything.
    fn toggle_uncommitted(&mut self) {
        self.leave_code();
        let Some(loc) = self.cur() else { return };
        if loc.range == Range::Uncommitted {
            self.set_range(Range::Everything);
        } else if self.applicable(Action::RangeUncommitted) {
            self.set_range(Range::Uncommitted);
        } else if loc.repo {
            // Without a base branch, everything shown is uncommitted already.
            self.message = Some("everything here is uncommitted already".to_string());
        }
    }

    /// Esc: leave the narrowest thing first, and never quit.
    fn back_out(&mut self) {
        if self.cur().is_some_and(|l| l.agent_return.is_some()) {
            self.return_from_agent();
            return;
        }
        let Some(loc) = self.cur() else { return };
        if loc.pr.as_ref().is_some_and(|p| !p.ready) {
            // Esc while a PR is still being fetched cancels opening it.
            self.close_location();
            return;
        }
        if loc.code.is_some() {
            self.leave_code();
            return;
        }
        if loc.focus != Focus::All {
            self.narrow(None);
        } else if loc.range != Range::Everything {
            self.set_range(Range::Everything);
        }
    }

    /// Back from files as they are to the location's diff.
    fn leave_code(&mut self) {
        if let Some(loc) = self.cur_mut()
            && loc.code.take().is_some()
        {
            self.settle_view(None);
        }
    }

    /// Show `file` as it is, in its repository's location.
    fn open_file(&mut self, file: PathBuf) {
        let Some(parent) = file.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        if gix::discover(&parent).is_err() {
            self.open_dir(parent, None);
            return;
        }
        let root = checkout_root(&parent);
        let Ok(rel) = file
            .strip_prefix(&root)
            .map(|r| r.to_string_lossy().into_owned())
        else {
            return;
        };
        self.open(Target {
            dir: root,
            base: None,
            repo: true,
        });
        self.show_file(None, rel);
    }

    /// Show one file of the current location, as it is or as of `rev`.
    fn show_file(&mut self, rev: Option<String>, path: String) {
        if let Some(loc) = self.cur_mut() {
            loc.code = Some(crate::proto::CodeSpec {
                rev,
                files: vec![crate::proto::CodeFile {
                    path,
                    ..Default::default()
                }],
            });
            loc.focus = Focus::All;
        }
        self.settle_view(None);
    }

    /// Show a comparison typed in the Open prompt in the current location.
    fn apply_spec(&mut self, spec: git::Spec) {
        let Some(loc) = self.cur_mut() else { return };
        loc.code = None;
        match spec {
            git::Spec::Range(range, label) => {
                loc.range = range;
                loc.range_label = Some(label);
            }
            git::Spec::Base(name) => {
                loc.base = Some(name);
                loc.range = Range::Everything;
                loc.range_label = None;
            }
        }
        loc.error = None;
        self.settle_view(None);
    }

    /// Bring the screen in line with the current location's range and focus:
    /// narrow and place it at once when that range is already shown, else
    /// when it has loaded. `None` places it at the top.
    fn settle_view(&mut self, anchor: Option<Anchor>) {
        let Some(loc) = self.cur_mut() else { return };
        loc.desc_dirty = true;
        let shown = loc.code.is_none()
            && loc.code_shown.is_none()
            && loc.info.as_ref().is_some_and(|i| i.range == loc.range);
        if shown {
            narrow_view(&mut loc.view, &loc.focus);
            match anchor {
                Some(a) => loc.view.restore(a),
                None => loc.view.go_top(),
            }
            loc.pending_anchor = None;
        } else {
            loc.pending_anchor = anchor;
        }
        // A changed base or range loads; an unchanged one is a cheap no-op.
        self.request();
    }

    /// An agent's view: update the location for that checkout in place, or
    /// open one, remembering what was on screen so Esc can go back to it.
    pub fn show_view(
        &mut self,
        target: Target,
        mut view: crate::proto::ViewSpec,
    ) -> crate::proto::ShowAck {
        // Whatever the last key said is old news once the agent shows something.
        self.message = None;
        let mut fresh: Vec<crate::proto::HighlightSpec> = view
            .highlights
            .iter()
            .take(crate::proto::MAX_HIGHLIGHTS)
            .cloned()
            .collect();
        // Appending adds to the files the location shows as they are;
        // anything else is a fresh view.
        let append = view.append
            && self
                .cur()
                .is_some_and(|l| l.code.is_some() && l.dir == checkout_root(&target.dir));
        if append && let Some(loc) = self.cur() {
            let shown = loc.code.as_ref().expect("checked").files.clone();
            if let Some(code) = view.code.as_mut() {
                let mut files = shown;
                for f in code.files.drain(..) {
                    match files.iter_mut().find(|s| s.path == f.path) {
                        Some(s) => *s = f,
                        None => files.push(f),
                    }
                }
                code.files = files;
            }
        }
        // Lettered by where they sit: by file as the call lists them, then
        // by line.
        let order = |path: &str| {
            view.code
                .as_ref()
                .and_then(|c| c.files.iter().position(|f| f.path == path))
                .unwrap_or(usize::MAX)
        };
        fresh.sort_by_key(|h| (order(&h.path), h.path.clone(), h.start, h.end));
        let placed = !fresh.is_empty()
            || view
                .code
                .as_ref()
                .is_some_and(|c| c.files.iter().any(|f| f.line.is_some()));
        let before = self.cur().map(|l| l.id);
        self.show_view_inner(target, view);
        let mut added = Vec::new();
        // An update while the user is reviewing here leaves their cursor
        // alone and says what came.
        let reviewing =
            self.cur().map(|l| l.id) == before && self.cur().is_some_and(|l| l.unread() > 0);
        if let Some(loc) = self.cur_mut() {
            // A new call replaces the agent's own marks; the user's stay.
            if !append {
                loc.marks.retain(|m| m.author == diffview::Author::User);
            }
            for h in fresh {
                let m = loc.add_mark(diffview::Author::Agent, h.path, h.start, h.end, h.note);
                added.push(m.info());
            }
            loc.view.set_marks(Vec::new());
            loc.place_pending = placed && !reviewing;
            // Rebuild even if the content is unchanged: marks need lines.
            loc.fingerprint = 0;
            loc.code_fp = 0;
        }
        if reviewing && !added.is_empty() {
            let letters: Vec<String> = added.iter().map(|m| format!("[{}]", m.label)).collect();
            self.message = Some(format!("agent added {}", letters.join(" ")));
        }
        self.request();
        self.ack(added)
    }

    /// What to tell an agent about the location on screen after its call.
    fn ack(&self, marks: Vec<crate::proto::MarkInfo>) -> crate::proto::ShowAck {
        let Some(loc) = self.cur() else {
            return crate::proto::ShowAck {
                applied: true,
                ..Default::default()
            };
        };
        crate::proto::ShowAck {
            location: loc.label(),
            dir: loc.dir.clone(),
            marks,
            unread: loc.unread(),
            applied: true,
        }
    }

    /// The location an agent names, "L3", or the one on screen.
    fn location_by(&mut self, name: Option<&str>) -> Result<&mut Location, String> {
        let Some(name) = name else {
            return self.cur_mut().ok_or("no location is open".to_string());
        };
        let id: u64 = name
            .trim()
            .trim_start_matches(['L', 'l'])
            .parse()
            .map_err(|_| format!("{name} is not a location id; they look like L3"))?;
        self.locations
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(format!("no location {name} in this viewer"))
    }

    /// Answer an agent's query.
    fn answer(&mut self, query: crate::proto::ViewerQuery) -> Result<serde_json::Value, String> {
        match query {
            crate::proto::ViewerQuery::Review { location } => {
                let r = self.review(location.as_deref())?;
                Ok(serde_json::json!(r))
            }
            crate::proto::ViewerQuery::ClearMarks {
                location,
                which,
                letters,
            } => {
                let r = self.clear_marks(location.as_deref(), &which, &letters)?;
                Ok(serde_json::json!(r))
            }
        }
    }

    /// Every mark on a location for an agent; its comments count as read.
    fn review(&mut self, location: Option<&str>) -> Result<crate::proto::ReviewResult, String> {
        let current = self.cur().map(|l| l.id);
        let showing_current = self.scope_words();
        let loc = self.location_by(location)?;
        let showing = if Some(loc.id) == current {
            showing_current
        } else {
            "not on screen".to_string()
        };
        let is_current = Some(loc.id) == current;
        let unread = loc.unread();
        let mut marks: Vec<&mut LocMark> = loc.marks.iter_mut().collect();
        marks.sort_by_key(|m| (m.path.clone(), m.old_side, m.start, m.end, m.seq));
        let infos: Vec<crate::proto::MarkInfo> = marks
            .into_iter()
            .map(|m| {
                let info = m.info();
                m.seen = true;
                info
            })
            .collect();
        let result = crate::proto::ReviewResult {
            location: loc.label(),
            dir: loc.dir.clone(),
            showing,
            marks: infos,
            unread,
        };
        // Read, the comments no longer hold the agent's request back.
        if is_current && !self.typing() {
            self.apply_pending();
        }
        Ok(result)
    }

    fn clear_marks(
        &mut self,
        location: Option<&str>,
        which: &str,
        letters: &[String],
    ) -> Result<crate::proto::ClearResult, String> {
        let current = self.cur().map(|l| l.id);
        let loc = self.location_by(location)?;
        let goes = |m: &LocMark| {
            if !letters.is_empty() {
                return letters.iter().any(|l| l.eq_ignore_ascii_case(&m.label));
            }
            match which {
                "user" => m.author == diffview::Author::User,
                "all" => true,
                _ => m.author == diffview::Author::Agent,
            }
        };
        let cleared: Vec<String> = loc
            .marks
            .iter()
            .filter(|m| goes(m))
            .map(|m| m.label.clone())
            .collect();
        if let Some(missing) = letters
            .iter()
            .find(|l| !loc.marks.iter().any(|m| m.label.eq_ignore_ascii_case(l)))
        {
            return Err(format!(
                "no mark [{}] on {}",
                missing.to_uppercase(),
                loc.label()
            ));
        }
        loc.marks.retain(|m| !goes(m));
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        let result = crate::proto::ClearResult {
            location: loc.label(),
            cleared,
            remaining: loc.marks.len(),
            unread: loc.unread(),
        };
        if Some(loc.id) == current && !result.cleared.is_empty() {
            self.message = Some(format!(
                "agent cleared {}",
                result
                    .cleared
                    .iter()
                    .map(|l| format!("[{l}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        Ok(result)
    }

    fn show_view_inner(&mut self, target: Target, view: crate::proto::ViewSpec) {
        let before = self.cur().map(|l| l.id);
        let focus = match view.files.as_slice() {
            [] => Focus::All,
            [one] => Focus::File(one.clone()),
            many => Focus::Files(many.to_vec()),
        };
        if let Some(spec) = view.range.as_deref().and_then(crate::pr::parse) {
            self.open_pr_from(spec, Some(target.dir.clone()));
            if let Some(loc) = self.cur_mut() {
                loc.focus = focus;
                if loc.agent_return.is_none() && Some(loc.id) != before {
                    loc.agent_return = before.map(AgentReturn::Switch);
                }
            }
            self.settle_view(None);
            return;
        }
        let dir = if target.repo {
            checkout_root(&target.dir)
        } else {
            target.dir.clone()
        };
        let here = self.cur().is_some_and(|l| l.dir == dir);
        if here {
            if let Some(loc) = self.cur_mut()
                && loc.agent_return.is_none()
            {
                loc.agent_return = Some(AgentReturn::View(Box::new(SavedView {
                    range: loc.range,
                    base: loc.base.clone(),
                    label: loc.range_label.clone(),
                    focus: loc.focus.clone(),
                    anchor: loc.view.anchor(),
                    code: loc.code.clone(),
                })));
            }
        } else {
            self.open(Target { dir, ..target });
            if let Some(loc) = self.cur_mut()
                && loc.agent_return.is_none()
                && Some(loc.id) != before
            {
                loc.agent_return = before.map(AgentReturn::Switch);
            }
        }
        if let Some(code) = view.code {
            // Files as they are; the location's diff stays as it was.
            if let Some(loc) = self.cur_mut() {
                loc.code = Some(code);
                loc.focus = Focus::All;
                loc.error = None;
            }
            self.settle_view(None);
            return;
        }
        let parsed = match &view.range {
            Some(r) => self
                .cur()
                .filter(|l| l.repo)
                .map(|l| git::parse_spec(&l.dir, r).map_err(|e| e.to_string())),
            None => None,
        };
        let Some(loc) = self.cur_mut() else { return };
        loc.focus = focus;
        loc.error = None;
        loc.code = None;
        match parsed {
            Some(Ok(git::Spec::Range(range, label))) => {
                loc.range = range;
                loc.range_label = Some(label);
            }
            Some(Ok(git::Spec::Base(name))) => {
                loc.base = Some(name);
                loc.range = Range::Everything;
                loc.range_label = None;
            }
            Some(Err(e)) => self.message = Some(e),
            None => {
                // No range: everything since the branch point.
                loc.base = None;
                loc.range = Range::Everything;
                loc.range_label = None;
            }
        }
        self.settle_view(None);
    }

    /// Esc after an agent's call: back to exactly what was on screen.
    fn return_from_agent(&mut self) {
        let Some(ret) = self.cur_mut().and_then(|l| l.agent_return.take()) else {
            return;
        };
        match ret {
            AgentReturn::Switch(id) => {
                if let Some(i) = self.locations.iter().position(|l| l.id == id) {
                    self.switch_to(i);
                }
            }
            AgentReturn::View(saved) => {
                let SavedView {
                    range,
                    base,
                    label,
                    focus,
                    anchor,
                    code,
                } = *saved;
                let Some(loc) = self.cur_mut() else { return };
                loc.range = range;
                loc.base = base;
                loc.range_label = label;
                loc.focus = focus;
                loc.code = code;
                loc.error = None;
                self.settle_view(anchor);
            }
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
            _ => loc.view.go_top(),
        }
    }

    fn go_to_path(&mut self, path: &str) {
        let fi = self
            .cur()
            .and_then(|l| l.view.files.iter().position(|f| f.path == path));
        if let Some(fi) = fi {
            self.go_to_file(fi);
        }
    }

    /// Keep focus and the screen in agreement once a list has closed: a
    /// narrowed file the shown range lacks means showing all files.
    fn reconcile_focus(&mut self) {
        let Some(loc) = self.cur_mut() else { return };
        let Focus::File(path) = loc.focus.clone() else {
            return;
        };
        // A load for the asked range is on its way and will decide.
        if loc.info.as_ref().is_none_or(|i| i.range != loc.range) {
            return;
        }
        if loc
            .view
            .narrowed()
            .is_some_and(|fi| loc.view.files[fi].path == path)
        {
            return;
        }
        if !loc.view.narrow_to_path(Some(&path)) {
            loc.focus = Focus::All;
            loc.desc_dirty = true;
            self.message = Some(format!("{path} has no changes here; showing all files"));
        }
    }

    /// Move to file `fi`: jump in the full view, switch file while narrowed.
    fn go_to_file(&mut self, fi: usize) {
        let Some(loc) = self.cur_mut() else { return };
        let Some(path) = loc.view.files.get(fi).map(|f| f.path.clone()) else {
            return;
        };
        match &loc.focus {
            Focus::All => loc.view.scroll_to_file(fi),
            Focus::Files(paths) if paths.contains(&path) => loc.view.scroll_to_file(fi),
            _ => self.narrow(Some(path)),
        }
    }

    /// Show what a previewing list has highlighted, if it changed.
    /// `before` is the key of the row highlighted before the last key press.
    fn preview(&mut self, before: Option<String>) {
        let Some(panel) = &self.panel else { return };
        if !panel.kind.previews() {
            return;
        }
        let now_key = panel.selected_key();
        if now_key == before {
            return;
        }
        let now = panel.selected_action();
        match now {
            Some(Action::GoToFile) => {
                if let Some(path) = now_key {
                    self.go_to_path(&path);
                }
            }
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
            self.reconcile_focus();
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
        narrow_view(&mut loc.view, &loc.focus);
        if let Some(a) = origin.anchor {
            loc.view.restore(a);
        }
        self.reconcile_focus();
    }

    /// Enter in the Open prompt: a typed path that exists wins, else the
    /// highlighted recent location.
    fn submit_open(&mut self, input: &str, selected: Option<Action>) {
        let input = input.trim();
        if let Some(spec) = crate::pr::parse(input) {
            self.close_panel(false);
            self.open_pr(spec);
            return;
        }
        // A file: show it as it is.
        if !input.is_empty()
            && let Ok(file) = std::fs::canonicalize(self.expand(input))
            && file.is_file()
        {
            self.close_panel(false);
            self.open_file(file);
            return;
        }
        // A file as of a revision, in git's syntax: main:src/diff.rs.
        if let Some((rev, path)) = input.split_once(':')
            && !rev.is_empty()
            && !path.is_empty()
            && let Some(dir) = self.cur().filter(|l| l.repo).map(|l| l.dir.clone())
        {
            let path = path.trim_start_matches("./").to_string();
            match git::read_code(&dir, Some(rev), std::slice::from_ref(&path)) {
                Ok(_) => {
                    self.close_panel(false);
                    self.show_file(Some(rev.to_string()), path);
                }
                Err(e) => {
                    if let Some(p) = self.panel.as_mut() {
                        p.note = Some(format!("{e:#}"));
                    }
                }
            }
            return;
        }
        let typed = (!input.is_empty()).then(|| self.resolve_path(input));
        // Not a path: a comparison in the current repository, in git's syntax.
        if matches!(typed, Some(Err(_)))
            && let Some(dir) = self.cur().filter(|l| l.repo).map(|l| l.dir.clone())
            && let Ok(spec) = git::parse_spec(&dir, input)
        {
            self.close_panel(false);
            self.apply_spec(spec);
            return;
        }
        match (typed, selected) {
            (Some(Ok(dir)), _) => {
                self.close_panel(false);
                self.open_dir(dir, None);
            }
            (_, Some(action)) => {
                self.close_panel(false);
                self.perform(action);
            }
            (Some(Err(_)), None) => {
                if let Some(p) = self.panel.as_mut() {
                    p.note = Some(format!("no such directory, PR or revision: {input}"));
                }
            }
            (None, None) => self.close_panel(false),
        }
    }

    fn open_commits(&mut self) {
        let Some(loc) = self.cur() else { return };
        let Some(info) = &loc.info else { return };
        let mut items = Vec::new();
        let pr_number = loc.pr.as_ref().map(|p| p.spec.number);
        if let Some(base) = &info.base.name {
            let n = info.commits.len();
            items.push(panel::Item {
                tag: None,
                text: match pr_number {
                    Some(n) => format!("Everything in #{n}"),
                    None => format!("Everything since {base}"),
                },
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
            Range::Compare(..) => String::new(),
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
                let text = if steps[0] == Range::Uncommitted {
                    "no commits since the branch point"
                } else {
                    "this is the oldest commit"
                };
                self.message = Some(text.to_string());
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

    fn pr_description(&self, pr: &crate::pr::Info, width: u16) -> Vec<Line<'static>> {
        let width = width.saturating_sub(6) as usize;
        let dim = Style::default().fg(self.palette.note_fg);
        let who = pr
            .author
            .as_ref()
            .map(|a| format!(" · @{}", a.login))
            .unwrap_or_default();
        let branches = if pr.head_ref_name.is_empty() {
            format!("into {}", pr.base_ref_name)
        } else {
            format!("{} into {}", pr.head_ref_name, pr.base_ref_name)
        };
        let mut out = vec![Line::from(Span::styled(
            format!("#{}{who} · {branches}", pr.number),
            dim,
        ))];
        let title = format!("**{}**", pr.title.replace('*', "\\*"));
        out.extend(markdown::render(&title, width, &self.palette));
        if !pr.body.trim().is_empty() {
            out.push(Line::default());
            out.extend(markdown::render(&pr.body, width, &self.palette));
        }
        out
    }

    /// The description block for the current view, wrapped to `width`.
    fn description(&self, width: u16) -> Vec<Line<'static>> {
        let Some(loc) = self.cur() else {
            return Vec::new();
        };
        if loc.focus != Focus::All || loc.code.is_some() {
            return Vec::new();
        }
        let Some(info) = &loc.info else {
            return Vec::new();
        };
        if let Some(pr) = loc.pr.as_ref().and_then(|p| p.info.as_ref())
            && info.range == Range::Everything
        {
            return if self.show_pr_desc {
                self.pr_description(pr, width)
            } else {
                Vec::new()
            };
        }
        if !self.show_commit_desc {
            return Vec::new();
        }
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
            .map(|f| {
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
                    action: Action::GoToFile,
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
            Action::PreviousLocation | Action::NextLocation | Action::CloseLocation => several,
            Action::Commits => has_info,
            Action::NextMark | Action::PrevMark => loc.view.has_marks(),
            Action::Comment | Action::MarkRegion => has_rows,
            Action::EditComment | Action::DeleteComment => loc.view.comment_at_cursor().is_some(),
            Action::ClearComments => loc.comments() > 0,
            Action::OlderCommit | Action::NewerCommit => loc
                .info
                .as_ref()
                .is_some_and(|i| !i.commits.is_empty() || i.dirty),
            Action::ShowCommit(_) => loc.repo,
            Action::ToggleDescription => {
                matches!(loc.range, Range::Commit(_))
                    || loc.pr.is_some() && loc.range == Range::Everything
            }
            Action::Files | Action::Narrow | Action::GoToFile => has_rows,
            Action::NextHunk
            | Action::PrevHunk
            | Action::NextFile
            | Action::PrevFile
            | Action::Top
            | Action::Bottom => has_rows,
            Action::RangeEverything => loc.repo && loc.range != Range::Everything,
            Action::RangeUncommitted => loc.repo && has_base && loc.range != Range::Uncommitted,
            Action::Reload => loc.repo || loc.pr.is_some(),
            Action::Quit => true,
        }
    }

    // ---- holding an agent's switch ------------------------------------------

    /// The user is in the middle of writing or answering something.
    fn typing(&self) -> bool {
        self.marking.is_some() || self.composing.is_some() || self.confirm.is_some()
    }

    /// Whether showing `target` would move away from the location on
    /// screen, as `show_view_inner` would resolve it.
    fn would_switch(&self, target: &Target, view: Option<&crate::proto::ViewSpec>) -> bool {
        let Some(cur) = self.cur() else { return false };
        if let Some(spec) = view
            .and_then(|v| v.range.as_deref())
            .and_then(crate::pr::parse)
        {
            return cur.pr.as_ref().is_none_or(|p| p.spec != spec);
        }
        let dir = if target.repo {
            checkout_root(&target.dir)
        } else {
            target.dir.clone()
        };
        cur.dir != dir
    }

    /// A request that would take the user away from a review they are in
    /// the middle of waits: while they type, or hold comments no agent has
    /// read.
    fn held(&self, target: &Target, view: Option<&crate::proto::ViewSpec>) -> bool {
        self.would_switch(target, view)
            && (self.typing() || self.cur().is_some_and(|l| l.unread() > 0))
    }

    fn hold(
        &mut self,
        target: Target,
        view: Option<crate::proto::ViewSpec>,
    ) -> crate::proto::ShowAck {
        let words = describe_request(&target, view.as_ref());
        // Only the latest request waits.
        self.pending_show = Some(PendingShow {
            target: target.clone(),
            view,
            words,
        });
        crate::proto::ShowAck {
            location: String::new(),
            dir: target.dir,
            marks: Vec::new(),
            unread: self.cur().map_or(0, Location::unread),
            applied: false,
        }
    }

    /// Show what the agent asked for while the user was busy.
    fn apply_pending(&mut self) {
        let Some(p) = self.pending_show.take() else {
            return;
        };
        match p.view {
            Some(view) => {
                self.show_view(p.target, view);
            }
            None => self.open(p.target),
        }
        self.message = Some(format!("switched to {}, as the agent asked", p.words));
    }

    // ---- comments ------------------------------------------------------------

    /// Enter: a comment on the cursor's line, or edit the comment whose row
    /// the cursor is on.
    fn comment_here(&mut self) {
        let Some(loc) = self.cur() else { return };
        let cursor = loc.view.cursor();
        if let Some(seq) = loc.view.comment_at_cursor()
            && loc.view.region(cursor, cursor).is_none()
        {
            self.edit(seq);
            return;
        }
        // On an agent's highlight, the comment answers it on its lines.
        if let Some(region) = loc.view.highlight_at_cursor() {
            self.compose(region, None);
            return;
        }
        match loc.view.region(cursor, cursor) {
            Some(region) => self.compose(region, None),
            None => self.message = Some("move onto a line first".to_string()),
        }
    }

    fn start_region(&mut self) {
        let Some(loc) = self.cur() else { return };
        let cursor = loc.view.cursor();
        if loc.view.region(cursor, cursor).is_some() {
            self.marking = Some(cursor);
        } else {
            self.message = Some("move onto a line first".to_string());
        }
    }

    fn end_region(&mut self) {
        let Some(from) = self.marking.take() else {
            return;
        };
        let Some(loc) = self.cur() else { return };
        match loc.view.region(from, loc.view.cursor()) {
            Some(region) => self.compose(region, None),
            None => self.message = Some("nothing to comment on".to_string()),
        }
    }

    /// Open the prompt for a comment on `region`: a new one, or the one
    /// `editing` names, whose text is put up to change.
    fn compose(&mut self, region: diffview::Region, editing: Option<usize>) {
        let Some(loc) = self.cur() else { return };
        let existing = editing.and_then(|seq| loc.marks.iter().find(|m| m.seq == seq));
        let text = existing.and_then(|m| m.note.clone()).unwrap_or_default();
        let where_ = match (region.start == region.end, region.old_side) {
            (true, false) => format!("{} {}", region.path, region.start),
            (false, false) => format!("{} {}-{}", region.path, region.start, region.end),
            (true, true) => format!("{} old {}", region.path, region.start),
            (false, true) => format!("{} old {}-{}", region.path, region.start, region.end),
        };
        let note = match (existing, &region.re) {
            (Some(m), _) => format!("[{}] {where_}", m.label),
            (None, Some(re)) => format!("re [{re}] {where_}"),
            (None, None) => where_,
        };
        let mut panel = Panel::new(panel::Kind::Comment, Vec::new(), Vec::new(), None);
        panel.set_input(text);
        panel.note = Some(note);
        self.panel = Some(panel);
        self.composing = Some((region, editing));
    }

    fn edit(&mut self, seq: usize) {
        let Some(loc) = self.cur() else { return };
        let Some(m) = loc.marks.iter().find(|m| m.seq == seq) else {
            return;
        };
        let region = diffview::Region {
            path: m.path.clone(),
            start: m.start,
            end: m.end,
            old_side: m.old_side,
            with_deletions: m.with_deletions,
            text: m.text.clone().unwrap_or_default(),
            re: m.re.clone(),
        };
        self.compose(region, Some(seq));
    }

    fn edit_comment(&mut self) {
        match self.cur().and_then(|l| l.view.comment_at_cursor()) {
            Some(seq) => self.edit(seq),
            None => self.message = Some("no comment of yours here".to_string()),
        }
    }

    /// The prompt was answered: a new comment, or new text for one.
    fn save_comment(&mut self, text: String) {
        let Some((region, editing)) = self.composing.take() else {
            return;
        };
        let Some(loc) = self.cur_mut() else { return };
        let note = Some(text);
        let label = match editing.and_then(|seq| loc.marks.iter_mut().find(|m| m.seq == seq)) {
            Some(m) => {
                m.note = note;
                m.seen = false;
                m.label.clone()
            }
            None => {
                let m = loc.add_mark(
                    diffview::Author::User,
                    region.path,
                    region.start,
                    region.end,
                    note,
                );
                m.old_side = region.old_side;
                m.with_deletions = region.with_deletions;
                m.text = Some(region.text);
                m.re = region.re;
                m.label.clone()
            }
        };
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        self.message = Some(format!("[{label}] saved"));
    }

    fn delete_comment(&mut self) {
        let Some(loc) = self.cur_mut() else { return };
        let Some(seq) = loc.view.comment_at_cursor() else {
            self.message = Some("no comment of yours here".to_string());
            return;
        };
        let label = loc
            .marks
            .iter()
            .find(|m| m.seq == seq)
            .map(|m| m.label.clone())
            .unwrap_or_default();
        loc.marks.retain(|m| m.seq != seq);
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        self.message = Some(format!("[{label}] removed"));
    }

    fn clear_comments(&mut self) {
        let Some(loc) = self.cur_mut() else { return };
        let n = loc.comments();
        loc.marks.retain(|m| m.author != diffview::Author::User);
        loc.view
            .set_marks(loc.marks.iter().map(LocMark::drawn).collect());
        let s = if n == 1 { "" } else { "s" };
        self.message = Some(format!("{n} comment{s} cleared"));
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
        // Diff-only actions first go back from files as they are to the diff.
        if matches!(
            action,
            Action::Commits
                | Action::OlderCommit
                | Action::NewerCommit
                | Action::ShowCommit(_)
                | Action::RangeEverything
                | Action::RangeUncommitted
                | Action::ToggleDescription
        ) {
            self.leave_code();
        }
        match action {
            Action::Quit => self.quit = true,
            Action::Files => self.open_files(),
            Action::Open => self.open_prompt(),
            Action::OpenRecent(i) => {
                if let Some(e) = self.open_choices.get(i).cloned() {
                    if let Some(spec) = e.pr.as_ref().and_then(|p| crate::pr::parse(&p.url)) {
                        self.open_pr(spec);
                    } else if e.dir.is_dir() {
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
            Action::PreviousLocation => self.flip(true),
            Action::NextLocation => self.flip(false),
            Action::CloseLocation => self.close_location(),
            Action::Commits => self.open_commits(),
            Action::NextMark | Action::PrevMark => {
                let Some(loc) = self.cur_mut() else { return };
                let v = &mut loc.view;
                // Highlights when there are any, else hunks.
                match (action == Action::NextMark, v.has_marks()) {
                    (true, true) => v.next_mark(),
                    (false, true) => v.prev_mark(),
                    (true, false) => v.next_hunk(),
                    (false, false) => v.prev_hunk(),
                }
            }
            Action::Comment => self.comment_here(),
            Action::MarkRegion => match self.marking {
                Some(_) => self.end_region(),
                None => self.start_region(),
            },
            Action::EditComment => self.edit_comment(),
            Action::DeleteComment => self.delete_comment(),
            Action::ClearComments => {
                let n = self.cur().map_or(0, Location::comments);
                if n == 0 {
                    self.message = Some("no comments of yours here".to_string());
                } else {
                    let s = if n == 1 { "" } else { "s" };
                    self.message = Some(format!("clear your {n} comment{s} here? y/n"));
                    self.confirm = Some(Confirm::ClearComments);
                }
            }
            Action::OlderCommit => self.step(true),
            Action::NewerCommit => self.step(false),
            Action::ShowCommit(id) => self.set_range(Range::Commit(id)),
            Action::ToggleDescription => {
                let commit = self
                    .cur()
                    .is_some_and(|l| matches!(l.range, Range::Commit(_)));
                if commit {
                    self.show_commit_desc = !self.show_commit_desc;
                } else {
                    self.show_pr_desc = !self.show_pr_desc;
                }
                if let Some(loc) = self.cur_mut() {
                    loc.desc_dirty = true;
                }
            }
            // Handled where the file list's path is known.
            Action::GoToFile => {}
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
                if self
                    .cur()
                    .is_some_and(|l| matches!(l.focus, Focus::File(_))) =>
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
            Action::Reload if self.cur().is_some_and(|l| l.pr.is_some()) => self.refetch_pr(),
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
                    Action::Top => v.go_top(),
                    Action::Bottom => v.go_bottom(),
                    _ => {}
                }
            }
        }
    }

    // ---- words ---------------------------------------------------------------

    /// What the screen shows, in words: the left side of the status bar.
    fn scope(&self) -> String {
        let scope = self.range_scope();
        let Some(loc) = self.cur() else {
            return scope;
        };
        // After an agent's call, Esc goes back to where you were instead.
        let agent = loc.agent_return.is_some();
        let esc = if agent { "" } else { " · Esc for all" };
        let scope = match &loc.focus {
            // Narrowed first: on a narrow terminal the scope is what gets cut.
            Focus::File(path) => format!("{path} only{esc} · {scope}"),
            Focus::Files(paths) => format!("{} files only{esc} · {scope}", paths.len()),
            Focus::All => scope,
        };
        if agent {
            format!("{scope} · Esc returns")
        } else {
            scope
        }
    }

    /// What the screen shows, for an agent: the scope without key hints.
    fn scope_words(&self) -> String {
        let scope = self.range_scope();
        let Some(loc) = self.cur() else {
            return scope;
        };
        match &loc.focus {
            Focus::File(path) => format!("{path} only · {scope}"),
            Focus::Files(paths) => format!("{} files only · {scope}", paths.len()),
            Focus::All => scope,
        }
    }

    fn range_scope(&self) -> String {
        let Some(loc) = self.cur() else {
            return String::new();
        };
        if let Some(code) = &loc.code {
            let what = match code.files.as_slice() {
                [one] => one.path.clone(),
                many => format!("{} files", many.len()),
            };
            return match (&loc.code_rev, &code.rev) {
                (Some(r), _) | (None, Some(r)) => format!("{what} @ {r}"),
                (None, None) => format!("{what} · working tree"),
            };
        }
        if let Some(p) = &loc.pr {
            let n = p.spec.number;
            let doing = if p.fetching {
                "fetching"
            } else {
                "could not be opened"
            };
            let Some(info) = &p.info else {
                return format!("#{n} · {doing}");
            };
            if !p.ready {
                return format!("#{n} {} · {doing}", info.title);
            }
            match loc.range {
                Range::Everything => {
                    let mut s = format!("#{n} {} · into {}", info.title, info.base_ref_name);
                    if info.is_draft {
                        s.push_str(" · draft");
                    }
                    match info.state.as_str() {
                        "MERGED" => s.push_str(" · merged"),
                        "CLOSED" => s.push_str(" · closed"),
                        _ => {}
                    }
                    return s;
                }
                Range::Uncommitted => return format!("#{n} · local edits"),
                Range::Commit(_) | Range::Compare(..) => {}
            }
        }
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
            Range::Compare(..) => format!(
                "{head} · {}",
                loc.range_label.as_deref().unwrap_or("comparison")
            ),
            Range::Commit(id) => {
                let n = info.commits.len();
                match info.commits.iter().position(|c| c.id == *id) {
                    Some(i) => {
                        let c = &info.commits[i];
                        format!(
                            "{COMMIT_ICON} commit {} of {n} · {} {}",
                            i + 1,
                            c.short,
                            c.subject
                        )
                    }
                    None => match &info.commit {
                        Some(c) if c.id == *id => {
                            format!("{COMMIT_ICON} {} {}", c.short, c.subject)
                        }
                        _ => format!("{head} · commit"),
                    },
                }
            }
        }
    }

    /// The centered text shown instead of a diff, if any.
    fn notice(&self) -> Option<Vec<String>> {
        let loc = self.cur()?;
        if let Some(e) = &loc.error {
            return Some(vec![e.clone()]);
        }
        if let Some(p) = loc.pr.as_ref().filter(|p| !p.ready) {
            let n = p.spec.number;
            let from = p
                .info
                .as_ref()
                .and_then(|i| i.repo())
                .or_else(|| p.spec.repo.clone())
                .map(|r| format!(" from {}", r.slug()))
                .unwrap_or_default();
            let mut lines = vec![format!("Fetching #{n}{from}")];
            if let Some(i) = &p.info {
                lines.push(i.title.clone());
            }
            lines.push("Esc cancels".to_string());
            return Some(lines);
        }
        if !loc.repo {
            return Some(vec![
                "not inside a git repository".to_string(),
                loc.dir.display().to_string(),
            ]);
        }
        let stale = match &loc.code {
            Some(code) => loc.code_shown.as_ref() != Some(code),
            None => {
                loc.code_shown.is_some() || loc.info.as_ref().is_none_or(|i| i.range != loc.range)
            }
        };
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
            (Range::Compare(..), _) => "no differences".to_string(),
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
        // The banner for a held request takes the top line of the body.
        let banner = self.pending_show.as_ref().map(|p| {
            let banner = Rect::new(body.x, body.y, body.width, 1);
            body.y += 1;
            body.height = body.height.saturating_sub(1);
            (banner, p.words.clone())
        });
        self.body_h = body.height as usize;
        self.body_y = body.y;
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
        let marking = self.marking;
        if let Some(loc) = self.locations.first_mut() {
            let cursor = loc.view.cursor();
            loc.view.marking = marking.map(|from| (from.min(cursor), from.max(cursor)));
            loc.view.resize(self.body_h);
            // A target line inside a highlight opens at that highlight, its
            // note at the top; any other target line sits about a third of
            // the way down. Without one the view opens with ① at the top.
            if std::mem::take(&mut loc.view.place_target) {
                let target = loc.view.target.clone();
                let on_mark = target
                    .as_ref()
                    .is_some_and(|(p, l)| loc.view.mark_at(p, *l));
                if !on_mark {
                    match target.and_then(|(p, l)| loc.view.row_of_line(&p, l)) {
                        Some(row) => loc.view.center_on(row),
                        None => {
                            loc.view.first_mark();
                        }
                    }
                }
            }
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
        if let Some((area, words)) = banner {
            let style = Style::default()
                .bg(self.palette.comment_bg)
                .fg(self.palette.comment_fg);
            buf.set_style(area, style);
            let line = Line::from(vec![
                Span::styled(
                    format!(" the agent wants to show {words} "),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled("· y switches · Esc drops it ", style),
            ]);
            buf.set_line(area.x, area.y, &line, area.width);
        }
        self.render_status(status, buf);
        let cursor = self
            .panel
            .as_mut()
            .filter(|_| panel_area.height >= 2)
            .map(|p| p.render(panel_area, buf, &self.palette, &info));
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }

    fn render_notice(&self, lines: &[String], body: Rect, buf: &mut Buffer) {
        let top = body.y + body.height.saturating_sub(lines.len() as u16) / 2;
        for (i, text) in lines.iter().enumerate() {
            if top + i as u16 >= body.bottom() {
                break;
            }
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
                let name = format!(" {} ", l.name());
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
        if let Some(from) = self.marking
            && let Some(loc) = self.cur()
        {
            let lines = match loc.view.region(from, loc.view.cursor()) {
                Some(r) if r.start == r.end => format!("line {}", r.start),
                Some(r) => format!("lines {}-{}", r.start, r.end),
                None => "nothing yet".to_string(),
            };
            let text = format!(" marking {lines} · Enter or Ctrl-Space to comment · Esc cancels");
            buf.set_span(area.x, area.y, &Span::styled(text, style), area.width);
            return;
        }
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
            && loc.code_shown.is_some()
            && loc.view.has_files()
        {
            // Files as they are: no changes to count.
            let n = loc.view.files.len();
            if n > 1 {
                let pos = loc.view.current_file().map_or(0, |i| i + 1);
                right = format!(" file {pos} of {n}  ·");
            }
        } else if let Some(loc) = self.cur()
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
                    let shown = loc.view.shown();
                    let k = shown.len();
                    let noun = if n == 1 { "file" } else { "files" };
                    // Several files of the diff, as an agent asked for.
                    let count = if k < n {
                        format!("{k} of {n} {noun}")
                    } else {
                        format!("{n} {noun}")
                    };
                    let at = cur.and_then(|fi| shown.iter().position(|&s| s == fi));
                    match at {
                        Some(i) => format!(" {count}  +{added} −{removed}  ·  {}/{k}  ·", i + 1),
                        // Above the first file, in the description.
                        None => format!(" {count}  +{added} −{removed}  ·"),
                    }
                }
            };
        }
        match self.cur().and_then(|l| l.view.mark_position()) {
            Some((Some((label, i)), n)) => {
                right.insert_str(0, &format!(" [{label}] {i} of {n}  ·"))
            }
            Some((None, 1)) => right.insert_str(0, " 1 mark  ·"),
            Some((None, n)) => right.insert_str(0, &format!(" {n} marks  ·")),
            None => {}
        }
        if let Some(loc) = self.cur().filter(|l| l.comments() > 0) {
            let unread = match loc.unread() {
                0 => String::new(),
                n => format!(" ({n} unread)"),
            };
            right.insert_str(0, &format!(" {} yours{unread}  ·", loc.comments()));
        }
        if let Some(loc) = self.cur().filter(|l| l.repo) {
            right.insert_str(0, &format!(" {}  ·", loc.label()));
        }
        // Several marks under the cursor: all their letters, newest last.
        if let Some(labels) = self.cur().map(|l| l.view.labels_at_cursor())
            && labels.len() > 1
        {
            let all: Vec<String> = labels.iter().map(|l| format!("[{l}]")).collect();
            right.insert_str(0, &format!(" {}  ·", all.join(" ")));
        }
        right.push_str(" : commands ");
        let rw = right.chars().count() as u16;
        let lw = area.width.saturating_sub(rw);
        buf.set_span(area.x, area.y, &Span::styled(left, style), lw);
        if area.width > rw {
            buf.set_span(area.right() - rw, area.y, &Span::styled(right, style), rw);
        }
    }
}

/// Narrow `view` to what `focus` names. False when the shown range has none
/// of it, in which case every file shows.
fn narrow_view(view: &mut DiffView, focus: &Focus) -> bool {
    match focus {
        Focus::All => {
            view.narrow_to(None);
            true
        }
        Focus::File(path) => view.narrow_to_path(Some(path)),
        Focus::Files(paths) => view.narrow_to_paths(paths) > 0,
    }
}

/// What to say when a focus can no longer be shown.
fn focus_lost(focus: &Focus) -> String {
    match focus {
        Focus::File(path) => format!("{path} has no changes here; showing all files"),
        _ => "none of those files has changes here; showing all files".to_string(),
    }
}

/// Drop a location's highlights, and the lines shown only for them.
/// The root of the checkout `dir` is in, so every way of naming a
/// repository lands on the same location.
fn checkout_root(dir: &std::path::Path) -> PathBuf {
    gix::discover(dir)
        .ok()
        .and_then(|r| r.workdir().map(|w| w.to_path_buf()))
        .and_then(|w| std::fs::canonicalize(w).ok())
        .unwrap_or_else(|| dir.to_path_buf())
}

/// A directory's short name.
/// An agent's request in a few words: the checkout, and the files, range
/// or PR it asks for.
fn describe_request(target: &Target, view: Option<&crate::proto::ViewSpec>) -> String {
    let name = dir_name(&target.dir);
    let Some(view) = view else {
        return name;
    };
    if let Some(code) = &view.code {
        return match code.files.as_slice() {
            [one] => format!("{name}: {}", one.path),
            many => format!("{name}: {} files", many.len()),
        };
    }
    match (&view.range, view.files.as_slice()) {
        (Some(r), _) if crate::pr::parse(r).is_some() => r.clone(),
        (Some(r), _) => format!("{name}: {r}"),
        (None, []) => format!("{name}: the diff"),
        (None, [one]) => format!("{name}: {one}"),
        (None, many) => format!("{name}: {} files", many.len()),
    }
}

fn dir_name(dir: &std::path::Path) -> String {
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

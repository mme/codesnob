//! Wire protocol and socket conventions shared by the daemon, the TUI and
//! one-shot commands. The marks and locations themselves live in `marks`
//! and `registry`; this is how they travel. One JSON object per newline-terminated line in both
//! directions; requests carry an id, events do not.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 3;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SOCKET_PATH: usize = 100;

// ---- paths ------------------------------------------------------------------

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "codesnob-dev"
    } else {
        "codesnob"
    }
}

fn short_runtime_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("{}-{uid}", app_dir_name()))
}

pub fn runtime_dir() -> PathBuf {
    let preferred = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join(app_dir_name()),
        _ => return short_runtime_dir(),
    };
    if preferred.as_os_str().len() + "/codesnobd.sock".len() > MAX_SOCKET_PATH {
        short_runtime_dir()
    } else {
        preferred
    }
}

pub fn daemon_socket() -> PathBuf {
    match std::env::var_os("CODESNOB_DAEMON_SOCKET") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => runtime_dir().join("codesnobd.sock"),
    }
}

pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Our own executable, tolerant of the binary having been replaced on disk.
pub fn own_binary() -> PathBuf {
    match std::env::current_exe() {
        Ok(p) => {
            let s = p.to_string_lossy();
            match s.strip_suffix(" (deleted)") {
                Some(stripped) => PathBuf::from(stripped),
                None => p,
            }
        }
        Err(_) => PathBuf::from("codesnob"),
    }
}

// ---- frames -----------------------------------------------------------------

pub use crate::marks::{MAX_MARKS, MarkInfo, MarkSpec};
pub use crate::registry::{LocationKey, LocationRef, LocationState};

/// What the daemon currently asks viewers to display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Whether `dir` was inside a git repository when it was set.
    pub repo: bool,
}

/// What an agent asks a viewer to show in a checkout. Fields are optional
/// and unknown ones ignored, so older and newer versions can talk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSpec {
    /// A comparison in git syntax, `worktree`, `staged`, `unstaged`, or a
    /// PR. None: everything since the branch point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// Only these files, relative to the repository root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Show files as they are instead of a diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeSpec>,
    /// Marks to place, lettered in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<MarkSpec>,
}

/// What an agent asks the daemon about a location's marks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentQuery {
    /// Every location open, the one on screen first.
    Locations,
    /// Every mark on a location; the user's words count as read
    /// afterwards.
    Review {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<LocationRef>,
    },
    /// Remove marks: `letters` names them, else `which` of "mine" (the
    /// agent's own), "user" or "all". A mark the user made or wrote under
    /// goes whole, and only with `user_asked`: the user explicitly asked
    /// for that.
    ClearMarks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<LocationRef>,
        which: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        letters: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_asked: bool,
    },
    /// Marks of the agent's with words on lines, without a show.
    Mark {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<LocationRef>,
        marks: Vec<MarkSpec>,
    },
    /// Answer under a mark the user made or wrote under.
    Reply {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<LocationRef>,
        letter: String,
        text: String,
    },
    /// Move a mark to other lines, of `path` or of its own file, keeping
    /// its letter, text and thread.
    MoveMark {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<LocationRef>,
        letter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        start: usize,
        end: usize,
    },
}

/// A location as listed for an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationInfo {
    /// How to name it: the checkout's path, or `owner/repo#142`.
    pub location: String,
    /// "#142 Fix the parser" for a pull request, else the directory's name.
    pub name: String,
    pub dir: PathBuf,
    /// What it shows, in words; empty when no viewer has it open.
    pub showing: String,
    /// On screen in the viewer the agent's requests reach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_screen: bool,
    /// The viewers it is open in, by id and workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewers: Vec<String>,
    pub marks: usize,
    /// The user's words: the marks they made, and their turns in threads.
    pub by_user: usize,
    /// Those the agent has not read.
    pub unread: usize,
}

impl LocationInfo {
    pub fn render(&self) -> String {
        let mut out = match &self.name {
            n if n == &self.location => self.location.clone(),
            n => format!("{n} · {}", self.location),
        };
        if self.on_screen {
            out.push_str(" · on screen");
        } else if !self.viewers.is_empty() {
            out.push_str(&format!(" · open in {}", self.viewers.join(", ")));
        } else {
            out.push_str(" · not open in any viewer");
        }
        if !self.showing.is_empty() {
            out.push_str(&format!(" · {}", self.showing));
        }
        out.push_str(&match self.marks {
            0 => " · no marks".to_string(),
            1 => " · 1 mark".to_string(),
            n => format!(" · {n} marks"),
        });
        if self.by_user > 0 {
            out.push_str(&format!(", {} by the user", self.by_user));
        }
        if self.unread > 0 {
            out.push_str(&format!(", {} unread", self.unread));
        }
        out
    }
}

/// The locations open, as answered to an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationsResult {
    /// The one on screen first.
    pub locations: Vec<LocationInfo>,
}

impl LocationsResult {
    pub fn render(&self) -> String {
        let n = self.locations.len();
        if n == 0 {
            return "no locations: nothing has been shown or commented on yet".to_string();
        }
        let on_screen = self.locations.first().is_some_and(|l| l.on_screen);
        let mut out = format!(
            "{n} location{}{}",
            if n == 1 { "" } else { "s" },
            if on_screen {
                ", the one on screen first"
            } else {
                ""
            }
        );
        for l in &self.locations {
            out.push('\n');
            out.push_str(&l.render());
        }
        let unread: usize = self.locations.iter().map(|l| l.unread).sum();
        if unread > 0 {
            out.push_str(
                "\nread unread comments with get_review, naming the location by repo or pr",
            );
        }
        out
    }
}

/// What `get_review` answers with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// How to name it: the checkout's path, or `owner/repo#142`.
    pub location: String,
    pub dir: PathBuf,
    /// What the location shows, in words.
    pub showing: String,
    pub marks: Vec<MarkInfo>,
    /// Words of the user's the agent had not read before this.
    pub unread: usize,
}

impl ReviewResult {
    pub fn render(&self) -> String {
        let mut out = format!("{} · {}", self.location, self.showing);
        if self.marks.is_empty() {
            out.push_str("\nno marks");
            return out;
        }
        let comments = self
            .marks
            .iter()
            .filter(|m| m.author == crate::marks::Author::User)
            .count();
        let turns: usize = self.marks.iter().map(|m| m.thread.len()).sum();
        out.push_str(&format!(
            "\n{} mark{}, {comments} comment{} by the user{}{}",
            self.marks.len(),
            if self.marks.len() == 1 { "" } else { "s" },
            if comments == 1 { "" } else { "s" },
            match turns {
                0 => String::new(),
                1 => ", 1 reply".to_string(),
                n => format!(", {n} replies"),
            },
            match self.unread {
                0 => String::new(),
                n => format!(", {n} new"),
            }
        ));
        for m in &self.marks {
            out.push('\n');
            out.push_str(&m.render());
        }
        out
    }
}

/// What a viewer answers a show request with, once it has applied it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowAck {
    /// How to name the location: the checkout's path, or `owner/repo#142`.
    pub location: String,
    pub dir: PathBuf,
    /// The marks the call placed, with their letters.
    pub marks: Vec<MarkInfo>,
    /// Every mark of the agent's on the location afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mine: Vec<MarkInfo>,
    /// The user's words the agent has not read.
    pub unread: usize,
    /// Whether the view changed now, or waits for the user.
    pub applied: bool,
}

/// What `clear_marks` answers with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearResult {
    pub location: String,
    pub cleared: Vec<String>,
    pub remaining: usize,
    pub unread: usize,
}

impl ClearResult {
    pub fn render(&self) -> String {
        let what = if self.cleared.is_empty() {
            "nothing to clear".to_string()
        } else {
            format!(
                "cleared {}",
                self.cleared
                    .iter()
                    .map(|l| format!("[{l}]"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        format!(
            "{what} on {}; {} mark{} remain{}",
            self.location,
            self.remaining,
            if self.remaining == 1 { "" } else { "s" },
            if self.remaining == 1 { "s" } else { "" }
        )
    }
}

/// What a reply is answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    pub location: String,
    /// The letter of the mark the reply went under.
    pub letter: String,
    pub unread: usize,
}

impl ReplyResult {
    pub fn render(&self) -> String {
        format!("replied under [{}] on {}", self.letter, self.location)
    }
}

/// What marking is answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkResult {
    pub location: String,
    /// The marks placed, with their letters, in the order given.
    pub marks: Vec<MarkInfo>,
    /// Every mark of the agent's on the location afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mine: Vec<MarkInfo>,
    pub unread: usize,
}

impl MarkResult {
    /// "[A] src/x.rs 1-2, [B] src/y.rs 3".
    pub fn placed(&self) -> String {
        self.marks
            .iter()
            .map(|m| format!("[{}] {} {}", m.label, m.path, m.lines_word()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn render(&self) -> String {
        format!("marked {} on {}", self.placed(), self.location)
    }
}

/// What moving a mark is answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveResult {
    pub location: String,
    pub label: String,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub unread: usize,
}

impl MoveResult {
    pub fn lines_word(&self) -> String {
        if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{}-{}", self.start, self.end)
        }
    }

    pub fn render(&self) -> String {
        format!(
            "moved [{}] to {} {} on {}",
            self.label,
            self.path,
            self.lines_word(),
            self.location
        )
    }
}

/// The agent's own marks on a location, by file, with the reminder to
/// clean up; for every reply that leaves some.
pub fn mine_note(location: &str, mine: &[MarkInfo]) -> String {
    if mine.is_empty() {
        return String::new();
    }
    let mut files: Vec<(&str, Vec<String>)> = Vec::new();
    for m in mine {
        let letter = format!("[{}]", m.label);
        match files.iter_mut().find(|(p, _)| *p == m.path) {
            Some((_, letters)) => letters.push(letter),
            None => files.push((&m.path, vec![letter])),
        }
    }
    let each: Vec<String> = files
        .iter()
        .map(|(p, letters)| format!("{} {p}", letters.join(" ")))
        .collect();
    format!(
        "; your marks on {location}: {}; remember to remove any marks that are not needed any more",
        each.join(", ")
    )
}

/// The agent's nudge about the user's words it has not read, for every
/// reply.
pub fn unread_note(location: &str, unread: usize) -> String {
    match unread {
        0 => String::new(),
        1 => format!("; the user has 1 unread comment on {location}, call get_review"),
        n => format!("; the user has {n} unread comments on {location}, call get_review"),
    }
}

/// Files to show as they are.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSpec {
    /// A revision to show them at; `None` is the working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub files: Vec<CodeFile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeFile {
    /// Relative to the repository root.
    pub path: String,
    /// Open at this line, marked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Show only the line and marks with a few lines around them
    /// (true), or the whole file (false). Unset: whole when it is the only
    /// file, excerpts when there are several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpts: Option<bool>,
}

/// A location a viewer has open, as it tells the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenLocation {
    pub key: LocationKey,
    /// What it shows there, in words.
    pub showing: String,
}

/// A connected viewer. Re-sent in full whenever anything changes; the
/// daemon keeps it until the viewer's connection closes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerEntry {
    pub id: String,
    pub pid: u32,
    /// Unix seconds when the viewer started.
    pub since: u64,
    /// Which desktop backend produced the placement, e.g. "hyprland".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Compositor-native window token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Display name of the workspace holding the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<crate::wm::Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// The location on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationKey>,
    /// Every location open there, the one on screen first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<OpenLocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    Hello {
        protocol: u32,
        version: String,
    },
    /// Display `dir` in one viewer: the first on `workspace`, else the
    /// nearest workspace, else the first viewer. Errors when none is connected.
    Show {
        dir: PathBuf,
        #[serde(default)]
        base: Option<String>,
        /// The caller's own workspace, when it could be determined.
        #[serde(default)]
        workspace: Option<String>,
        /// What to show there, for an agent's request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ViewSpec>,
    },
    /// An agent's question about marks, answered by the daemon. Without a
    /// location named, the one on screen in the viewer picked as for
    /// `Show`, else the one for `cwd`.
    Query {
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        query: AgentQuery,
    },
    /// A viewer's answer to a `Show` event, by the event's id.
    #[serde(rename = "viewer.reply")]
    ViewerReply {
        id: String,
        result: Value,
    },
    Status,
    Subscribe,
    #[serde(rename = "viewer.register")]
    ViewerRegister(ViewerEntry),
    /// A viewer opened a location, or reconnected with it open: this is
    /// its copy. The daemon answers with a `location` event carrying the
    /// shared one.
    #[serde(rename = "viewer.location")]
    ViewerLocation(LocationState),
    /// A viewer changed a location's marks.
    #[serde(rename = "viewer.op")]
    ViewerOp {
        key: LocationKey,
        op: crate::marks::Op,
    },
    /// A viewer closed a location.
    #[serde(rename = "viewer.close")]
    ViewerClose {
        key: LocationKey,
    },
    #[serde(rename = "daemon.shutdown")]
    DaemonShutdown,
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub version: String,
    pub target: Option<Target>,
}

#[derive(Serialize, Deserialize)]
pub struct ShowResult {
    pub repo: bool,
    /// The viewer that was told to display the target.
    pub viewer: String,
    pub workspace: Option<String>,
    /// How the viewer was chosen: "same", "nearest" or "first".
    pub picked_by: String,
    /// What the viewer made of it; `None` when it did not answer in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<ShowAck>,
}

#[derive(Serialize, Deserialize)]
pub struct StatusResult {
    pub target: Option<Target>,
    pub viewers: Vec<ViewerEntry>,
    /// Every location the daemon keeps.
    #[serde(default)]
    pub locations: Vec<LocationInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "params", rename_all = "snake_case")]
pub enum Event {
    /// Addressed to one viewer; others ignore it. The viewer answers with
    /// `viewer.reply` under `id` when there is one.
    Show {
        viewer: String,
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ViewSpec>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// A location's marks as the daemon has them, after any change. Every
    /// viewer with it open takes them over. `note` says what an agent did,
    /// for the viewer showing the location to tell the user.
    Location {
        state: LocationState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    ViewerConnected(ViewerEntry),
    ViewerUpdated(ViewerEntry),
    ViewerDisconnected {
        id: String,
    },
}

pub enum ServerFrame {
    Response(Response),
    Event(Event),
    /// An event this version does not know; readers skip it.
    Unknown,
}

impl ServerFrame {
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        let v: Value = serde_json::from_str(line)?;
        if v.get("event").is_some() {
            Ok(match serde_json::from_value::<Event>(v) {
                Ok(e) => ServerFrame::Event(e),
                Err(_) => ServerFrame::Unknown,
            })
        } else {
            Ok(ServerFrame::Response(serde_json::from_value(v)?))
        }
    }
}

pub fn to_line<T: Serialize>(frame: &T) -> String {
    let mut s = serde_json::to_string(frame).expect("frames serialize");
    s.push('\n');
    s
}

// ---- client -----------------------------------------------------------------

/// Connect to the daemon, starting one on demand.
pub fn connect_or_spawn() -> Option<UnixStream> {
    // A daemon from an older build makes way for one from this build,
    // which asks it to exit as it starts.
    let current = |s: UnixStream| -> Option<UnixStream> {
        // The probe waits only so long; the stream goes on without a limit.
        let wait = Some(Duration::from_secs(2));
        s.set_read_timeout(wait).ok()?;
        s.set_write_timeout(wait).ok()?;
        let mut probe = Client::new(s.try_clone().ok()?, "probe-").ok()?;
        let hello = probe.hello().ok()?;
        s.set_read_timeout(None).ok()?;
        s.set_write_timeout(None).ok()?;
        (!crate::daemon::outdated(&hello)).then_some(s)
    };
    if let Ok(s) = UnixStream::connect(daemon_socket())
        && let Some(s) = current(s)
    {
        return Some(s);
    }
    spawn_daemon();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        if let Ok(s) = UnixStream::connect(daemon_socket())
            && let Some(s) = current(s)
        {
            return Some(s);
        }
    }
    None
}

fn spawn_daemon() {
    use std::process::{Command, Stdio};
    let _ = Command::new(own_binary())
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// A simple request/response client over one connection.
pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    id_prefix: &'static str,
    /// Event lines that arrived while a call waited for its response;
    /// the reader hands them out first.
    stashed: VecDeque<String>,
}

/// The daemon's frames, one line each, in the order they came.
pub struct FrameReader {
    stashed: VecDeque<String>,
    inner: BufReader<UnixStream>,
}

impl FrameReader {
    /// As `BufRead::read_line`: the next line into `buf`, its length, 0 at
    /// the end.
    pub fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        if let Some(line) = self.stashed.pop_front() {
            buf.push_str(&line);
            return Ok(line.len());
        }
        self.inner.read_line(buf)
    }
}

impl Client {
    pub fn new(stream: UnixStream, id_prefix: &'static str) -> std::io::Result<Self> {
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
            next_id: 1,
            id_prefix,
            stashed: VecDeque::new(),
        })
    }

    /// The writer and the reader apart, for a session that sends and
    /// listens on its own; events a call skipped are read first.
    pub fn into_parts(self) -> (UnixStream, FrameReader) {
        (
            self.writer,
            FrameReader {
                stashed: self.stashed,
                inner: self.reader,
            },
        )
    }

    pub fn call(&mut self, method: Method) -> Result<Value, String> {
        let id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        let req = Request {
            id: id.clone(),
            method,
        };
        self.writer
            .write_all(to_line(&req).as_bytes())
            .map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("daemon closed the connection".to_string());
            }
            if line.trim().is_empty() {
                continue;
            }
            match ServerFrame::parse(&line) {
                Ok(ServerFrame::Response(r)) if r.id == id => {
                    return match r.error {
                        Some(e) => Err(e.message),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    };
                }
                // An event that raced the response is kept for the reader.
                Ok(ServerFrame::Event(_)) => self.stashed.push_back(line),
                Ok(_) => continue,
                Err(e) => return Err(format!("bad frame from daemon: {e}")),
            }
        }
    }

    pub fn hello(&mut self) -> Result<HelloResult, String> {
        let v = self.call(Method::Hello {
            protocol: PROTOCOL_VERSION,
            version: VERSION.to_string(),
        })?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::{Author, MarkInfo, TurnInfo};

    #[test]
    fn locations_render_one_line_each_the_one_on_screen_first() {
        let mut r = LocationsResult {
            locations: vec![
                LocationInfo {
                    location: "acme/widget#142".to_string(),
                    name: "#142 Fix the parser".to_string(),
                    dir: PathBuf::from("/c/pr/142"),
                    showing: "#142 Fix the parser · into main".to_string(),
                    on_screen: true,
                    ..Default::default()
                },
                LocationInfo {
                    location: "/work/repo".to_string(),
                    name: "repo".to_string(),
                    dir: PathBuf::from("/work/repo"),
                    showing: "feature · 2 commits since main".to_string(),
                    viewers: vec!["v9 on workspace 2".to_string()],
                    marks: 3,
                    by_user: 2,
                    unread: 1,
                    ..Default::default()
                },
                LocationInfo {
                    location: "/work/other".to_string(),
                    name: "other".to_string(),
                    dir: PathBuf::from("/work/other"),
                    marks: 1,
                    by_user: 1,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(
            r.render(),
            "3 locations, the one on screen first\n\
             #142 Fix the parser · acme/widget#142 · on screen · #142 Fix the parser · into main · no marks\n\
             repo · /work/repo · open in v9 on workspace 2 · feature · 2 commits since main · 3 marks, 2 by the user, 1 unread\n\
             other · /work/other · not open in any viewer · 1 mark, 1 by the user\n\
             read unread comments with get_review, naming the location by repo or pr"
        );
        r.locations.truncate(1);
        assert!(
            r.render()
                .starts_with("1 location, the one on screen first"),
            "{}",
            r.render()
        );
        assert!(
            !r.render().contains("get_review"),
            "nothing unread, no nudge"
        );
        r.locations.clear();
        assert!(r.render().starts_with("no locations"));
    }

    #[test]
    fn a_review_renders_one_line_per_mark_with_the_commented_code() {
        let r = ReviewResult {
            location: "/work/repo".to_string(),
            dir: PathBuf::from("/work/repo"),
            showing: "src/x.rs · working tree".to_string(),
            marks: vec![
                MarkInfo {
                    label: "A".to_string(),
                    author: Author::Agent,
                    path: "src/x.rs".to_string(),
                    start: 40,
                    end: 46,
                    text: Some("the loop".to_string()),
                    ..Default::default()
                },
                MarkInfo {
                    label: "C".to_string(),
                    author: Author::User,
                    path: "src/x.rs".to_string(),
                    start: 42,
                    end: 43,
                    new: true,
                    theirs: true,
                    with_deletions: true,
                    text: Some("rename this".to_string()),
                    lines: vec!["let a = 1;".to_string(), "let b = 2;".to_string()],
                    ..Default::default()
                },
                MarkInfo {
                    label: "D".to_string(),
                    author: Author::User,
                    path: "src/y.rs".to_string(),
                    start: 7,
                    end: 7,
                    old_side: true,
                    stale: true,
                    theirs: true,
                    text: Some(String::new()),
                    thread: vec![TurnInfo {
                        author: Author::Agent,
                        text: "gone for good".to_string(),
                        new: false,
                    }],
                    ..Default::default()
                },
            ],
            unread: 1,
        };
        assert_eq!(
            r.render(),
            "/work/repo · src/x.rs · working tree\n\
             3 marks, 2 comments by the user, 1 reply, 1 new\n\
             [A] agent mark src/x.rs 40-46: the loop\n\
             [C] user mark src/x.rs 42-43 (new, includes deleted lines): rename this\n    \
             42 | let a = 1;\n    \
             43 | let b = 2;\n\
             [D] user mark src/y.rs old 7 (stale)\n    \
             agent: gone for good"
        );
    }

    #[test]
    fn empty_reviews_and_clears_say_so() {
        let r = ReviewResult {
            location: "/r".to_string(),
            dir: PathBuf::from("/r"),
            showing: "all".to_string(),
            ..Default::default()
        };
        assert_eq!(r.render(), "/r · all\nno marks");
        let c = ClearResult {
            location: "/r".to_string(),
            cleared: vec!["C".to_string(), "D".to_string()],
            remaining: 1,
            unread: 0,
        };
        assert_eq!(c.render(), "cleared [C] [D] on /r; 1 mark remains");
        let c = ClearResult {
            location: "/r".to_string(),
            ..Default::default()
        };
        assert_eq!(c.render(), "nothing to clear on /r; 0 marks remain");
        assert_eq!(
            unread_note("/r", 2),
            "; the user has 2 unread comments on /r, call get_review"
        );
        assert_eq!(unread_note("/r", 0), "");
        let r = ReplyResult {
            location: "acme/widget#3".to_string(),
            letter: "B".to_string(),
            unread: 0,
        };
        assert_eq!(r.render(), "replied under [B] on acme/widget#3");
        let mine = [
            MarkInfo {
                label: "A".to_string(),
                path: "src/x.rs".to_string(),
                ..Default::default()
            },
            MarkInfo {
                label: "E".to_string(),
                path: "src/y.rs".to_string(),
                ..Default::default()
            },
            MarkInfo {
                label: "F".to_string(),
                path: "src/x.rs".to_string(),
                ..Default::default()
            },
        ];
        assert_eq!(
            mine_note("/r", &mine),
            "; your marks on /r: [A] [F] src/x.rs, [E] src/y.rs; remember to remove any marks that are not needed any more"
        );
        assert_eq!(mine_note("/r", &[]), "");
        let r = MarkResult {
            location: "/r".to_string(),
            marks: vec![
                MarkInfo {
                    label: "E".to_string(),
                    path: "src/y.rs".to_string(),
                    start: 10,
                    end: 12,
                    ..Default::default()
                },
                MarkInfo {
                    label: "F".to_string(),
                    path: "src/z.rs".to_string(),
                    start: 3,
                    end: 3,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            r.render(),
            "marked [E] src/y.rs 10-12, [F] src/z.rs 3 on /r"
        );
    }

    #[test]
    fn queries_name_locations_by_repo_or_pr() {
        let q = AgentQuery::Review {
            location: Some(LocationRef {
                repo: Some(PathBuf::from("/work/repo")),
                pr: None,
            }),
        };
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"review","location":{"repo":"/work/repo"}}"#
        );
        let back: AgentQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
    }
}

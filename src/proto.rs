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

pub const PROTOCOL_VERSION: u32 = 4;
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

pub use crate::marks::{MAX_MARKS, MarkEntry, MarkInfo, MarkSpec};
pub use crate::registry::{LocationKey, LocationState};

/// What the daemon currently asks viewers to display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Whether `dir` was inside a git repository when it was set.
    pub repo: bool,
}

/// A file to show, whole or one region of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSpec {
    /// Relative to the checkout root.
    pub path: String,
    /// Only these lines, 1-based and inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(usize, usize)>,
}

impl FileSpec {
    pub fn words(&self) -> String {
        match self.lines {
            Some((a, b)) if a == b => format!("{} {a}", self.path),
            Some((a, b)) => format!("{} {a}-{b}", self.path),
            None => self.path.clone(),
        }
    }
}

/// A show as relayed to a viewer: the location as the daemon has it, how
/// to fetch it when it is a pull request, and what to put on screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowEvent {
    pub state: LocationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<crate::pr::Spec>,
    #[serde(default)]
    pub view: ViewSpec,
}

/// What a viewer shows for an agent: the files, and the marks to place.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSpec {
    /// The files, whole or in part; none means every file the view has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileSpec>,
    /// Marks to place, lettered in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<MarkSpec>,
}

/// What an agent asks the daemon about a location's marks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentQuery {
    /// Every mark on a location, or the ones `letters` names; the user's
    /// words count as read afterwards.
    Review {
        location: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        letters: Vec<String>,
    },
    /// Answer under a mark the user made or wrote under.
    Reply {
        location: String,
        letter: String,
        text: String,
    },
}

/// A location as listed for `codesnob status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationInfo {
    /// How to name it.
    pub location: String,
    /// "#142 Fix the parser" for a pull request, else the directory's name.
    pub name: String,
    pub dir: PathBuf,
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
        if self.viewers.is_empty() {
            out.push_str(" · not open in any viewer");
        } else {
            out.push_str(&format!(" · open in {}", self.viewers.join(", ")));
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

/// The state of a location, closing every reply to an agent the same way.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// How to name the location, to copy into the next call.
    pub location: String,
    /// Every mark there.
    pub total: usize,
    /// The agent's own: the ones the user has not written under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mine: Vec<MarkInfo>,
    /// The user's words: the marks they made, and their turns.
    pub by_user: usize,
    /// Those the agent has not read.
    pub unread: usize,
    /// Anything out of the ordinary: a show held, a mark gone stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Report {
    pub fn of(location: &str, marks: &crate::marks::Marks) -> Self {
        Self {
            location: location.to_string(),
            total: marks.marks.len(),
            mine: marks.mine(),
            by_user: marks.users_words(),
            unread: marks.unread(),
            notes: Vec::new(),
        }
    }

    /// "[A] [D] src/marks.rs, [B] src/ui/diagram.rs".
    pub fn mine_by_file(&self) -> String {
        let mut files: Vec<(&str, Vec<String>)> = Vec::new();
        for m in &self.mine {
            let letter = format!("[{}]", m.label);
            match files.iter_mut().find(|(p, _)| *p == m.path) {
                Some((_, letters)) => letters.push(letter),
                None => files.push((&m.path, vec![letter])),
            }
        }
        files
            .iter()
            .map(|(p, letters)| format!("{} {p}", letters.join(" ")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The closing lines of every reply.
    pub fn render(&self) -> String {
        let mut out = format!("location: {}", self.location);
        let mut marks = match self.total {
            0 => "none".to_string(),
            1 => "1 here".to_string(),
            n => format!("{n} here"),
        };
        if !self.mine.is_empty() {
            marks.push_str(&format!(" · yours {}", self.mine_by_file()));
        }
        if self.by_user > 0 {
            marks.push_str(&format!(" · {} by the user", self.by_user));
            if self.unread > 0 {
                marks.push_str(&format!(", {} unread", self.unread));
            }
        }
        out.push_str(&format!("\nmarks: {marks}"));
        for n in &self.notes {
            out.push_str(&format!("\nnote: {n}"));
        }
        out
    }
}

/// "[A] src/x.rs 1-2, [B] src/y.rs 3".
pub fn lettered(marks: &[MarkInfo]) -> String {
    marks
        .iter()
        .map(|m| format!("[{}] {} {}", m.label, m.path, m.lines_word()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `review` answers with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    pub marks: Vec<MarkInfo>,
    /// Words of the user's the agent had not read before this.
    pub new: usize,
    pub report: Report,
}

impl ReviewResult {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.marks.is_empty() {
            out.push_str("review: no marks");
        } else {
            let comments = self
                .marks
                .iter()
                .filter(|m| m.author == crate::marks::Author::User)
                .count();
            let turns: usize = self.marks.iter().map(|m| m.thread.len()).sum();
            out.push_str(&format!(
                "review: {} mark{}, {comments} by the user{}{}",
                self.marks.len(),
                if self.marks.len() == 1 { "" } else { "s" },
                match turns {
                    0 => String::new(),
                    1 => ", 1 reply".to_string(),
                    n => format!(", {n} replies"),
                },
                match self.new {
                    0 => String::new(),
                    n => format!(", {n} new"),
                }
            ));
            for m in &self.marks {
                out.push('\n');
                out.push_str(&m.render());
            }
        }
        out.push('\n');
        out.push_str(&self.report.render());
        out
    }
}

/// What a reply is answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    /// The letter of the mark the reply went under.
    pub letter: String,
    pub report: Report,
}

impl ReplyResult {
    pub fn render(&self) -> String {
        format!("replied: under [{}]\n{}", self.letter, self.report.render())
    }
}

/// What a viewer answers a show request with, once it has applied it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowAck {
    /// The marks the call placed, with their letters.
    pub marks: Vec<MarkInfo>,
    /// Whether the view changed now, or waits for the user.
    pub applied: bool,
}

/// What a show is answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowResult {
    /// What is on screen now, in words; empty when the view did not change.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shown: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marked: Vec<MarkInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moved: Vec<MarkInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
    /// The viewer that was told to display it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub viewer: String,
    pub report: Report,
}

impl ShowResult {
    pub fn render(&self) -> String {
        let mut out = Vec::new();
        if !self.shown.is_empty() {
            out.push(format!("shown: {}", self.shown));
        }
        if !self.marked.is_empty() {
            out.push(format!("marked: {}", lettered(&self.marked)));
        }
        if !self.moved.is_empty() {
            out.push(format!("moved: {}", lettered(&self.moved)));
        }
        if !self.deleted.is_empty() {
            let letters: Vec<String> = self.deleted.iter().map(|l| format!("[{l}]")).collect();
            out.push(format!("deleted: {}", letters.join(" ")));
        }
        if out.is_empty() {
            out.push("nothing to do".to_string());
        }
        out.push(self.report.render());
        out.join("\n")
    }
}

/// Files to show as they are, as the viewer keeps the request.
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
    /// Only these lines, 1-based and inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(usize, usize)>,
    /// Only the marks, each with a few lines around it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excerpts: bool,
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
    /// Show a location in one viewer: the first on `workspace`, else the
    /// nearest workspace, else the first viewer. The marks are changed
    /// first, by the daemon; the view changes when there are files or new
    /// marks, or nothing at all to do to the marks. Errors when no viewer
    /// is connected.
    Show {
        location: String,
        /// The caller's own workspace, when it could be determined.
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileSpec>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        marks: Vec<MarkEntry>,
        /// The agent's own marks on the location go first.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
        /// The user explicitly asked for their marks to be deleted.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_asked: bool,
    },
    /// An agent's question about marks, answered by the daemon.
    Query {
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
    /// `viewer.reply` under `id` when there is one. `state` is the
    /// location as the daemon has it, for a viewer opening it now; `pr`
    /// says how to fetch a pull request.
    Show {
        viewer: String,
        show: ShowEvent,
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
    fn locations_render_one_line_each_for_status() {
        let l = LocationInfo {
            location: "/work/repo#branch".to_string(),
            name: "repo".to_string(),
            dir: PathBuf::from("/work/repo"),
            viewers: vec!["v9 on workspace 2".to_string()],
            marks: 3,
            by_user: 2,
            unread: 1,
        };
        assert_eq!(
            l.render(),
            "repo · /work/repo#branch · open in v9 on workspace 2 · 3 marks, 2 by the user, 1 unread"
        );
        let l = LocationInfo {
            location: "acme/widget#142".to_string(),
            name: "#142 Fix the parser".to_string(),
            dir: PathBuf::from("/c/pr/142"),
            ..Default::default()
        };
        assert_eq!(
            l.render(),
            "#142 Fix the parser · acme/widget#142 · not open in any viewer · no marks"
        );
    }

    #[test]
    fn a_review_renders_one_line_per_mark_with_the_commented_code() {
        let r = ReviewResult {
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
            new: 1,
            report: Report {
                location: "/work/repo".to_string(),
                total: 3,
                mine: vec![MarkInfo {
                    label: "A".to_string(),
                    path: "src/x.rs".to_string(),
                    ..Default::default()
                }],
                by_user: 2,
                unread: 0,
                notes: Vec::new(),
            },
        };
        assert_eq!(
            r.render(),
            "review: 3 marks, 2 by the user, 1 reply, 1 new\n\
             [A] agent mark src/x.rs 40-46: the loop\n\
             [C] user mark src/x.rs 42-43 (new, includes deleted lines): rename this\n    \
             42 | let a = 1;\n    \
             43 | let b = 2;\n\
             [D] user mark src/y.rs old 7 (stale)\n    \
             agent: gone for good\n\
             location: /work/repo\n\
             marks: 3 here · yours [A] src/x.rs · 2 by the user"
        );
    }

    #[test]
    fn every_reply_closes_with_the_same_lines() {
        let mine = vec![
            MarkInfo {
                label: "A".to_string(),
                path: "src/x.rs".to_string(),
                ..Default::default()
            },
            MarkInfo {
                label: "E".to_string(),
                path: "src/y.rs".to_string(),
                start: 10,
                end: 12,
                ..Default::default()
            },
            MarkInfo {
                label: "F".to_string(),
                path: "src/x.rs".to_string(),
                start: 3,
                end: 3,
                ..Default::default()
            },
        ];
        let report = Report {
            location: "/r#branch".to_string(),
            total: 5,
            mine: mine.clone(),
            by_user: 2,
            unread: 1,
            notes: vec!["the user is busy here".to_string()],
        };
        assert_eq!(
            report.render(),
            "location: /r#branch\n\
             marks: 5 here · yours [A] [F] src/x.rs, [E] src/y.rs · 2 by the user, 1 unread\n\
             note: the user is busy here"
        );
        let empty = Report::of("/r", &crate::marks::Marks::default());
        assert_eq!(empty.render(), "location: /r\nmarks: none");
        let r = ReviewResult {
            report: empty.clone(),
            ..Default::default()
        };
        assert_eq!(r.render(), "review: no marks\nlocation: /r\nmarks: none");
        let r = ReplyResult {
            letter: "B".to_string(),
            report: empty.clone(),
        };
        assert_eq!(r.render(), "replied: under [B]\nlocation: /r\nmarks: none");
        let r = ShowResult {
            shown: "the working tree · src/y.rs 1-40".to_string(),
            marked: mine[1..].to_vec(),
            moved: mine[..1].to_vec(),
            deleted: vec!["C".to_string(), "D".to_string()],
            viewer: "v1".to_string(),
            report: report.clone(),
        };
        assert_eq!(
            r.render(),
            "shown: the working tree · src/y.rs 1-40\n\
             marked: [E] src/y.rs 10-12, [F] src/x.rs 3\n\
             moved: [A] src/x.rs 0\n\
             deleted: [C] [D]\n\
             location: /r#branch\n\
             marks: 5 here · yours [A] [F] src/x.rs, [E] src/y.rs · 2 by the user, 1 unread\n\
             note: the user is busy here"
        );
        let r = ShowResult {
            report: empty,
            ..Default::default()
        };
        assert_eq!(r.render(), "nothing to do\nlocation: /r\nmarks: none");
    }

    #[test]
    fn queries_and_shows_travel_as_json() {
        let q = AgentQuery::Review {
            location: "/work/repo#main...HEAD".to_string(),
            letters: vec!["C".to_string()],
        };
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"review","location":"/work/repo#main...HEAD","letters":["C"]}"#
        );
        let back: AgentQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q);
        let m = Method::Show {
            location: "/work/repo".to_string(),
            workspace: None,
            files: vec![FileSpec {
                path: "a.rs".to_string(),
                lines: Some((1, 4)),
            }],
            marks: vec![
                MarkEntry::Delete {
                    letter: "A".to_string(),
                },
                MarkEntry::Add(MarkSpec {
                    path: "a.rs".to_string(),
                    start: 2,
                    end: 2,
                    text: None,
                }),
            ],
            replace: true,
            user_asked: false,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains(r#""do":"delete""#) && json.contains(r#""do":"add""#),
            "{json}"
        );
        assert!(
            json.contains(r#""replace":true"#) && !json.contains("user_asked"),
            "{json}"
        );
        let key = LocationKey::Repo {
            dir: PathBuf::from("/work/repo"),
            view: crate::location::View::parse("branch"),
        };
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"repo","dir":"/work/repo","view":{"kind":"branch"}}"#
        );
    }
}

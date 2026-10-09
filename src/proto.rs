//! Wire protocol and socket conventions shared by the daemon, the TUI and
//! one-shot commands. One JSON object per newline-terminated line in both
//! directions; requests carry an id, events do not.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;
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
    /// Line ranges to highlight, lettered in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<HighlightSpec>,
    /// Add the files and highlights to what the location shows, keeping
    /// every mark, instead of replacing the view.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub append: bool,
}

/// What an agent asks a viewer, through the daemon, besides showing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViewerQuery {
    /// Every mark on a location, with the user's comments; the comments
    /// count as read afterwards.
    Review {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
    },
    /// Remove marks: `letters` names them, replies too ("C2"), else
    /// `which` of "mine" (the agent's highlights and replies), "user" or
    /// "all". The user's comments and replies go only with `user_asked`:
    /// the user explicitly asked for that.
    ClearMarks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
        which: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        letters: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_asked: bool,
    },
    /// Answer one of the user's comments, under it: C1, C2…
    Reply {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
        letter: String,
        text: String,
    },
    /// Move one of the user's comments to other lines, of `path` or of its
    /// own file, keeping its letter, text and replies.
    MoveComment {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
        letter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        start: usize,
        end: usize,
    },
}

/// A mark as reported to an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkInfo {
    pub label: String,
    /// "agent" or "user".
    pub author: String,
    pub path: String,
    pub start: usize,
    pub end: usize,
    /// The lines are the old side's: deleted lines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub old_side: bool,
    /// Deleted lines were among those marked, with others.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_deletions: bool,
    /// Its lines are nowhere to be found since the last load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// A comment the agent has not read yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    /// The letter of the mark this comment answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The marked code, as it was when the mark was made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    /// The answers under a comment, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<ReplyInfo>,
}

/// A reply under a comment, as reported to an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyInfo {
    /// The comment's letter and the reply's number: "C2".
    pub label: String,
    /// "agent" or "user".
    pub author: String,
    pub text: String,
    /// The user's, and the agent has not read it yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
}

/// The most code lines a mark is reported with.
pub const MAX_MARK_LINES: usize = 12;

impl MarkInfo {
    pub fn lines_word(&self) -> String {
        let side = if self.old_side { "old " } else { "" };
        if self.start == self.end {
            format!("{side}{}", self.start)
        } else {
            format!("{side}{}-{}", self.start, self.end)
        }
    }

    /// One line, plus the code for the user's comments.
    pub fn render(&self) -> String {
        let kind = if self.author == "user" {
            "comment"
        } else {
            "highlight"
        };
        let mut flags = Vec::new();
        if self.new {
            flags.push("new".to_string());
        }
        if let Some(re) = &self.re {
            flags.push(format!("re [{re}]"));
        }
        if self.stale {
            flags.push("stale".to_string());
        }
        if self.with_deletions {
            flags.push("includes deleted lines".to_string());
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(", "))
        };
        let mut out = format!(
            "[{}] {kind} {} {}{flags}",
            self.label,
            self.path,
            self.lines_word()
        );
        match &self.text {
            Some(t) if !t.is_empty() => {
                out.push_str(": ");
                out.push_str(t);
            }
            _ => {}
        }
        if self.author == "user" {
            let w = self.end.to_string().len();
            for (i, line) in self.lines.iter().take(MAX_MARK_LINES).enumerate() {
                out.push_str(&format!("\n    {:>w$} | {line}", self.start + i));
            }
            if self.lines.len() > MAX_MARK_LINES {
                out.push_str(&format!(
                    "\n    … {} more lines",
                    self.lines.len() - MAX_MARK_LINES
                ));
            }
        }
        for r in &self.replies {
            let new = if r.new { " (new)" } else { "" };
            out.push_str(&format!(
                "\n    [{}] {} reply{new}: {}",
                r.label, r.author, r.text
            ));
        }
        out
    }
}

/// What a viewer answers `get_review` with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// "L3": the location's id in that viewer.
    pub location: String,
    pub dir: PathBuf,
    /// What the location shows, in words.
    pub showing: String,
    pub marks: Vec<MarkInfo>,
    /// Comments the agent had not read before this.
    pub unread: usize,
}

impl ReviewResult {
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} {} · {}",
            self.location,
            self.dir.display(),
            self.showing
        );
        if self.marks.is_empty() {
            out.push_str("\nno marks");
            return out;
        }
        let comments = self.marks.iter().filter(|m| m.author == "user").count();
        let replies: usize = self.marks.iter().map(|m| m.replies.len()).sum();
        out.push_str(&format!(
            "\n{} mark{}, {comments} comment{} by the user{}{}",
            self.marks.len(),
            if self.marks.len() == 1 { "" } else { "s" },
            if comments == 1 { "" } else { "s" },
            match replies {
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
    pub location: String,
    pub dir: PathBuf,
    /// The highlights the call placed, with their letters.
    pub marks: Vec<MarkInfo>,
    /// The user's comments the agent has not read.
    pub unread: usize,
    /// Whether the view changed now, or waits for the user.
    pub applied: bool,
}

/// What a viewer answers `clear_marks` with.
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

/// What a viewer answers a reply with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    pub location: String,
    /// The reply's own label: "C1".
    pub label: String,
    pub unread: usize,
}

impl ReplyResult {
    pub fn render(&self) -> String {
        format!("replied [{}] on {}", self.label, self.location)
    }
}

/// What a viewer answers moving a comment with.
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

/// The agent's nudge about comments it has not read, for every reply.
pub fn unread_note(location: &str, unread: usize) -> String {
    match unread {
        0 => String::new(),
        1 => format!("; the user has 1 unread comment on {location}, call get_review"),
        n => format!("; the user has {n} unread comments on {location}, call get_review"),
    }
}

/// The most highlights one call can place.
pub const MAX_HIGHLIGHTS: usize = 20;

/// The letter of the `n`th mark made on a location, counting from 0:
/// A to Z, then AA, AB and on, as spreadsheet columns go.
pub fn letter(n: usize) -> String {
    let mut n = n;
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (n % 26) as u8);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

/// A highlighted line range, 1-based and inclusive, with an optional note.
/// In a diff the lines are the new side's, or the old side's for a deleted
/// file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpec {
    /// Relative to the repository root.
    pub path: String,
    pub start: usize,
    pub end: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    /// Show only the line and highlights with a few lines around them
    /// (true), or the whole file (false). Unset: whole when it is the only
    /// file, excerpts when there are several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpts: Option<bool>,
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
    /// Ask the viewer picked as for `Show` something, and relay its answer.
    Query {
        #[serde(default)]
        workspace: Option<String>,
        query: ViewerQuery,
    },
    /// A viewer's answer to a `Show` or `Query` event, by the event's id.
    #[serde(rename = "viewer.reply")]
    ViewerReply {
        id: String,
        result: Value,
    },
    Status,
    Subscribe,
    #[serde(rename = "viewer.register")]
    ViewerRegister(ViewerEntry),
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
    /// A question for one viewer, answered with `viewer.reply` under `id`.
    Query {
        viewer: String,
        id: String,
        query: ViewerQuery,
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
    if let Ok(s) = UnixStream::connect(daemon_socket()) {
        return Some(s);
    }
    spawn_daemon();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        if let Ok(s) = UnixStream::connect(daemon_socket()) {
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
}

impl Client {
    pub fn new(stream: UnixStream, id_prefix: &'static str) -> std::io::Result<Self> {
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            writer: stream,
            reader,
            next_id: 1,
            id_prefix,
        })
    }

    pub fn into_parts(self) -> (UnixStream, BufReader<UnixStream>) {
        (self.writer, self.reader)
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

    #[test]
    fn a_review_renders_one_line_per_mark_with_the_commented_code() {
        let r = ReviewResult {
            location: "L3".to_string(),
            dir: PathBuf::from("/work/repo"),
            showing: "src/x.rs · working tree".to_string(),
            marks: vec![
                MarkInfo {
                    label: "A".to_string(),
                    author: "agent".to_string(),
                    path: "src/x.rs".to_string(),
                    start: 40,
                    end: 46,
                    text: Some("the loop".to_string()),
                    ..Default::default()
                },
                MarkInfo {
                    label: "C".to_string(),
                    author: "user".to_string(),
                    path: "src/x.rs".to_string(),
                    start: 42,
                    end: 43,
                    new: true,
                    with_deletions: true,
                    text: Some("rename this".to_string()),
                    lines: vec!["let a = 1;".to_string(), "let b = 2;".to_string()],
                    ..Default::default()
                },
                MarkInfo {
                    label: "D".to_string(),
                    author: "user".to_string(),
                    path: "src/y.rs".to_string(),
                    start: 7,
                    end: 7,
                    old_side: true,
                    stale: true,
                    text: Some(String::new()),
                    ..Default::default()
                },
            ],
            unread: 1,
        };
        let text = r.render();
        assert_eq!(
            text,
            "L3 /work/repo · src/x.rs · working tree\n\
             3 marks, 2 comments by the user, 1 new\n\
             [A] highlight src/x.rs 40-46: the loop\n\
             [C] comment src/x.rs 42-43 (new, includes deleted lines): rename this\n    \
             42 | let a = 1;\n    \
             43 | let b = 2;\n\
             [D] comment src/y.rs old 7 (stale)"
        );
    }

    #[test]
    fn long_comments_show_twelve_lines_and_say_how_many_more() {
        let m = MarkInfo {
            label: "B".to_string(),
            author: "user".to_string(),
            path: "a.txt".to_string(),
            start: 1,
            end: 20,
            lines: (1..=20).map(|i| format!("l{i}")).collect(),
            ..Default::default()
        };
        let text = m.render();
        assert!(
            text.contains("12 | l12") && !text.contains("13 | l13"),
            "{text}"
        );
        assert!(text.ends_with("… 8 more lines"), "{text}");
    }

    #[test]
    fn empty_reviews_and_clears_say_so() {
        let r = ReviewResult {
            location: "L1".to_string(),
            dir: PathBuf::from("/r"),
            showing: "all".to_string(),
            ..Default::default()
        };
        assert_eq!(r.render(), "L1 /r · all\nno marks");
        let c = ClearResult {
            location: "L1".to_string(),
            cleared: vec!["C".to_string(), "D".to_string()],
            remaining: 1,
            unread: 0,
        };
        assert_eq!(c.render(), "cleared [C] [D] on L1; 1 mark remains");
        let c = ClearResult {
            location: "L1".to_string(),
            ..Default::default()
        };
        assert_eq!(c.render(), "nothing to clear on L1; 0 marks remain");
        assert_eq!(
            unread_note("L1", 2),
            "; the user has 2 unread comments on L1, call get_review"
        );
        assert_eq!(unread_note("L1", 0), "");
    }

    #[test]
    fn letters_go_on_like_spreadsheet_columns() {
        let got: Vec<String> = [0, 1, 25, 26, 27, 51, 52, 701, 702]
            .into_iter()
            .map(letter)
            .collect();
        assert_eq!(got, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
    }
}

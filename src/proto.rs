//! Wire protocol and socket conventions shared by the daemon, the TUI and
//! one-shot commands. One JSON object per newline-terminated line in both
//! directions; requests carry an id, events do not.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
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
}

#[derive(Serialize, Deserialize)]
pub struct StatusResult {
    pub target: Option<Target>,
    pub viewers: Vec<ViewerEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "params", rename_all = "snake_case")]
pub enum Event {
    /// Addressed to one viewer; others ignore it.
    Show {
        viewer: String,
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ViewSpec>,
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

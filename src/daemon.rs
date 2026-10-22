//! The headless daemon: one per user. It keeps the locations and their
//! marks, answers agents' questions about them, relays what agents want
//! shown to the right viewer, and tells every viewer whenever a location's
//! marks change. Started on demand by clients.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::location::Named;
use crate::proto::{
    self, AgentQuery, Event, HelloResult, LocationInfo, LocationKey, Method, Report, Response,
    RpcError, ShowResult, StatusResult, Target, ViewerEntry, to_line,
};
use crate::registry::Registry;

/// How long a viewer gets to answer a show or a query.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct State {
    target: Option<Target>,
    subscribers: Vec<(u64, Sender<Event>)>,
    /// Registered viewers, keyed by the connection that owns them.
    viewers: Vec<(u64, ViewerEntry)>,
    /// Answers awaited from viewers, by the id they were asked under.
    pending: Vec<(String, Sender<Value>)>,
    next_query: u64,
    /// Every location open in a viewer or carrying marks.
    registry: Registry,
}

impl State {
    fn broadcast(&mut self, ev: Event) {
        self.subscribers
            .retain(|(_, tx)| tx.send(ev.clone()).is_ok());
    }

    /// An event for one connection only.
    fn send_to(&mut self, conn: u64, ev: Event) {
        if let Some((_, tx)) = self.subscribers.iter().find(|(c, _)| *c == conn) {
            let _ = tx.send(ev);
        }
    }

    /// Locations nobody has open go once they carry no marks.
    fn drop_unused(&mut self) {
        let viewers = self.viewers.clone();
        let open = |key: &LocationKey| {
            viewers
                .iter()
                .any(|(_, v)| v.open.iter().any(|o| o.key == *key))
        };
        for key in self.registry.drop_unused(&open) {
            eprintln!("location {} closed", key.name());
        }
    }

    /// Tell every viewer what a location's marks are now.
    fn announce(&mut self, key: &LocationKey, note: Option<String>) {
        if let Some(state) = self.registry.get(key).cloned() {
            self.broadcast(Event::Location { state, note });
        }
    }

    /// Every location, for `codesnob status`.
    fn locations_info(&self) -> Vec<LocationInfo> {
        self.registry
            .locations
            .iter()
            .map(|l| {
                let viewers: Vec<String> = self
                    .viewers
                    .iter()
                    .filter(|(_, v)| v.open.iter().any(|o| o.key == l.key))
                    .map(|(_, v)| match &v.workspace {
                        Some(ws) => format!("{} on workspace {ws}", v.id),
                        None => v.id.clone(),
                    })
                    .collect();
                LocationInfo {
                    location: l.key.name(),
                    name: l.name.clone(),
                    dir: l.dir.clone(),
                    viewers,
                    marks: l.marks.marks.len(),
                    by_user: l.marks.users_words(),
                    unread: l.marks.unread(),
                }
            })
            .collect()
    }

    /// Answer an agent's question about marks.
    fn answer(&mut self, query: AgentQuery) -> Result<Value, String> {
        match query {
            AgentQuery::Review { location, letters } => {
                let (key, ..) = locate(&location)?;
                let name = key.name();
                let Some(loc) = self.registry.get_mut(&key) else {
                    if !letters.is_empty() {
                        return Err(format!("no marks on {name}"));
                    }
                    return Ok(json!(proto::ReviewResult {
                        marks: Vec::new(),
                        new: 0,
                        report: Report::of(&name, &Default::default()),
                    }));
                };
                let new = loc.marks.unread();
                let marks = loc.marks.read(&letters)?;
                let result = proto::ReviewResult {
                    marks,
                    new,
                    report: Report::of(&name, &loc.marks),
                };
                // Read, the comments no longer hold an agent's request back.
                self.announce(&key, None);
                Ok(json!(result))
            }
            AgentQuery::Reply {
                location,
                letter,
                text,
            } => {
                let (key, ..) = locate(&location)?;
                let name = key.name();
                let loc = self
                    .registry
                    .get_mut(&key)
                    .ok_or_else(|| format!("no marks on {name}"))?;
                let letter = loc.marks.reply(&letter, &text)?;
                let result = proto::ReplyResult {
                    letter: letter.clone(),
                    report: Report::of(&name, &loc.marks),
                };
                self.announce(&key, Some(format!("agent replied under [{letter}]")));
                Ok(json!(result))
            }
        }
    }

    /// A channel for a viewer's answer, under a fresh id.
    fn expect_reply(&mut self) -> (String, std::sync::mpsc::Receiver<Value>) {
        self.next_query += 1;
        let id = format!("q{}", self.next_query);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.push((id.clone(), tx));
        (id, rx)
    }

    /// Nobody waits for `id` any more.
    fn forget_reply(&mut self, id: &str) {
        self.pending.retain(|(p, _)| p != id);
    }

    /// However the connection ended, everything it owned goes with it.
    fn disconnect(&mut self, conn: u64) {
        self.subscribers.retain(|(c, _)| *c != conn);
        let gone: Vec<String> = self
            .viewers
            .iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, v)| v.id.clone())
            .collect();
        self.viewers.retain(|(c, _)| *c != conn);
        for id in gone {
            eprintln!("viewer {id} disconnected");
            self.broadcast(Event::ViewerDisconnected { id });
        }
        self.drop_unused();
    }
}

/// What a location an agent named is: its key, checkout and short name.
fn locate(location: &str) -> Result<(LocationKey, std::path::PathBuf, String, Named), String> {
    let named = crate::location::parse(location)?;
    let (key, dir, name) = crate::registry::resolve(&named)?;
    Ok((key, dir, name, named))
}

/// What a show puts on screen, in words.
fn shown_words(key: &LocationKey, files: &[proto::FileSpec], adds: bool) -> String {
    let what = match key {
        LocationKey::Repo { view, .. } => view.words(),
        LocationKey::Pr { number, .. } => format!("#{number}"),
    };
    let diff = key.view().is_none_or(|v| v.is_diff());
    let which = if !files.is_empty() {
        files
            .iter()
            .map(proto::FileSpec::words)
            .collect::<Vec<_>>()
            .join(", ")
    } else if adds && !diff {
        "the marked lines, with a few around them".to_string()
    } else {
        "all files".to_string()
    };
    format!("{what} · {which}")
}

struct Shared {
    state: Mutex<State>,
    shutdown: AtomicBool,
    /// Where this daemon listens; a handler connects to it to wake the
    /// accept loop on shutdown.
    socket: std::path::PathBuf,
}

pub fn run() -> Result<()> {
    run_at(proto::daemon_socket())
}

/// Serve on `socket` until asked to shut down.
pub fn run_at(socket: std::path::PathBuf) -> Result<()> {
    if let Some(dir) = socket.parent() {
        proto::ensure_private_dir(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let listener = bind(&socket)?;
    let identity = file_identity(&socket);
    eprintln!(
        "codesnobd {} listening on {}",
        proto::VERSION,
        socket.display()
    );

    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        shutdown: AtomicBool::new(false),
        socket: socket.clone(),
    });
    let next_conn = AtomicU64::new(1);

    for stream in listener.incoming() {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let conn = next_conn.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            serve(conn, stream, &shared);
            shared.state.lock().unwrap().disconnect(conn);
        });
    }

    // Only remove the socket if it is still ours; a successor may have replaced it.
    if file_identity(&socket) == identity {
        let _ = std::fs::remove_file(&socket);
    }
    eprintln!("codesnobd exiting");
    Ok(())
}

fn file_identity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

enum Existing {
    Stale,
    Newer,
    Replaced,
}

fn bind(socket: &std::path::Path) -> Result<UnixListener> {
    match UnixListener::bind(socket) {
        Ok(l) => return Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(e).with_context(|| format!("cannot bind {}", socket.display())),
    }
    match ask_existing_daemon_to_exit(socket) {
        Existing::Newer => {
            bail!("a codesnob daemon of the same or newer version is already running")
        }
        Existing::Stale | Existing::Replaced => {}
    }
    for _ in 0..40 {
        if let Ok(l) = UnixListener::bind(socket) {
            return Ok(l);
        }
        let _ = std::fs::remove_file(socket);
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("cannot take over the socket at {}", socket.display())
}

fn ask_existing_daemon_to_exit(socket: &std::path::Path) -> Existing {
    let Ok(stream) = UnixStream::connect(socket) else {
        return Existing::Stale;
    };
    let timeout = Some(Duration::from_secs(2));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return Existing::Stale;
    }
    let Ok(mut client) = proto::Client::new(stream, "takeover-") else {
        return Existing::Stale;
    };
    let Ok(hello) = client.hello() else {
        return Existing::Stale;
    };
    if !outdated(&hello) {
        return Existing::Newer;
    }
    let _ = client.call(Method::DaemonShutdown);
    Existing::Replaced
}

/// Whether a running daemon that answered `hello` should make way for
/// this build: it speaks an older protocol, or is an older version.
pub fn outdated(hello: &HelloResult) -> bool {
    hello.protocol < proto::PROTOCOL_VERSION || is_newer(proto::VERSION, &hello.version)
}

fn is_newer(candidate: &str, running: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(running)
}

type SharedWriter = Arc<Mutex<UnixStream>>;

fn send(writer: &SharedWriter, line: &str) -> bool {
    let mut w = writer.lock().unwrap();
    w.write_all(line.as_bytes()).is_ok()
}

fn serve(conn: u64, stream: UnixStream, shared: &Arc<Shared>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let writer: SharedWriter = Arc::new(Mutex::new(stream));
    let reader = BufReader::new(read_half);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (id, method) = match parse_request(&line) {
            Ok(x) => x,
            Err((id, code, message)) => {
                let resp = Response {
                    id,
                    result: None,
                    error: Some(RpcError { code, message }),
                };
                if !send(&writer, &to_line(&resp)) {
                    break;
                }
                continue;
            }
        };
        let resp = match handle(conn, method, shared, &writer) {
            Ok(result) => Response {
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id,
                result: None,
                error: Some(error),
            },
        };
        if !send(&writer, &to_line(&resp)) {
            break;
        }
        if shared.shutdown.load(Ordering::Relaxed) {
            // Wake the accept loop so it observes the flag.
            let _ = UnixStream::connect(&shared.socket);
            break;
        }
    }
}

fn parse_request(line: &str) -> Result<(String, Method), (String, String, String)> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| (String::new(), "invalid_frame".to_string(), e.to_string()))?;
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let method_name = v
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match serde_json::from_value::<Method>(v) {
        Ok(m) => Ok((id, m)),
        Err(e) => {
            let known = [
                "hello",
                "show",
                "query",
                "status",
                "subscribe",
                "viewer.register",
                "viewer.reply",
                "viewer.location",
                "viewer.op",
                "viewer.close",
                "daemon.shutdown",
            ];
            let code = if known.contains(&method_name.as_str()) {
                "invalid_params"
            } else {
                "unsupported_method"
            };
            Err((id, code.to_string(), e.to_string()))
        }
    }
}

fn no_viewer() -> RpcError {
    RpcError {
        code: "no_viewer".to_string(),
        message: "no codesnob viewer is connected".to_string(),
    }
}

/// Choose the viewer for a request coming from `workspace`.
fn pick_viewer<'a>(
    viewers: &'a [(u64, ViewerEntry)],
    workspace: Option<&str>,
) -> Option<(&'a ViewerEntry, &'static str)> {
    let first = viewers.first().map(|(_, v)| v)?;
    let Some(ws) = workspace else {
        return Some((first, "first"));
    };
    if let Some((_, v)) = viewers
        .iter()
        .find(|(_, v)| v.workspace.as_deref() == Some(ws))
    {
        return Some((v, "same"));
    }
    // Nearest by numeric distance; names that are not numbers are far away.
    let num = |s: &str| s.trim().parse::<i64>().ok();
    let Some(mine) = num(ws) else {
        return Some((first, "first"));
    };
    let nearest = viewers
        .iter()
        .filter_map(|(_, v)| {
            v.workspace
                .as_deref()
                .and_then(num)
                .map(|n| ((n - mine).abs(), v))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, v)| v);
    match nearest {
        Some(v) => Some((v, "nearest")),
        None => Some((first, "first")),
    }
}

fn handle(
    conn: u64,
    method: Method,
    shared: &Arc<Shared>,
    writer: &SharedWriter,
) -> Result<Value, RpcError> {
    Ok(match method {
        Method::Hello { .. } => {
            let target = shared.state.lock().unwrap().target.clone();
            json!(HelloResult {
                protocol: proto::PROTOCOL_VERSION,
                version: proto::VERSION.to_string(),
                target
            })
        }
        Method::Show {
            location,
            workspace,
            files,
            marks,
            replace,
            user_asked,
        } => {
            let query_error = |message: String| RpcError {
                code: "query_error".to_string(),
                message,
            };
            let (key, dir, name, named) = locate(&location).map_err(query_error)?;
            let pr = match &named {
                Named::Pr(spec) => Some(spec.clone()),
                Named::Repo { .. } => None,
            };
            // The view changes for files or new marks, or when there is
            // nothing else to do: a plain "go there".
            let go = !files.is_empty()
                || marks.iter().any(|m| matches!(m, proto::MarkEntry::Add(_)))
                || marks.is_empty() && !replace;
            // The marks first, by the daemon, all or nothing.
            let (edited, state) = {
                let mut st = shared.state.lock().unwrap();
                if go && pick_viewer(&st.viewers, workspace.as_deref()).is_none() {
                    return Err(no_viewer());
                }
                let loc = st.registry.ensure(key.clone(), dir.clone(), name.clone());
                let mut edited = match loc.marks.edit(&marks, replace, user_asked) {
                    Ok(e) => e,
                    Err(e) => {
                        // Recorded for nothing: an empty location goes.
                        st.drop_unused();
                        return Err(query_error(e));
                    }
                };
                // Lettered as they will stand on screen, top down.
                let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                let diff = key.view().is_none_or(crate::location::View::is_diff);
                crate::marks::in_view_order(&mut edited.adds, &paths, diff);
                let state = loc.clone();
                let mut notes = Vec::new();
                if !edited.deleted.is_empty() {
                    let letters: Vec<String> =
                        edited.deleted.iter().map(|l| format!("[{l}]")).collect();
                    notes.push(format!("agent deleted {}", letters.join(" ")));
                }
                if !edited.moved.is_empty() {
                    notes.push(format!("agent moved {}", proto::lettered(&edited.moved)));
                }
                if !notes.is_empty() {
                    st.announce(&key, Some(notes.join("; ")));
                }
                (edited, state)
            };
            let mut result = ShowResult {
                shown: String::new(),
                marked: Vec::new(),
                moved: edited.moved.clone(),
                deleted: edited.deleted.clone(),
                viewer: String::new(),
                report: Report::default(),
            };
            let mut notes = Vec::new();
            if go {
                let (viewer, id, rx) = {
                    let mut st = shared.state.lock().unwrap();
                    let Some((viewer, picked_by)) = pick_viewer(&st.viewers, workspace.as_deref())
                    else {
                        return Err(no_viewer());
                    };
                    let viewer = viewer.id.clone();
                    if let Some(d) = key.dir() {
                        st.target = Some(Target {
                            dir: d.to_path_buf(),
                            base: None,
                            repo: true,
                        });
                    }
                    eprintln!(
                        "show {} in {viewer} ({picked_by}; caller on {:?})",
                        key.name(),
                        workspace
                    );
                    let (id, rx) = st.expect_reply();
                    st.broadcast(Event::Show {
                        viewer: viewer.clone(),
                        show: proto::ShowEvent {
                            state,
                            pr,
                            view: proto::ViewSpec {
                                files: files.clone(),
                                marks: edited.adds.clone(),
                            },
                        },
                        id: Some(id.clone()),
                    });
                    (viewer, id, rx)
                };
                result.viewer = viewer;
                result.shown = shown_words(&key, &files, !edited.adds.is_empty());
                // An older viewer never answers; the result then says less.
                let ack: Option<proto::ShowAck> = rx
                    .recv_timeout(REPLY_TIMEOUT)
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok());
                let mut st = shared.state.lock().unwrap();
                st.forget_reply(&id);
                match ack {
                    Some(ack) => {
                        if !ack.applied {
                            notes.push(
                                "the user is busy here; the view and the new marks wait until they are done or you call review"
                                    .to_string(),
                            );
                        }
                        // The viewer lettered the marks from its own copy,
                        // which may have been behind the daemon's: the
                        // letters that count are the ones the daemon gave,
                        // when it applied the viewer's op just before this
                        // answer.
                        let n = ack.marks.len();
                        result.marked = ack.marks;
                        if n > 0
                            && let Some(loc) = st.registry.get(&key)
                        {
                            let fresh: Vec<proto::MarkInfo> = loc
                                .marks
                                .marks
                                .iter()
                                .rev()
                                .take(n)
                                .rev()
                                .map(crate::marks::Mark::info)
                                .collect();
                            if fresh.len() == n {
                                result.marked = fresh;
                            }
                        }
                    }
                    None => {
                        if !edited.adds.is_empty() {
                            notes.push(
                                "the viewer did not say which letters it gave; call review to see them"
                                    .to_string(),
                            );
                        } else {
                            notes.push(
                                "the viewer did not answer; it may be an older version".to_string(),
                            );
                        }
                    }
                }
            }
            let st = shared.state.lock().unwrap();
            result.report = match st.registry.get(&key) {
                Some(loc) => Report::of(&key.name(), &loc.marks),
                None => Report::of(&key.name(), &Default::default()),
            };
            result.report.notes = notes;
            drop(st);
            json!(result)
        }
        Method::Query { query } => {
            let mut st = shared.state.lock().unwrap();
            match st.answer(query) {
                Ok(v) => v,
                Err(message) => {
                    return Err(RpcError {
                        code: "query_error".to_string(),
                        message,
                    });
                }
            }
        }
        Method::ViewerReply { id, result } => {
            let mut st = shared.state.lock().unwrap();
            if let Some(pos) = st.pending.iter().position(|(p, _)| *p == id) {
                let (_, tx) = st.pending.remove(pos);
                let _ = tx.send(result);
            }
            json!({ "ok": true })
        }
        Method::Status => {
            let st = shared.state.lock().unwrap();
            json!(StatusResult {
                target: st.target.clone(),
                viewers: st.viewers.iter().map(|(_, v)| v.clone()).collect(),
                locations: st.locations_info(),
            })
        }
        Method::ViewerRegister(entry) => {
            let mut st = shared.state.lock().unwrap();
            match st.viewers.iter_mut().find(|(c, _)| *c == conn) {
                Some((_, existing)) => {
                    if *existing != entry {
                        *existing = entry.clone();
                        st.broadcast(Event::ViewerUpdated(entry));
                    }
                }
                None => {
                    eprintln!(
                        "viewer {} connected (pid {}, workspace {:?})",
                        entry.id, entry.pid, entry.workspace
                    );
                    st.viewers.push((conn, entry.clone()));
                    st.broadcast(Event::ViewerConnected(entry));
                }
            }
            json!({ "ok": true })
        }
        Method::ViewerLocation(state) => {
            let mut st = shared.state.lock().unwrap();
            let (record, new) = st.registry.adopt(state);
            let ev = Event::Location {
                state: record.clone(),
                note: None,
            };
            if new {
                st.broadcast(ev);
            } else {
                st.send_to(conn, ev);
            }
            json!({ "ok": true })
        }
        Method::ViewerOp { key, op } => {
            let mut st = shared.state.lock().unwrap();
            let Some(loc) = st.registry.get_mut(&key) else {
                return Err(RpcError {
                    code: "unknown_location".to_string(),
                    message: format!("{} is not a location here", key.name()),
                });
            };
            loc.marks.apply(&op);
            st.announce(&key, None);
            json!({ "ok": true })
        }
        Method::ViewerClose { .. } => {
            shared.state.lock().unwrap().drop_unused();
            json!({ "ok": true })
        }
        Method::Subscribe => {
            let (tx, rx) = std::sync::mpsc::channel::<Event>();
            shared.state.lock().unwrap().subscribers.push((conn, tx));
            let writer = Arc::clone(writer);
            std::thread::spawn(move || {
                for ev in rx {
                    if !send(&writer, &to_line(&ev)) {
                        break;
                    }
                }
            });
            json!({ "ok": true })
        }
        Method::DaemonShutdown => {
            shared.shutdown.store(true, Ordering::Relaxed);
            json!({ "ok": true })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::View;
    use crate::marks::{Author, Op};
    use crate::proto::{Client, LocationState, MarkEntry, OpenLocation, Request};

    /// A daemon on a socket of its own, and clients for it.
    struct Daemon {
        socket: std::path::PathBuf,
        dir: std::path::PathBuf,
    }

    impl Daemon {
        fn start(name: &str) -> Self {
            let dir = crate::testutil::fresh_dir(&format!("daemon-{name}"));
            let socket = dir.join("d.sock");
            let at = socket.clone();
            std::thread::spawn(move || run_at(at));
            for _ in 0..100 {
                if UnixStream::connect(&socket).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Self { socket, dir }
        }

        fn client(&self, prefix: &'static str) -> Client {
            let stream = UnixStream::connect(&self.socket).unwrap();
            // Generous: the suite runs many tests at once.
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut c = Client::new(stream, prefix).unwrap();
            c.hello().unwrap();
            c
        }

        /// A viewer called `id`, registered and subscribed, with `open`
        /// open and the first of them on screen.
        fn viewer(&self, id: &str, open: &[&LocationState]) -> Client {
            let mut c = self.client("v");
            c.call(Method::Subscribe).unwrap();
            c.call(Method::ViewerRegister(entry(id, open))).unwrap();
            for l in open {
                c.call(Method::ViewerLocation((*l).clone())).unwrap();
            }
            c
        }

        fn query(&self, query: AgentQuery) -> Result<Value, String> {
            let mut c = self.client("c");
            c.call(Method::Query { query })
        }

        /// A show from an agent, in a thread of its own since the daemon
        /// waits for the viewer's answer.
        fn show(
            &self,
            location: &str,
            files: Vec<proto::FileSpec>,
            marks: Vec<MarkEntry>,
            replace: bool,
        ) -> std::thread::JoinHandle<Result<Value, String>> {
            let socket = self.socket.clone();
            let location = location.to_string();
            std::thread::spawn(move || {
                let stream = UnixStream::connect(&socket).unwrap();
                let mut c = Client::new(stream, "c").unwrap();
                c.hello().unwrap();
                c.call(Method::Show {
                    location,
                    workspace: None,
                    files,
                    marks,
                    replace,
                    user_asked: false,
                })
            })
        }
    }

    fn entry(id: &str, open: &[&LocationState]) -> ViewerEntry {
        ViewerEntry {
            id: id.to_string(),
            pid: 1,
            since: 0,
            platform: None,
            window: None,
            workspace: None,
            position: None,
            target: None,
            location: open.first().map(|l| l.key.clone()),
            open: open
                .iter()
                .map(|l| OpenLocation {
                    key: l.key.clone(),
                    showing: format!("{} · everything", l.name),
                })
                .collect(),
        }
    }

    /// A checkout's branch view, as a viewer has it open.
    fn repo(dir: &std::path::Path) -> LocationState {
        LocationState::fresh(
            LocationKey::Repo {
                dir: dir.to_path_buf(),
                view: View::Branch,
            },
            dir.to_path_buf(),
            dir.file_name().unwrap().to_string_lossy().into_owned(),
        )
    }

    fn with_comment(mut l: LocationState, text: &str) -> LocationState {
        l.marks.apply(&Op::Mark {
            author: Author::User,
            path: "a.rs".to_string(),
            start: 3,
            end: 3,
            old_side: false,
            with_deletions: false,
            text: text.to_string(),
            lines: vec!["let a = 1;".to_string()],
        });
        l
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let mut c = self.client("x");
            let _ = c.call(Method::DaemonShutdown);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The next show or location event a viewer gets; the daemon's news
    /// about viewers coming and going is skipped.
    fn next_event(reader: &mut proto::FrameReader) -> Event {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "daemon closed");
            if let Ok(proto::ServerFrame::Event(e)) = proto::ServerFrame::parse(&line)
                && matches!(e, Event::Show { .. } | Event::Location { .. })
            {
                return e;
            }
        }
    }

    fn location_event(reader: &mut proto::FrameReader) -> (LocationState, Option<String>) {
        match next_event(reader) {
            Event::Location { state, note } => (state, note),
            other => panic!("a location event, not {other:?}"),
        }
    }

    #[test]
    fn an_older_protocol_or_version_makes_way() {
        let hello = |protocol: u32, version: &str| HelloResult {
            protocol,
            version: version.to_string(),
            target: None,
        };
        assert!(outdated(&hello(
            proto::PROTOCOL_VERSION - 1,
            proto::VERSION
        )));
        assert!(outdated(&hello(proto::PROTOCOL_VERSION, "0.0.1")));
        assert!(!outdated(&hello(proto::PROTOCOL_VERSION, proto::VERSION)));
        assert!(!outdated(&hello(
            proto::PROTOCOL_VERSION + 1,
            proto::VERSION
        )));
    }

    #[test]
    fn a_viewers_location_is_shared_and_an_agent_reads_it_through_the_daemon() {
        let d = Daemon::start("share");
        let checkout = crate::testutil::branch_with_history("checkout-share");
        let at = format!("{}#branch", checkout.display());
        let loc = with_comment(repo(&checkout), "rename this");
        let viewer = d.viewer("v1", &[&loc]);
        let (_, mut reader) = viewer.into_parts();
        // Opening a location gets its shared state back: its own, being new.
        let (state, _) = location_event(&mut reader);
        assert_eq!(state.marks.marks[0].label, "A");

        let v = d
            .query(AgentQuery::Review {
                location: format!("{}/src#branch", checkout.display()),
                letters: Vec::new(),
            })
            .unwrap();
        let r: proto::ReviewResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.report.location, at, "named by the root");
        assert_eq!((r.new, r.report.unread), (1, 0));
        assert!(r.marks[0].new);
        // Read, the viewer hears that the comment is no longer unread.
        let (state, note) = location_event(&mut reader);
        assert!(state.marks.marks[0].seen && note.is_none());

        let v = d
            .query(AgentQuery::Reply {
                location: at.clone(),
                letter: "a".to_string(),
                text: "done".to_string(),
            })
            .unwrap();
        assert_eq!(v["letter"], "A");
        let (state, note) = location_event(&mut reader);
        assert_eq!(state.marks.marks[0].thread[0].author, Author::Agent);
        assert_eq!(note.as_deref(), Some("agent replied under [A]"));

        // Another view of the same checkout is a location of its own.
        let v = d
            .query(AgentQuery::Review {
                location: checkout.display().to_string(),
                letters: Vec::new(),
            })
            .unwrap();
        let r: proto::ReviewResult = serde_json::from_value(v).unwrap();
        assert!(r.marks.is_empty() && r.report.total == 0);
        assert_eq!(r.report.location, checkout.display().to_string());
        let err = d
            .query(AgentQuery::Review {
                location: checkout.display().to_string(),
                letters: vec!["A".to_string()],
            })
            .unwrap_err();
        assert!(err.starts_with("no marks on"), "{err}");
        let err = d
            .query(AgentQuery::Reply {
                location: "/nowhere/at/all".to_string(),
                letter: "A".to_string(),
                text: "x".to_string(),
            })
            .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        let err = d
            .query(AgentQuery::Review {
                location: "src".to_string(),
                letters: Vec::new(),
            })
            .unwrap_err();
        assert!(err.contains("absolute directory"), "{err}");
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn a_second_viewer_opening_the_same_location_gets_the_marks_already_there() {
        let d = Daemon::start("second");
        let checkout = crate::testutil::branch_with_history("checkout-second");
        let loc = with_comment(repo(&checkout), "mine");
        let _first = d.viewer("v1", &[&loc]);
        let second = d.viewer("v2", &[&repo(&checkout)]);
        let (_, mut reader) = second.into_parts();
        let (state, _) = location_event(&mut reader);
        assert_eq!(
            state.marks.marks.len(),
            1,
            "the second viewer takes over the first's marks"
        );
        let mut c = d.client("c");
        let v = c.call(Method::Status).unwrap();
        let st: StatusResult = serde_json::from_value(v).unwrap();
        assert_eq!(st.locations.len(), 1);
        assert_eq!(st.locations[0].viewers, ["v1", "v2"]);
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn marks_survive_their_viewer_and_empty_locations_do_not() {
        let d = Daemon::start("survive");
        let kept_dir = crate::testutil::branch_with_history("checkout-kept");
        let kept = with_comment(repo(&kept_dir), "still here");
        let empty = repo(std::path::Path::new("/work/empty"));
        let viewer = d.viewer("v1", &[&kept, &empty]);
        drop(viewer);
        for _ in 0..50 {
            let mut c = d.client("c");
            let st: StatusResult = serde_json::from_value(c.call(Method::Status).unwrap()).unwrap();
            if st.locations.len() == 1 {
                assert_eq!(st.locations[0].location, kept.key.name());
                assert!(st.locations[0].viewers.is_empty());
                assert!(
                    st.locations[0].render().contains("not open in any viewer"),
                    "{}",
                    st.locations[0].render()
                );
                let _ = std::fs::remove_dir_all(kept_dir);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the empty location went and the commented one stayed");
    }

    #[test]
    fn a_viewers_op_reaches_the_other_viewer() {
        let d = Daemon::start("op");
        let loc = repo(std::path::Path::new("/work/repo"));
        let a = d.viewer("v1", &[&loc]);
        let b = d.viewer("v2", &[&loc]);
        let (mut aw, _) = a.into_parts();
        let (_, mut br) = b.into_parts();
        let _ = location_event(&mut br);
        let op = Request {
            id: "v".to_string(),
            method: Method::ViewerOp {
                key: loc.key.clone(),
                op: Op::Mark {
                    author: Author::User,
                    path: "a.rs".to_string(),
                    start: 1,
                    end: 2,
                    old_side: false,
                    with_deletions: false,
                    text: "from v1".to_string(),
                    lines: vec![],
                },
            },
        };
        aw.write_all(to_line(&op).as_bytes()).unwrap();
        let (state, _) = location_event(&mut br);
        assert_eq!(state.marks.marks[0].comment.as_deref(), Some("from v1"));
    }

    #[test]
    fn a_show_edits_the_marks_first_then_relays_the_view_and_carries_the_answer() {
        let d = Daemon::start("relay-show");
        let checkout = crate::testutil::branch_with_history("checkout-relay");
        let at = format!("{}#branch", checkout.display());
        // The daemon already holds the user's mark and one of the agent's
        // that the viewer never saw.
        let mut loc = with_comment(repo(&checkout), "kept");
        loc.marks.apply(&Op::Marks {
            marks: vec![proto::MarkSpec {
                path: "a.rs".to_string(),
                start: 9,
                end: 9,
                text: None,
            }],
        });
        let viewer = d.viewer("v1", &[&loc]);
        let (mut writer, mut reader) = viewer.into_parts();
        let _ = location_event(&mut reader);
        let caller = d.show(
            &at,
            vec![proto::FileSpec {
                path: "a.rs".to_string(),
                lines: None,
            }],
            vec![
                MarkEntry::Add(proto::MarkSpec {
                    path: "b.rs".to_string(),
                    start: 2,
                    end: 2,
                    text: None,
                }),
                MarkEntry::Add(proto::MarkSpec {
                    path: "a.rs".to_string(),
                    start: 7,
                    end: 7,
                    text: None,
                }),
                MarkEntry::Move {
                    letter: "A".to_string(),
                    path: "a.rs".to_string(),
                    start: 5,
                    end: 5,
                },
            ],
            true,
        );
        // Replacing its own took [B]; the move changed [A]; both announced
        // before the view is relayed.
        let (state, note) = location_event(&mut reader);
        assert_eq!(state.marks.marks.len(), 1);
        assert_eq!(
            (
                state.marks.marks[0].start,
                state.marks.marks[0].label.as_str()
            ),
            (5, "A")
        );
        assert_eq!(
            note.as_deref(),
            Some("agent deleted [B]; agent moved [A] a.rs 5")
        );
        let Event::Show {
            show, id: Some(id), ..
        } = next_event(&mut reader)
        else {
            panic!("a show event with an id");
        };
        assert_eq!(show.state.key.name(), at);
        assert_eq!(
            show.state.marks.marks.len(),
            1,
            "the daemon's copy comes along"
        );
        assert_eq!(show.view.files[0].path, "a.rs");
        let placed: Vec<(&str, usize)> = show
            .view
            .marks
            .iter()
            .map(|m| (m.path.as_str(), m.start))
            .collect();
        assert_eq!(
            placed,
            [("a.rs", 7), ("b.rs", 2)],
            "the new marks are the viewer's to place, in the order it shows them"
        );
        // The viewer, lettering from a copy that was behind, calls its new
        // marks [B] [C] and says so right after its op; the daemon knows
        // them as [C] [D].
        let op = Request {
            id: "v".to_string(),
            method: Method::ViewerOp {
                key: loc.key.clone(),
                op: Op::Marks {
                    marks: show.view.marks.clone(),
                },
            },
        };
        writer.write_all(to_line(&op).as_bytes()).unwrap();
        let reply = Request {
            id: "v".to_string(),
            method: Method::ViewerReply {
                id,
                result: json!({ "marks": [
                    { "label": "B", "author": "agent", "path": "a.rs", "start": 7, "end": 7 },
                    { "label": "C", "author": "agent", "path": "b.rs", "start": 2, "end": 2 }
                ], "applied": true }),
            },
        };
        writer.write_all(to_line(&reply).as_bytes()).unwrap();
        let r: ShowResult = serde_json::from_value(caller.join().unwrap().unwrap()).unwrap();
        assert_eq!(r.shown, "everything since the branch point · a.rs");
        let marked: Vec<&str> = r.marked.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(marked, ["C", "D"], "the daemon's letters, not the viewer's");
        assert_eq!(r.moved[0].label, "A");
        assert_eq!(r.deleted, ["B"]);
        assert_eq!(r.viewer, "v1");
        assert_eq!(r.report.location, at);
        assert_eq!(
            (r.report.total, r.report.by_user, r.report.unread),
            (3, 1, 1)
        );
        assert_eq!(r.report.mine[0].label, "C");
        assert!(r.report.notes.is_empty(), "{:?}", r.report.notes);
        let text = r.render();
        assert!(text.starts_with("shown: everything since the branch point · a.rs\nmarked: [C] a.rs 7, [D] b.rs 2\nmoved: [A] a.rs 5\ndeleted: [B]\nlocation: "), "{text}");

        // The viewer's op was announced too.
        let (state, _) = location_event(&mut reader);
        assert_eq!(state.marks.marks.len(), 3);
        // Only deletions: the marks change, the view does not.
        let caller = d.show(
            &at,
            Vec::new(),
            vec![MarkEntry::Delete {
                letter: "C".to_string(),
            }],
            false,
        );
        let (state, note) = location_event(&mut reader);
        assert_eq!(state.marks.marks.len(), 2);
        assert_eq!(note.as_deref(), Some("agent deleted [C]"));
        let r: ShowResult = serde_json::from_value(caller.join().unwrap().unwrap()).unwrap();
        assert!(r.shown.is_empty() && r.viewer.is_empty());
        assert_eq!(r.deleted, ["C"]);
        // The user's mark needs the flag: nothing changes.
        let caller = d.show(
            &at,
            Vec::new(),
            vec![MarkEntry::Delete {
                letter: "A".to_string(),
            }],
            false,
        );
        let err = caller.join().unwrap().unwrap_err();
        assert!(err.starts_with("[A] is the user's"), "{err}");
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn a_show_opens_a_location_the_daemon_never_saw() {
        let d = Daemon::start("relay-new");
        let checkout = crate::testutil::branch_with_history("checkout-new");
        let other = repo(std::path::Path::new("/work/other"));
        let viewer = d.viewer("v1", &[&other]);
        let (mut writer, mut reader) = viewer.into_parts();
        let _ = location_event(&mut reader);
        let caller = d.show(
            &format!("{}/src", checkout.display()),
            Vec::new(),
            Vec::new(),
            false,
        );
        let Event::Show {
            show, id: Some(id), ..
        } = next_event(&mut reader)
        else {
            panic!("a show event with an id");
        };
        assert_eq!(
            show.state.key.name(),
            checkout.display().to_string(),
            "the root, as the files are"
        );
        assert_eq!(show.state.dir, checkout);
        assert!(show.pr.is_none() && show.view.files.is_empty());
        let reply = Request {
            id: "v".to_string(),
            method: Method::ViewerReply {
                id,
                result: json!({ "marks": [], "applied": false }),
            },
        };
        writer.write_all(to_line(&reply).as_bytes()).unwrap();
        let r: ShowResult = serde_json::from_value(caller.join().unwrap().unwrap()).unwrap();
        assert_eq!(r.shown, "the working tree · all files");
        assert!(
            r.report.notes[0].starts_with("the user is busy"),
            "{:?}",
            r.report.notes
        );
        let _ = std::fs::remove_dir_all(checkout);
    }

    #[test]
    fn a_show_with_no_viewer_fails_at_once() {
        let d = Daemon::start("relay-none");
        let checkout = crate::testutil::branch_with_history("checkout-none");
        let err = d
            .show(
                &checkout.display().to_string(),
                Vec::new(),
                Vec::new(),
                false,
            )
            .join()
            .unwrap()
            .unwrap_err();
        assert!(err.contains("no codesnob viewer"), "{err}");
        let mut c = d.client("c");
        let st: StatusResult = serde_json::from_value(c.call(Method::Status).unwrap()).unwrap();
        assert!(
            st.locations.is_empty(),
            "nothing recorded for a show that went nowhere"
        );
        let _ = std::fs::remove_dir_all(checkout);
    }
}

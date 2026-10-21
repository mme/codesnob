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

use crate::proto::{
    self, AgentQuery, Event, HelloResult, LocationInfo, LocationKey, LocationRef, Method, Response,
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

    /// What `key` shows, in words: in the viewer `prefer` first, else in
    /// any viewer that has it open.
    fn showing(&self, key: &LocationKey, prefer: Option<&str>) -> Option<String> {
        let in_viewer = |id: &str| {
            self.viewers
                .iter()
                .find(|(_, v)| v.id == id)
                .and_then(|(_, v)| v.open.iter().find(|o| o.key == *key))
                .map(|o| o.showing.clone())
        };
        prefer.and_then(in_viewer).or_else(|| {
            self.viewers
                .iter()
                .find_map(|(_, v)| v.open.iter().find(|o| o.key == *key))
                .map(|o| o.showing.clone())
        })
    }

    /// Every location, for an agent whose requests reach `picked`.
    fn locations_info(&self, picked: Option<&ViewerEntry>) -> Vec<LocationInfo> {
        let on_screen = picked.and_then(|v| v.location.as_ref());
        let mut infos: Vec<LocationInfo> = self
            .registry
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
                    showing: self
                        .showing(&l.key, picked.map(|v| v.id.as_str()))
                        .unwrap_or_default(),
                    on_screen: on_screen == Some(&l.key),
                    viewers,
                    marks: l.marks.marks.len(),
                    by_user: l.marks.users_words(),
                    unread: l.marks.unread(),
                }
            })
            .collect();
        infos.sort_by_key(|i| !i.on_screen);
        infos
    }

    /// Answer an agent's question about marks.
    fn answer(
        &mut self,
        workspace: Option<&str>,
        cwd: Option<&std::path::Path>,
        query: AgentQuery,
    ) -> Result<Value, String> {
        let picked = pick_viewer(&self.viewers, workspace).map(|(v, _)| v.clone());
        let on_screen = picked.as_ref().and_then(|v| v.location.clone());
        let viewer_id = picked.as_ref().map(|v| v.id.clone());
        let resolve = |st: &Self, location: Option<LocationRef>| {
            st.registry
                .resolve(&location.unwrap_or_default(), on_screen.as_ref(), cwd)
        };
        match query {
            AgentQuery::Locations => Ok(json!(proto::LocationsResult {
                locations: self.locations_info(picked.as_ref()),
            })),
            AgentQuery::Review { location } => {
                let key = resolve(self, location)?;
                let showing = self
                    .showing(&key, viewer_id.as_deref())
                    .unwrap_or_else(|| "not open in any viewer".to_string());
                let loc = self.registry.get_mut(&key).expect("resolved");
                let unread = loc.marks.unread();
                let result = proto::ReviewResult {
                    location: key.name(),
                    dir: loc.dir.clone(),
                    showing,
                    marks: loc.marks.read(),
                    unread,
                };
                // Read, the comments no longer hold an agent's request back.
                self.announce(&key, None);
                Ok(json!(result))
            }
            AgentQuery::ClearMarks {
                location,
                which,
                letters,
                user_asked,
            } => {
                let key = resolve(self, location)?;
                let loc = self.registry.get_mut(&key).expect("resolved");
                let cleared = loc.marks.clear(&which, &letters, user_asked)?;
                let result = proto::ClearResult {
                    location: key.name(),
                    cleared: cleared.clone(),
                    remaining: loc.marks.marks.len(),
                    unread: loc.marks.unread(),
                };
                let note = (!cleared.is_empty()).then(|| {
                    format!(
                        "agent cleared {}",
                        cleared
                            .iter()
                            .map(|l| format!("[{l}]"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                });
                self.announce(&key, note);
                Ok(json!(result))
            }
            AgentQuery::Reply {
                location,
                letter,
                text,
            } => {
                let key = resolve(self, location)?;
                let loc = self.registry.get_mut(&key).expect("resolved");
                let letter = loc.marks.reply(&letter, &text)?;
                let result = proto::ReplyResult {
                    location: key.name(),
                    letter: letter.clone(),
                    unread: loc.marks.unread(),
                };
                self.announce(&key, Some(format!("agent replied under [{letter}]")));
                Ok(json!(result))
            }
            AgentQuery::Mark { location, marks } => {
                let key = resolve(self, location)?;
                let loc = self.registry.get_mut(&key).expect("resolved");
                let placed = loc.marks.mark(&marks)?;
                let result = proto::MarkResult {
                    location: key.name(),
                    marks: placed,
                    mine: loc.marks.mine(),
                    unread: loc.marks.unread(),
                };
                let note = format!("agent marked {}", result.placed());
                self.announce(&key, Some(note));
                Ok(json!(result))
            }
            AgentQuery::MoveMark {
                location,
                letter,
                path,
                start,
                end,
            } => {
                let key = resolve(self, location)?;
                let loc = self.registry.get_mut(&key).expect("resolved");
                let m = loc.marks.move_to(&letter, path, start, end)?;
                let result = proto::MoveResult {
                    location: key.name(),
                    label: m.label.clone(),
                    path: m.path.clone(),
                    start,
                    end,
                    unread: loc.marks.unread(),
                };
                let note = format!(
                    "agent moved [{}] to {} {}",
                    result.label,
                    result.path,
                    result.lines_word()
                );
                self.announce(&key, Some(note));
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
    if !is_newer(proto::VERSION, &hello.version) {
        return Existing::Newer;
    }
    let _ = client.call(Method::DaemonShutdown);
    Existing::Replaced
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
            dir,
            base,
            workspace,
            view,
        } => {
            let repo = gix::discover(&dir).is_ok();
            let target = Target { dir, base, repo };
            let (mut result, rx) = {
                let mut st = shared.state.lock().unwrap();
                let Some((viewer, picked_by)) = pick_viewer(&st.viewers, workspace.as_deref())
                else {
                    return Err(no_viewer());
                };
                let result = ShowResult {
                    repo,
                    viewer: viewer.id.clone(),
                    workspace: viewer.workspace.clone(),
                    picked_by: picked_by.to_string(),
                    ack: None,
                };
                st.target = Some(target.clone());
                eprintln!(
                    "show {} in {} (workspace {:?}, {picked_by}; caller on {:?})",
                    target.dir.display(),
                    result.viewer,
                    result.workspace,
                    workspace
                );
                let (id, rx) = st.expect_reply();
                st.broadcast(Event::Show {
                    viewer: result.viewer.clone(),
                    target,
                    view,
                    id: Some(id.clone()),
                });
                (result, (id, rx))
            };
            let (id, rx) = rx;
            // An older viewer never answers; the result then says less.
            result.ack = rx
                .recv_timeout(REPLY_TIMEOUT)
                .ok()
                .and_then(|v| serde_json::from_value(v).ok());
            shared.state.lock().unwrap().forget_reply(&id);
            json!(result)
        }
        Method::Query {
            workspace,
            cwd,
            query,
        } => {
            let mut st = shared.state.lock().unwrap();
            match st.answer(workspace.as_deref(), cwd.as_deref(), query) {
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
                locations: st.locations_info(None),
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
    use crate::marks::{Author, Marks, Op};
    use crate::proto::{Client, LocationState, OpenLocation, Request};

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
            c.call(Method::Query {
                workspace: None,
                cwd: None,
                query,
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

    fn repo(dir: &str) -> LocationState {
        LocationState {
            key: LocationKey::Repo { dir: dir.into() },
            dir: dir.into(),
            name: dir.rsplit('/').next().unwrap().to_string(),
            marks: Marks::default(),
        }
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
    fn a_viewers_location_is_shared_and_an_agent_reads_it_through_the_daemon() {
        let d = Daemon::start("share");
        let loc = with_comment(repo("/work/repo"), "rename this");
        let viewer = d.viewer("v1", &[&loc]);
        let (_, mut reader) = viewer.into_parts();
        // Opening a location gets its shared state back: its own, being new.
        let (state, _) = location_event(&mut reader);
        assert_eq!(state.marks.marks[0].label, "A");

        let v = d.query(AgentQuery::Review { location: None }).unwrap();
        let r: proto::ReviewResult = serde_json::from_value(v).unwrap();
        assert_eq!((r.location.as_str(), r.unread), ("/work/repo", 1));
        assert_eq!(r.showing, "repo · everything");
        assert!(r.marks[0].new);
        // Read, the viewer hears that the comment is no longer unread.
        let (state, note) = location_event(&mut reader);
        assert!(state.marks.marks[0].seen && note.is_none());

        let v = d
            .query(AgentQuery::Reply {
                location: Some(LocationRef {
                    repo: Some("/work/repo".into()),
                    pr: None,
                }),
                letter: "a".to_string(),
                text: "done".to_string(),
            })
            .unwrap();
        assert_eq!(v["letter"], "A");
        let (state, note) = location_event(&mut reader);
        assert_eq!(state.marks.marks[0].thread[0].author, Author::Agent);
        assert_eq!(note.as_deref(), Some("agent replied under [A]"));

        // The agent's own mark without a show, lettered on.
        let v = d
            .query(AgentQuery::Mark {
                location: None,
                marks: vec![
                    proto::MarkSpec {
                        path: "a.rs".to_string(),
                        start: 5,
                        end: 6,
                        text: Some("unused".to_string()),
                    },
                    proto::MarkSpec {
                        path: "b.rs".to_string(),
                        start: 1,
                        end: 1,
                        text: Some("this too".to_string()),
                    },
                ],
            })
            .unwrap();
        assert_eq!(v["marks"][0]["label"], "B");
        assert_eq!(v["marks"][1]["label"], "C");
        assert_eq!(v["mine"][0]["label"], "B", "the agent's marks come back");
        let (state, note) = location_event(&mut reader);
        let b = &state.marks.marks[1];
        assert!(b.author == Author::Agent && b.text.is_none());
        assert_eq!(
            note.as_deref(),
            Some("agent marked [B] a.rs 5-6, [C] b.rs 1"),
            "one event for all of them"
        );
        let err = d
            .query(AgentQuery::Mark {
                location: None,
                marks: vec![proto::MarkSpec {
                    path: "a.rs".to_string(),
                    start: 0,
                    end: 0,
                    text: Some("nowhere".to_string()),
                }],
            })
            .unwrap_err();
        assert!(err.contains("lines start at 1"), "{err}");

        let err = d
            .query(AgentQuery::ClearMarks {
                location: None,
                which: "user".to_string(),
                letters: Vec::new(),
                user_asked: false,
            })
            .unwrap_err();
        assert!(err.starts_with("[A] is the user's"), "{err}");
        let err = d
            .query(AgentQuery::Review {
                location: Some(LocationRef {
                    repo: Some("/elsewhere".into()),
                    pr: None,
                }),
            })
            .unwrap_err();
        assert_eq!(err, "nothing is open for /elsewhere; show it first");
    }

    #[test]
    fn a_second_viewer_opening_the_same_location_gets_the_marks_already_there() {
        let d = Daemon::start("second");
        let loc = with_comment(repo("/work/repo"), "mine");
        let _first = d.viewer("v1", &[&loc]);
        let second = d.viewer("v2", &[&repo("/work/repo")]);
        let (_, mut reader) = second.into_parts();
        let (state, _) = location_event(&mut reader);
        assert_eq!(
            state.marks.marks.len(),
            1,
            "the second viewer takes over the first's marks"
        );
        let v = d.query(AgentQuery::Locations).unwrap();
        let r: proto::LocationsResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.locations.len(), 1);
        assert_eq!(r.locations[0].viewers, ["v1", "v2"]);
        assert!(r.locations[0].on_screen);
    }

    #[test]
    fn marks_survive_their_viewer_and_empty_locations_do_not() {
        let d = Daemon::start("survive");
        let kept = with_comment(repo("/work/kept"), "still here");
        let empty = repo("/work/empty");
        let viewer = d.viewer("v1", &[&kept, &empty]);
        drop(viewer);
        for _ in 0..50 {
            let v = d.query(AgentQuery::Locations).unwrap();
            let r: proto::LocationsResult = serde_json::from_value(v).unwrap();
            if r.locations.len() == 1 {
                assert_eq!(r.locations[0].location, "/work/kept");
                assert!(r.locations[0].viewers.is_empty());
                assert!(
                    r.render().contains("not open in any viewer"),
                    "{}",
                    r.render()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the empty location went and the commented one stayed");
    }

    #[test]
    fn a_viewers_op_reaches_the_other_viewer() {
        let d = Daemon::start("op");
        let loc = repo("/work/repo");
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
    fn a_show_carries_the_viewers_answer_when_it_gives_one() {
        let d = Daemon::start("relay-show");
        let viewer = d.viewer("v1", &[]);
        let (mut writer, mut reader) = viewer.into_parts();
        let dir = d.dir.clone();
        let caller = std::thread::spawn({
            let socket = d.socket.clone();
            move || {
                let stream = UnixStream::connect(&socket).unwrap();
                let mut c = Client::new(stream, "c").unwrap();
                c.hello().unwrap();
                c.call(Method::Show {
                    dir,
                    base: None,
                    workspace: None,
                    view: None,
                })
            }
        });
        let Event::Show { id: Some(id), .. } = next_event(&mut reader) else {
            panic!("a show event with an id");
        };
        let reply = Request {
            id: "v".to_string(),
            method: Method::ViewerReply {
                id,
                result: json!({ "location": "/x", "dir": "/x", "marks": [], "unread": 1, "applied": true }),
            },
        };
        writer.write_all(to_line(&reply).as_bytes()).unwrap();
        let r: ShowResult = serde_json::from_value(caller.join().unwrap().unwrap()).unwrap();
        let ack = r.ack.expect("the viewer's answer");
        assert_eq!((ack.location.as_str(), ack.unread), ("/x", 1));
    }

    #[test]
    fn a_show_with_no_viewer_fails_at_once_and_a_query_needs_a_location() {
        let d = Daemon::start("relay-none");
        let mut c = d.client("c");
        let err = c
            .call(Method::Show {
                dir: d.dir.clone(),
                base: None,
                workspace: None,
                view: None,
            })
            .unwrap_err();
        assert!(err.contains("no codesnob viewer"), "{err}");
        let err = d.query(AgentQuery::Review { location: None }).unwrap_err();
        assert_eq!(err, "no location is on screen; name one with repo or pr");
        let v = d.query(AgentQuery::Locations).unwrap();
        let r: proto::LocationsResult = serde_json::from_value(v).unwrap();
        assert!(r.locations.is_empty());
    }
}

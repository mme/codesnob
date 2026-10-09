//! The headless daemon: one per user, holds the current display target and
//! broadcasts changes to subscribed viewers. Started on demand by clients.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::proto::{
    self, Event, HelloResult, Method, Response, RpcError, ShowResult, StatusResult, Target,
    ViewerEntry, to_line,
};

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
}

impl State {
    fn broadcast(&mut self, ev: Event) {
        self.subscribers
            .retain(|(_, tx)| tx.send(ev.clone()).is_ok());
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
        Method::Query { workspace, query } => {
            let rx = {
                let mut st = shared.state.lock().unwrap();
                let Some((viewer, _)) = pick_viewer(&st.viewers, workspace.as_deref()) else {
                    return Err(no_viewer());
                };
                let viewer = viewer.id.clone();
                let (id, rx) = st.expect_reply();
                st.broadcast(Event::Query {
                    viewer,
                    id: id.clone(),
                    query,
                });
                (id, rx)
            };
            let (id, rx) = rx;
            let answer = rx.recv_timeout(REPLY_TIMEOUT);
            shared.state.lock().unwrap().forget_reply(&id);
            match answer {
                Ok(v) => match v.get("error").and_then(Value::as_str) {
                    Some(message) => {
                        return Err(RpcError {
                            code: "viewer_error".to_string(),
                            message: message.to_string(),
                        });
                    }
                    None => v,
                },
                Err(_) => {
                    return Err(RpcError {
                        code: "no_reply".to_string(),
                        message: "the viewer did not answer; it may be an older version"
                            .to_string(),
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
                viewers: st.viewers.iter().map(|(_, v)| v.clone()).collect()
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
    use crate::proto::{Client, Request, ViewerQuery};
    use std::io::BufRead;

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
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut c = Client::new(stream, prefix).unwrap();
            c.hello().unwrap();
            c
        }

        /// A viewer called `id`, registered and subscribed.
        fn viewer(&self, id: &str) -> Client {
            let mut c = self.client("v");
            c.call(Method::Subscribe).unwrap();
            c.call(Method::ViewerRegister(ViewerEntry {
                id: id.to_string(),
                pid: 1,
                since: 0,
                platform: None,
                window: None,
                workspace: None,
                position: None,
                target: None,
            }))
            .unwrap();
            c
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let mut c = self.client("x");
            let _ = c.call(Method::DaemonShutdown);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The next show or query event a viewer gets; the daemon's news about
    /// viewers coming and going is skipped.
    fn next_event(reader: &mut std::io::BufReader<UnixStream>) -> Event {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "daemon closed");
            if let Ok(proto::ServerFrame::Event(e)) = proto::ServerFrame::parse(&line)
                && matches!(e, Event::Show { .. } | Event::Query { .. })
            {
                return e;
            }
        }
    }

    #[test]
    fn a_query_is_relayed_to_the_viewer_and_its_answer_comes_back() {
        let d = Daemon::start("relay");
        let viewer = d.viewer("v1");
        let (mut writer, mut reader) = viewer.into_parts();
        let caller = std::thread::spawn({
            let socket = d.socket.clone();
            move || {
                let stream = UnixStream::connect(&socket).unwrap();
                let mut c = Client::new(stream, "c").unwrap();
                c.hello().unwrap();
                c.call(Method::Query {
                    workspace: None,
                    query: ViewerQuery::Review { location: None },
                })
            }
        });
        let Event::Query { viewer, id, query } = next_event(&mut reader) else {
            panic!("a query event");
        };
        assert_eq!(viewer, "v1");
        assert_eq!(query, ViewerQuery::Review { location: None });
        let reply = Request {
            id: "v".to_string(),
            method: Method::ViewerReply {
                id,
                result: json!({ "location": "L1", "marks": [] }),
            },
        };
        writer.write_all(to_line(&reply).as_bytes()).unwrap();
        let answer = caller.join().unwrap().unwrap();
        assert_eq!(answer["location"], "L1");
    }

    #[test]
    fn a_viewer_that_does_not_answer_is_reported_in_time() {
        let d = Daemon::start("relay-timeout");
        let viewer = d.viewer("v1");
        let (_writer, mut reader) = viewer.into_parts();
        let caller = std::thread::spawn({
            let socket = d.socket.clone();
            move || {
                let stream = UnixStream::connect(&socket).unwrap();
                let mut c = Client::new(stream, "c").unwrap();
                c.hello().unwrap();
                let started = std::time::Instant::now();
                let r = c.call(Method::Query {
                    workspace: None,
                    query: ViewerQuery::Review { location: None },
                });
                (r, started.elapsed())
            }
        });
        let Event::Query { .. } = next_event(&mut reader) else {
            panic!("a query event");
        };
        let (r, took) = caller.join().unwrap();
        let err = r.unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(
            took >= REPLY_TIMEOUT && took < REPLY_TIMEOUT * 2,
            "{took:?}"
        );
    }

    #[test]
    fn a_show_carries_the_viewers_answer_when_it_gives_one() {
        let d = Daemon::start("relay-show");
        let viewer = d.viewer("v1");
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
                result: json!({ "location": "L2", "dir": "/x", "marks": [], "unread": 1, "applied": true }),
            },
        };
        writer.write_all(to_line(&reply).as_bytes()).unwrap();
        let r: ShowResult = serde_json::from_value(caller.join().unwrap().unwrap()).unwrap();
        let ack = r.ack.expect("the viewer's answer");
        assert_eq!((ack.location.as_str(), ack.unread), ("L2", 1));
    }

    #[test]
    fn a_query_with_no_viewer_fails_at_once() {
        let d = Daemon::start("relay-none");
        let mut c = d.client("c");
        let err = c
            .call(Method::Query {
                workspace: None,
                query: ViewerQuery::Review { location: None },
            })
            .unwrap_err();
        assert!(err.contains("no codesnob viewer"), "{err}");
    }
}

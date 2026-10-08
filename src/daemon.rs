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

#[derive(Default)]
struct State {
    target: Option<Target>,
    subscribers: Vec<(u64, Sender<Event>)>,
    /// Registered viewers, keyed by the connection that owns them.
    viewers: Vec<(u64, ViewerEntry)>,
}

impl State {
    fn broadcast(&mut self, ev: Event) {
        self.subscribers
            .retain(|(_, tx)| tx.send(ev.clone()).is_ok());
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
}

pub fn run() -> Result<()> {
    let socket = proto::daemon_socket();
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
            let _ = UnixStream::connect(proto::daemon_socket());
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
                "status",
                "subscribe",
                "viewer.register",
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
            let mut st = shared.state.lock().unwrap();
            let Some((viewer, picked_by)) = pick_viewer(&st.viewers, workspace.as_deref()) else {
                return Err(RpcError {
                    code: "no_viewer".to_string(),
                    message: "no codesnob viewer is connected".to_string(),
                });
            };
            let result = ShowResult {
                repo,
                viewer: viewer.id.clone(),
                workspace: viewer.workspace.clone(),
                picked_by: picked_by.to_string(),
            };
            st.target = Some(target.clone());
            eprintln!(
                "show {} in {} (workspace {:?}, {picked_by}; caller on {:?})",
                target.dir.display(),
                result.viewer,
                result.workspace,
                workspace
            );
            st.broadcast(Event::Show {
                viewer: result.viewer.clone(),
                target,
                view,
            });
            json!(result)
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

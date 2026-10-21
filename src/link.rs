//! The TUI's long-lived connection to the daemon: register this viewer,
//! announce what it shows, share its locations' marks, follow what agents
//! ask, reconnect with backoff.
//! Placement comes from the desktop backend in `wm` and is re-registered
//! whenever it changes.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::proto::{
    self, AgentQuery, Event, LocationKey, LocationRef, LocationState, Method, OpenLocation,
    Request, ServerFrame, Target, ViewerEntry, to_line,
};
use crate::watch::Msg;
use crate::wm;

const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(30);

struct Inner {
    entry: ViewerEntry,
    /// Write half of the live connection, if any. Registrations are
    /// fire-and-forget; the reader thread ignores their replies.
    writer: Option<UnixStream>,
}

/// Handle shared by the UI, the placement tracker and the link thread.
#[derive(Clone)]
pub struct Link {
    inner: Arc<Mutex<Inner>>,
}

impl Link {
    pub fn new() -> Self {
        let pid = std::process::id();
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = ViewerEntry {
            id: format!("v{pid}"),
            pid,
            since,
            platform: None,
            window: None,
            workspace: None,
            position: None,
            target: None,
            location: None,
            open: Vec::new(),
        };
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entry,
                writer: None,
            })),
        }
    }

    /// What this viewer shows: the target and location on screen, and
    /// every location open.
    pub fn set_showing(
        &self,
        target: Option<Target>,
        location: Option<LocationKey>,
        open: Vec<OpenLocation>,
    ) {
        self.update(|e| {
            e.target = target;
            e.location = location;
            e.open = open;
        });
    }

    /// What the daemon knows about this viewer.
    #[cfg(test)]
    pub fn entry(&self) -> ViewerEntry {
        self.inner.lock().unwrap().entry.clone()
    }

    /// This viewer opened a location, or reconnected with it open.
    pub fn send_location(&self, state: LocationState) {
        self.inner
            .lock()
            .unwrap()
            .send(Method::ViewerLocation(state));
    }

    /// This viewer changed a location's marks.
    pub fn send_op(&self, key: LocationKey, op: crate::marks::Op) {
        self.inner
            .lock()
            .unwrap()
            .send(Method::ViewerOp { key, op });
    }

    /// This viewer closed a location.
    pub fn send_close(&self, key: LocationKey) {
        self.inner.lock().unwrap().send(Method::ViewerClose { key });
    }

    /// Answer a show or query event the daemon relays, under its id.
    pub fn reply(&self, id: String, result: serde_json::Value) {
        self.inner
            .lock()
            .unwrap()
            .send(Method::ViewerReply { id, result });
    }

    pub fn set_window(&self, window: Option<wm::Window>, platform: &'static str) {
        self.update(|e| {
            e.platform = Some(platform.to_string());
            e.window = window.as_ref().map(|w| w.address.clone());
            e.workspace = window.as_ref().and_then(|w| w.workspace.clone());
            e.position = window.as_ref().and_then(|w| w.position);
        });
    }

    fn update(&self, f: impl FnOnce(&mut ViewerEntry)) {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.entry.clone();
        f(&mut inner.entry);
        if inner.entry != before {
            let entry = inner.entry.clone();
            inner.send(Method::ViewerRegister(entry));
        }
    }
}

impl Inner {
    fn send(&mut self, method: Method) {
        let Some(w) = self.writer.as_mut() else {
            return;
        };
        let line = to_line(&Request {
            id: "v".to_string(),
            method,
        });
        if w.write_all(line.as_bytes()).is_err() {
            self.writer = None;
        }
    }
}

/// Start the link thread and the placement tracker.
pub fn spawn(tx: Sender<Msg>, link: Link) {
    let tracker = link.clone();
    std::thread::spawn(move || {
        let Some(platform) = wm::detect() else { return };
        let name = platform.name();
        platform.track(std::process::id(), &mut |window| {
            tracker.set_window(window, name)
        });
    });

    std::thread::spawn(move || {
        let mut delay = FIRST_RETRY;
        loop {
            if session(&tx, &link).is_ok() {
                delay = FIRST_RETRY;
            }
            link.inner.lock().unwrap().writer = None;
            if tx.send(Msg::Link(false)).is_err() {
                return;
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(MAX_RETRY);
        }
    });
}

fn session(tx: &Sender<Msg>, link: &Link) -> Result<(), String> {
    let stream = proto::connect_or_spawn().ok_or("no daemon")?;
    let mut client = proto::Client::new(stream, "v").map_err(|e| e.to_string())?;
    client.hello()?;
    client.call(Method::Subscribe)?;
    let (writer, mut reader) = client.into_parts();
    let my_id = {
        let mut inner = link.inner.lock().unwrap();
        inner.writer = Some(writer);
        let entry = inner.entry.clone();
        inner.send(Method::ViewerRegister(entry));
        inner.entry.id.clone()
    };
    tx.send(Msg::Link(true)).map_err(|_| "ui gone")?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("daemon closed".to_string());
        }
        let msg = match ServerFrame::parse(&line) {
            Ok(ServerFrame::Event(Event::Show {
                viewer,
                target,
                view,
                id,
            })) if viewer == my_id => match view {
                Some(view) => Msg::View(target, Box::new(view), id),
                None => Msg::Show(target, id),
            },
            Ok(ServerFrame::Event(Event::Location { state, note })) => Msg::Location {
                state: Box::new(state),
                note,
            },
            _ => continue,
        };
        tx.send(msg).map_err(|_| "ui gone")?;
    }
}

/// Ask the daemon to display `dir`, preferring a viewer on our workspace.
pub fn request_show(
    client: &mut proto::Client,
    dir: PathBuf,
    view: Option<proto::ViewSpec>,
) -> Result<proto::ShowResult, String> {
    let workspace = wm::own_workspace();
    let v = client.call(Method::Show {
        dir,
        base: None,
        workspace,
        view,
    })?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Ask the daemon about marks; without a location named, it answers for
/// the one on screen in the viewer on our workspace.
pub fn request_query(
    client: &mut proto::Client,
    query: AgentQuery,
) -> Result<serde_json::Value, String> {
    let workspace = wm::own_workspace();
    let cwd = std::env::current_dir().ok();
    client.call(Method::Query {
        workspace,
        cwd,
        query,
    })
}

pub fn describe(r: &proto::ShowResult) -> String {
    let Some(ws) = r.workspace.as_deref() else {
        return format!("shown in viewer {}", r.viewer);
    };
    match r.picked_by.as_str() {
        "same" => format!("shown in viewer {} on your workspace {ws}", r.viewer),
        "nearest" => format!(
            "shown in viewer {} on workspace {ws}, the nearest to yours",
            r.viewer
        ),
        _ => format!("shown in viewer {} (workspace {ws})", r.viewer),
    }
}

fn one_shot() -> Result<proto::Client, String> {
    let stream = proto::connect_or_spawn().ok_or("could not reach or start the codesnob daemon")?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut client = proto::Client::new(stream, "c").map_err(|e| e.to_string())?;
    client.hello()?;
    Ok(client)
}

pub fn connect() -> Result<proto::Client, String> {
    one_shot()
}

/// `codesnob show PATH [--range R]`: display in the best viewer and report.
pub fn show(dir: PathBuf, range: Option<String>) -> Result<(), String> {
    let mut client = one_shot()?;
    let view = range.map(|r| proto::ViewSpec {
        range: Some(r),
        ..Default::default()
    });
    let r = request_show(&mut client, dir.clone(), view)?;
    if !r.repo {
        eprintln!("codesnob: {} is not inside a git repository", dir.display());
    }
    println!("{}", describe(&r));
    Ok(())
}

/// `codesnob review`: the marks on a location, as an agent sees them.
pub fn review(location: Option<LocationRef>, json: bool) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = request_query(&mut client, AgentQuery::Review { location })?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let r: proto::ReviewResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("{}", r.render());
    Ok(())
}

/// `codesnob clear`: remove marks on a location.
pub fn clear(
    location: Option<LocationRef>,
    which: String,
    letters: Vec<String>,
    user_asked: bool,
) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = request_query(
        &mut client,
        AgentQuery::ClearMarks {
            location,
            which,
            letters,
            user_asked,
        },
    )?;
    let r: proto::ClearResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("{}", r.render());
    Ok(())
}

/// Every location, with its marks and unread comments.
pub fn request_locations(client: &mut proto::Client) -> Result<String, String> {
    let v = request_query(client, AgentQuery::Locations)?;
    let r: proto::LocationsResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok(r.render())
}

/// `codesnob locations`: what the viewer has open.
pub fn locations(json: bool) -> Result<(), String> {
    let mut client = one_shot()?;
    if json {
        let v = request_query(&mut client, AgentQuery::Locations)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("{}", request_locations(&mut client)?);
    Ok(())
}

/// `codesnob mark`: a mark of the agent's with words on lines of a file.
pub fn mark(query: AgentQuery) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = request_query(&mut client, query)?;
    let r: proto::MarkResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("{}{}", r.render(), proto::mine_note(&r.location, &r.mine));
    Ok(())
}

/// `codesnob reply`: answer in the thread under a mark.
pub fn reply(location: Option<LocationRef>, letter: String, text: String) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = request_query(
        &mut client,
        AgentQuery::Reply {
            location,
            letter,
            text,
        },
    )?;
    let r: proto::ReplyResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("{}", r.render());
    Ok(())
}

/// `codesnob move`: put a mark on other lines.
pub fn move_mark(query: AgentQuery) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = request_query(&mut client, query)?;
    let r: proto::MoveResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("{}", r.render());
    Ok(())
}

/// `codesnob status`: what the daemon shows and who is watching.
pub fn status(json: bool) -> Result<(), String> {
    let mut client = one_shot()?;
    let v = client.call(Method::Status)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let st: proto::StatusResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
    match &st.target {
        Some(t) => println!(
            "target: {}{}{}",
            t.dir.display(),
            t.base
                .as_ref()
                .map(|b| format!(" (base {b})"))
                .unwrap_or_default(),
            if t.repo {
                ""
            } else {
                "  [not a git repository]"
            }
        ),
        None => println!("target: none"),
    }
    if st.viewers.is_empty() {
        println!("viewers: none");
    } else {
        println!("viewers:");
        for v in &st.viewers {
            println!(
                "  {:<10} pid {:<8} workspace {:<12} window {:<14} {}",
                v.id,
                v.pid,
                v.workspace.as_deref().unwrap_or("-"),
                v.window.as_deref().unwrap_or("-"),
                v.target
                    .as_ref()
                    .map(|t| t.dir.display().to_string())
                    .unwrap_or_default(),
            );
        }
    }
    if !st.locations.is_empty() {
        println!("locations:");
        for l in &st.locations {
            println!("  {}", l.render());
        }
    }
    Ok(())
}

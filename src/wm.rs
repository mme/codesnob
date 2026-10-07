//! Where is this viewer on the desktop? A small platform seam: `detect()`
//! returns a backend for the running compositor, or `None` on anything we
//! do not know, in which case placement stays unknown and everything else
//! works as usual. Hyprland is the only backend so far.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

/// A viewer's window as the compositor sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Compositor-native token, normalized (no `0x`, lowercase).
    pub address: String,
    /// Display name of the workspace holding the window.
    pub workspace: Option<String>,
    pub position: Option<Position>,
}

pub trait Platform: Send {
    fn name(&self) -> &'static str;
    /// Find the window owning `pid` or one of its ancestors.
    fn resolve(&self, pid: u32) -> Option<Window>;
    /// Find the window and then block, calling `on_change` with the initial
    /// placement and again whenever it moves or closes. `None` means the
    /// window is gone. Returns when tracking ends.
    fn track(&self, pid: u32, on_change: &mut dyn FnMut(Option<Window>));
}

/// Pick the backend for the current desktop, if any.
pub fn detect() -> Option<Box<dyn Platform>> {
    hyprland::Hyprland::detect().map(|h| Box::new(h) as Box<dyn Platform>)
}

/// The workspace holding this process's terminal window, if known.
pub fn own_workspace() -> Option<String> {
    detect()?.resolve(std::process::id())?.workspace
}

const RESOLVE_ATTEMPTS: u32 = 20;
const RESOLVE_INTERVAL: Duration = Duration::from_millis(250);

/// Resolve with retries: a viewer launched by `foot -e codesnob` runs
/// before its window is mapped.
pub fn resolve_when_mapped(platform: &dyn Platform, pid: u32) -> Option<Window> {
    for attempt in 0..RESOLVE_ATTEMPTS {
        if let Some(w) = platform.resolve(pid) {
            return Some(w);
        }
        if attempt + 1 < RESOLVE_ATTEMPTS {
            std::thread::sleep(RESOLVE_INTERVAL);
        }
    }
    None
}

/// Parent pids of `pid`, nearest first, including `pid` itself.
/// Linux only via /proc; elsewhere just `[pid]`.
pub fn ancestry(pid: u32) -> Vec<u32> {
    const MAX: usize = 32;
    let mut out = vec![pid];
    let mut cur = pid;
    while out.len() < MAX {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{cur}/stat")) else { break };
        let Some(ppid) = parse_ppid(&stat) else { break };
        if ppid <= 1 {
            break;
        }
        out.push(ppid);
        cur = ppid;
    }
    out
}

fn parse_ppid(stat: &str) -> Option<u32> {
    // Fields after the last ')' so command names with spaces or parens are safe.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

pub fn normalize(address: &str) -> String {
    address.trim().trim_start_matches("0x").to_ascii_lowercase()
}

pub mod hyprland {
    use super::*;

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_REPLY: usize = 8 * 1024 * 1024;
    const MAX_EVENT_LINE: usize = 64 * 1024;

    pub struct Hyprland {
        dir: PathBuf,
    }

    impl Hyprland {
        pub fn detect() -> Option<Self> {
            let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty())?;
            let mut candidates = Vec::new();
            if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
                candidates.push(PathBuf::from(rt).join("hypr").join(&sig));
            }
            candidates.push(PathBuf::from("/tmp/hypr").join(&sig));
            let dir = candidates.into_iter().find(|d| d.join(".socket.sock").exists())?;
            Some(Self { dir })
        }

        fn request(&self, cmd: &str) -> Option<Vec<u8>> {
            let mut s = UnixStream::connect(self.dir.join(".socket.sock")).ok()?;
            s.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
            s.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
            s.write_all(cmd.as_bytes()).ok()?;
            let mut buf = Vec::new();
            s.take(MAX_REPLY as u64).read_to_end(&mut buf).ok()?;
            Some(buf)
        }

        fn clients(&self) -> Option<Vec<Client>> {
            let raw = self.request("j/clients")?;
            serde_json::from_slice(&raw).ok()
        }

        fn locate(&self, address: &str) -> Option<Window> {
            self.clients()?.into_iter().find(|c| normalize(&c.address) == address).map(Client::into_window)
        }

        fn connect_events(&self) -> std::io::Result<UnixStream> {
            UnixStream::connect(self.dir.join(".socket2.sock"))
        }
    }

    #[derive(Deserialize)]
    struct Client {
        address: String,
        pid: i64,
        #[serde(default)]
        workspace: Option<ClientWorkspace>,
        #[serde(default)]
        at: Option<Vec<i32>>,
    }

    #[derive(Deserialize)]
    struct ClientWorkspace {
        #[serde(default)]
        name: Option<String>,
    }

    impl Client {
        fn into_window(self) -> Window {
            Window {
                address: normalize(&self.address),
                workspace: self.workspace.and_then(|w| w.name),
                position: self.at.filter(|a| a.len() >= 2).map(|a| Position { x: a[0], y: a[1] }),
            }
        }
    }

    /// Nearest ancestor owning exactly one window wins. A process owning
    /// several windows is ambiguous, so it yields `None` rather than a guess.
    fn window_for_ancestry(clients: Vec<Client>, ancestry: &[u32]) -> Option<Window> {
        let mut by_pid: Vec<(u32, Client)> = clients
            .into_iter()
            .filter_map(|c| u32::try_from(c.pid).ok().map(|p| (p, c)))
            .collect();
        for &pid in ancestry {
            let n = by_pid.iter().filter(|(p, _)| *p == pid).count();
            match n {
                0 => continue,
                1 => {
                    let idx = by_pid.iter().position(|(p, _)| *p == pid)?;
                    return Some(by_pid.swap_remove(idx).1.into_window());
                }
                _ => return None,
            }
        }
        None
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Event {
        Moved { address: String, workspace: String },
        Closed { address: String },
        /// Layout may have changed; re-check our placement.
        Relaid,
    }

    pub fn parse_event(line: &str) -> Option<Event> {
        let (name, data) = line.split_once(">>")?;
        match name {
            "movewindow" => {
                let (addr, ws) = data.split_once(',')?;
                Some(Event::Moved { address: normalize(addr), workspace: ws.to_string() })
            }
            "movewindowv2" => {
                let (addr, rest) = data.split_once(',')?;
                let (_id, name) = rest.split_once(',')?;
                Some(Event::Moved { address: normalize(addr), workspace: name.to_string() })
            }
            "closewindow" => Some(Event::Closed { address: normalize(data) }),
            "openwindow" | "changefloatingmode" | "fullscreen" | "pin" | "swapwindow" | "moveworkspace"
            | "moveworkspacev2" | "monitoradded" | "monitorremoved" | "configreloaded" => Some(Event::Relaid),
            _ => None,
        }
    }

    impl Platform for Hyprland {
        fn name(&self) -> &'static str {
            "hyprland"
        }

        fn resolve(&self, pid: u32) -> Option<Window> {
            let clients = self.clients()?;
            window_for_ancestry(clients, &ancestry(pid))
        }

        fn track(&self, pid: u32, on_change: &mut dyn FnMut(Option<Window>)) {
            // Subscribe before resolving so a move during the lookup is queued, not lost.
            let Ok(events) = self.connect_events() else { return };
            let Some(mut window) = resolve_when_mapped(self, pid) else { return };
            on_change(Some(window.clone()));
            let mut reader = BufReader::new(events).take(u64::MAX);
            let mut line = String::new();
            loop {
                line.clear();
                let n = match (&mut reader).take(MAX_EVENT_LINE as u64).read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                if n >= MAX_EVENT_LINE {
                    continue;
                }
                match parse_event(line.trim_end()) {
                    Some(Event::Moved { address, workspace }) if address == window.address => {
                        let position = self.locate(&window.address).and_then(|w| w.position);
                        if window.workspace.as_deref() == Some(workspace.as_str()) && window.position == position {
                            continue;
                        }
                        window.workspace = Some(workspace);
                        window.position = position;
                        on_change(Some(window.clone()));
                    }
                    Some(Event::Closed { address }) if address == window.address => {
                        // A terminal may outlive one of its windows.
                        match self.resolve(pid) {
                            Some(found) => {
                                window = found;
                                on_change(Some(window.clone()));
                            }
                            None => {
                                on_change(None);
                                return;
                            }
                        }
                    }
                    Some(_) => {
                        let Some(placed) = self.locate(&window.address) else { continue };
                        if placed == window {
                            continue;
                        }
                        window = placed;
                        on_change(Some(window.clone()));
                    }
                    None => {}
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_events() {
            assert_eq!(
                parse_event("movewindow>>0xABC,dev"),
                Some(Event::Moved { address: "abc".into(), workspace: "dev".into() })
            );
            assert_eq!(
                parse_event("movewindowv2>>abc,3,name,with,commas"),
                Some(Event::Moved { address: "abc".into(), workspace: "name,with,commas".into() })
            );
            assert_eq!(parse_event("closewindow>>ABC"), Some(Event::Closed { address: "abc".into() }));
            assert_eq!(parse_event("openwindow>>abc,1,foot,foot"), Some(Event::Relaid));
            assert_eq!(parse_event("workspace>>2"), None);
            assert_eq!(parse_event("activewindowv2>>abc"), None);
            assert_eq!(parse_event("garbage"), None);
        }

        #[test]
        fn ppid_survives_parens_in_comm() {
            assert_eq!(parse_ppid("12 (my (odd) name) S 7 12 12 0 -1"), Some(7));
        }
    }
}

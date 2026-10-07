//! Background refresh: watch the worktree and git directory with inotify,
//! debounce, re-snapshot off the UI thread, and fall back to polling when
//! no watcher can be set up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::git;

pub enum Msg {
    Input(crossterm::event::Event),
    /// A snapshot from the refresher of generation `generation`; older generations are ignored.
    Refresh { generation: u64, result: Result<git::Snapshot, String> },
    /// The daemon asks viewers to display this target.
    Show(crate::proto::Target),
    /// Daemon link went up or down.
    Link(bool),
}

const DEBOUNCE: Duration = Duration::from_millis(150);
const POLL: Duration = Duration::from_secs(2);

/// How refreshes are triggered, for display.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Watching,
    Polling,
    Off,
}

/// Forward terminal input into the shared channel.
pub fn spawn_input(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break;
            }
        }
    });
}

/// Start the refresh thread. Returns a sender to request a refresh by hand.
pub fn spawn_refresher(dir: PathBuf, base: Option<String>, tx: Sender<Msg>, generation: u64) -> (Sender<()>, Mode) {
    let (poke_tx, poke_rx) = channel::<()>();
    let watcher = setup_watcher(&dir, poke_tx.clone());
    let mode = if watcher.is_some() { Mode::Watching } else { Mode::Polling };
    std::thread::spawn(move || refresh_loop(dir, base, tx, poke_rx, watcher, generation));
    (poke_tx, mode)
}

struct Watch {
    watcher: notify::RecommendedWatcher,
    dirs: HashSet<PathBuf>,
}

fn setup_watcher(dir: &Path, poke: Sender<()>) -> Option<Watch> {
    let repo = gix::discover(dir).ok()?;
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                poke.send(()).ok();
            }
        }
    })
    .ok()?;

    // HEAD, index and ORIG_HEAD live in the git dir; refs may live in the common dir.
    watcher.watch(repo.git_dir(), RecursiveMode::NonRecursive).ok()?;
    if repo.common_dir() != repo.git_dir() {
        watcher.watch(repo.common_dir(), RecursiveMode::NonRecursive).ok();
    }
    watcher.watch(&repo.common_dir().join("refs"), RecursiveMode::Recursive).ok();

    let mut w = Watch { watcher, dirs: HashSet::new() };
    w.add_worktree_dirs(&repo);
    Some(w)
}

impl Watch {
    /// Watch every non-ignored directory of the worktree that is not yet watched.
    fn add_worktree_dirs(&mut self, repo: &gix::Repository) {
        let Some(root) = repo.workdir() else { return };
        let Ok(worktree) = repo.worktree().ok_or(()) else { return };
        let Ok(mut excludes) = worktree.excludes(None) else { return };
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            if !self.dirs.contains(&d) {
                if self.watcher.watch(&d, RecursiveMode::NonRecursive).is_err() {
                    continue;
                }
                self.dirs.insert(d.clone());
            }
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_dir() {
                    continue;
                }
                let path = entry.path();
                if entry.file_name() == ".git" || path.join(".git").exists() {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(root) else { continue };
                let excluded = excludes
                    .at_path(rel, Some(gix::index::entry::Mode::DIR))
                    .map(|p| p.is_excluded())
                    .unwrap_or(true);
                if !excluded {
                    stack.push(path);
                }
            }
        }
    }
}

fn refresh_loop(
    dir: PathBuf,
    base: Option<String>,
    tx: Sender<Msg>,
    poke: Receiver<()>,
    mut watch: Option<Watch>,
    generation: u64,
) {
    loop {
        match watch {
            Some(_) => {
                if poke.recv().is_err() {
                    return;
                }
            }
            None => match poke.recv_timeout(POLL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            },
        }
        // Collapse bursts of events into one refresh.
        while poke.recv_timeout(DEBOUNCE).is_ok() {}

        let snap = git::snapshot(&dir, base.as_deref()).map_err(|e| format!("{e:#}"));
        if let Some(w) = watch.as_mut() {
            if let Ok(repo) = gix::discover(&dir) {
                w.add_worktree_dirs(&repo);
            }
        }
        if tx.send(Msg::Refresh { generation, result: snap }).is_err() {
            return;
        }
    }
}

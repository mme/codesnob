//! Change detection: watch the worktree and git directory (inotify on Linux,
//! FSEvents on macOS), debounce, and tell the UI something changed. Falls back
//! to polling when no watcher can be set up. Loading happens in `loader`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::git;

pub enum Msg {
    Input(crossterm::event::Event),
    /// Files changed under location `loc`.
    Changed {
        loc: u64,
    },
    /// A load finished. Only the latest `seq` of a location is applied.
    Loaded {
        loc: u64,
        seq: u64,
        result: Box<Result<git::Snapshot, String>>,
    },
    /// The daemon asks this viewer to display a target.
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

/// A running watcher. Dropping it stops the thread and its OS watches.
pub struct Handle {
    stop: Arc<AtomicBool>,
    poke: Sender<()>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the thread so it sees the flag now rather than at the next event.
        let _ = self.poke.send(());
    }
}

/// Watch `dir`'s repository and send `Msg::Changed { loc }` after each burst
/// of changes.
pub fn spawn_watcher(dir: PathBuf, tx: Sender<Msg>, loc: u64) -> (Handle, Mode) {
    let (poke_tx, poke_rx) = channel::<()>();
    let watcher = setup_watcher(&dir, poke_tx.clone());
    let mode = if watcher.is_some() {
        Mode::Watching
    } else {
        Mode::Polling
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || watch_loop(dir, tx, poke_rx, watcher, loc, flag));
    (
        Handle {
            stop,
            poke: poke_tx,
        },
        mode,
    )
}

/// macOS's FSEvents watches whole trees cheaply but re-creates its stream on
/// every added path, so there one recursive watch covers the worktree and
/// events from ignored directories are filtered out instead. inotify is the
/// opposite: one watch per directory, so ignored trees are never watched.
const RECURSIVE_ROOT: bool = cfg!(target_os = "macos");

struct Watch {
    watcher: notify::RecommendedWatcher,
    /// Every non-ignored directory of the worktree; shared with the event
    /// callback, which drops events from anywhere else.
    dirs: Arc<Mutex<HashSet<PathBuf>>>,
}

fn setup_watcher(dir: &Path, poke: Sender<()>) -> Option<Watch> {
    let repo = gix::discover(dir).ok()?;
    let dirs: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let git_dirs = vec![
        repo.git_dir().to_path_buf(),
        repo.common_dir().to_path_buf(),
    ];
    let known = Arc::clone(&dirs);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        if ev.paths.is_empty() || ev.paths.iter().any(|p| relevant(p, &git_dirs, &known)) {
            poke.send(()).ok();
        }
    })
    .ok()?;

    // HEAD, index and ORIG_HEAD live in the git dir; refs may live in the common dir.
    watcher
        .watch(repo.git_dir(), RecursiveMode::NonRecursive)
        .ok()?;
    if repo.common_dir() != repo.git_dir() {
        watcher
            .watch(repo.common_dir(), RecursiveMode::NonRecursive)
            .ok();
    }
    watcher
        .watch(&repo.common_dir().join("refs"), RecursiveMode::Recursive)
        .ok();
    if RECURSIVE_ROOT {
        watcher
            .watch(repo.workdir()?, RecursiveMode::Recursive)
            .ok()?;
    }

    let mut w = Watch { watcher, dirs };
    w.add_worktree_dirs(&repo);
    Some(w)
}

/// An event matters when it is in the git dir or in a non-ignored directory.
fn relevant(path: &Path, git_dirs: &[PathBuf], dirs: &Mutex<HashSet<PathBuf>>) -> bool {
    if git_dirs.iter().any(|g| path.starts_with(g)) {
        return true;
    }
    let dirs = dirs.lock().unwrap();
    dirs.contains(path) || path.parent().is_some_and(|parent| dirs.contains(parent))
}

impl Watch {
    /// Record, and on Linux watch, every non-ignored directory not yet known.
    fn add_worktree_dirs(&mut self, repo: &gix::Repository) {
        let Some(root) = repo.workdir() else { return };
        let Some(worktree) = repo.worktree() else {
            return;
        };
        let Ok(mut excludes) = worktree.excludes(None) else {
            return;
        };
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let known = self.dirs.lock().unwrap().contains(&d);
            if !known {
                if !RECURSIVE_ROOT && self.watcher.watch(&d, RecursiveMode::NonRecursive).is_err() {
                    continue;
                }
                self.dirs.lock().unwrap().insert(d.clone());
            }
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_dir() {
                    continue;
                }
                let path = entry.path();
                if entry.file_name() == ".git" || path.join(".git").exists() {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
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

fn watch_loop(
    dir: PathBuf,
    tx: Sender<Msg>,
    poke: Receiver<()>,
    mut watch: Option<Watch>,
    loc: u64,
    stop: Arc<AtomicBool>,
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
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Collapse bursts of events into one notification.
        while poke.recv_timeout(DEBOUNCE).is_ok() {}
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Directories created since the last burst get watched too.
        if let Some(w) = watch.as_mut()
            && let Ok(repo) = gix::discover(&dir)
        {
            w.add_worktree_dirs(&repo);
        }
        if tx.send(Msg::Changed { loc }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git runs")
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repo with one tracked file and an ignored directory, canonicalized
    /// the way main() canonicalizes (FSEvents reports real paths).
    fn repo(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codesnob-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// Whether a change notification arrives within `wait`.
    fn changed(rx: &Receiver<Msg>, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Msg::Changed { loc: 7 }) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    }

    /// Wait out the tail of a burst.
    fn quiet(rx: &Receiver<Msg>) {
        while changed(rx, Duration::from_millis(800)) {}
    }

    #[test]
    fn an_edit_notifies_and_an_ignored_write_does_not() {
        let dir = repo("edit");
        let (tx, rx) = channel();
        let (_handle, mode) = spawn_watcher(dir.clone(), tx, 7);
        assert!(mode == Mode::Watching, "a native watcher is available");
        // Let the watcher settle before the first change.
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        assert!(changed(&rx, Duration::from_secs(10)), "an edit notifies");

        quiet(&rx);
        std::fs::write(dir.join("ignored/build.log"), "noise\n").unwrap();
        assert!(
            !changed(&rx, Duration::from_millis(1500)),
            "writes in ignored directories do not notify"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_directory_is_picked_up() {
        let dir = repo("newdir");
        let (tx, rx) = channel();
        let (_handle, _) = spawn_watcher(dir.clone(), tx, 7);
        std::thread::sleep(Duration::from_millis(300));

        std::fs::create_dir(dir.join("src")).unwrap();
        assert!(
            changed(&rx, Duration::from_secs(10)),
            "creating a directory notifies"
        );
        quiet(&rx);

        std::fs::write(dir.join("src/lib.rs"), "fn main() {}\n").unwrap();
        assert!(
            changed(&rx, Duration::from_secs(10)),
            "a file in the new directory notifies"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_the_handle_stops_notifications() {
        let dir = repo("drop");
        let (tx, rx) = channel();
        let (handle, _) = spawn_watcher(dir.clone(), tx, 7);
        std::thread::sleep(Duration::from_millis(300));
        drop(handle);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join("tracked.txt"), "after\n").unwrap();
        assert!(
            !changed(&rx, Duration::from_millis(1500)),
            "a stopped watcher is silent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Background refresh: watch the worktree and git directory with inotify,
//! debounce, re-snapshot off the UI thread, and fall back to polling when
//! no watcher can be set up.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::git;

pub enum Msg {
    Input(crossterm::event::Event),
    /// A snapshot from the refresher of generation `generation`; older generations are ignored.
    Refresh {
        generation: u64,
        result: Result<git::Snapshot, String>,
    },
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
pub fn spawn_refresher(
    dir: PathBuf,
    base: Option<String>,
    tx: Sender<Msg>,
    generation: u64,
) -> (Sender<()>, Mode) {
    let (poke_tx, poke_rx) = channel::<()>();
    let watcher = setup_watcher(&dir, poke_tx.clone());
    let mode = if watcher.is_some() {
        Mode::Watching
    } else {
        Mode::Polling
    };
    std::thread::spawn(move || refresh_loop(dir, base, tx, poke_rx, watcher, generation));
    (poke_tx, mode)
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
        if let Some(w) = watch.as_mut()
            && let Ok(repo) = gix::discover(&dir)
        {
            w.add_worktree_dirs(&repo);
        }
        if tx
            .send(Msg::Refresh {
                generation,
                result: snap,
            })
            .is_err()
        {
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

    /// The next refresh within `wait`, skipping input and link messages.
    fn next_refresh(rx: &Receiver<Msg>, wait: Duration) -> Option<git::Snapshot> {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Msg::Refresh { result, .. }) => return Some(result.expect("snapshot")),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    #[test]
    fn an_edit_refreshes_and_an_ignored_write_does_not() {
        let dir = repo("edit");
        let (tx, rx) = channel();
        let (_poke, mode) = spawn_refresher(dir.clone(), None, tx, 1);
        assert!(mode == Mode::Watching, "a native watcher is available");
        // Let the watcher settle before the first change.
        std::thread::sleep(Duration::from_millis(300));

        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        let snap = next_refresh(&rx, Duration::from_secs(10)).expect("an edit triggers a refresh");
        assert_eq!(snap.files.len(), 1, "the edited file is the diff");

        // Drain the tail of that burst, then touch only ignored paths.
        while next_refresh(&rx, Duration::from_millis(800)).is_some() {}
        std::fs::write(dir.join("ignored/build.log"), "noise\n").unwrap();
        assert!(
            next_refresh(&rx, Duration::from_millis(1500)).is_none(),
            "writes in ignored directories do not refresh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_directory_is_picked_up() {
        let dir = repo("newdir");
        let (tx, rx) = channel();
        let (_poke, _) = spawn_refresher(dir.clone(), None, tx, 1);
        std::thread::sleep(Duration::from_millis(300));

        std::fs::create_dir(dir.join("src")).unwrap();
        next_refresh(&rx, Duration::from_secs(10)).expect("creating a directory refreshes");
        while next_refresh(&rx, Duration::from_millis(800)).is_some() {}

        std::fs::write(dir.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let snap = next_refresh(&rx, Duration::from_secs(10))
            .expect("a file in the new directory refreshes");
        assert!(
            snap.files
                .iter()
                .any(|f| f.new_path.as_deref() == Some("src/lib.rs"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

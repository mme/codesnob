//! The viewer driven headless: real repositories, real key presses, and the
//! rendered screen read back as text.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::testutil::{branch_with_history, fresh_dir};

struct Harness {
    app: App,
    rx: Receiver<Msg>,
    term: Terminal<TestBackend>,
    /// Scratch directories this harness owns; removed when it is dropped,
    /// even when a test fails, so runs do not pile up in the temp dir.
    scratch: Vec<PathBuf>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for d in &self.scratch {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

impl Harness {
    fn open(dir: &Path) -> Self {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let state = fresh_dir(&format!("state-{name}"));
        let mut h = Self::open_with_state(dir, state.clone());
        h.scratch.push(state);
        h
    }

    /// `state` belongs to the caller, which may reuse it across harnesses.
    fn open_with_state(dir: &Path, state: PathBuf) -> Self {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let cache = fresh_dir(&format!("cache-{name}"));
        let mut h = Self::build(dir, state, cache.clone(), std::sync::Arc::new(NoPrs));
        h.scratch.push(cache);
        h
    }

    fn build(
        dir: &Path,
        state: PathBuf,
        cache: PathBuf,
        source: std::sync::Arc<dyn crate::pr::Source>,
    ) -> Self {
        let (tx, rx) = channel();
        let hl = Highlighter::new("base16").unwrap();
        let opts = Options {
            context: 3,
            state_dir: Some(state),
            cache_dir: Some(cache),
            pr_source: source,
        };
        let mut app = App::new(opts, Palette::dark(), hl, tx, Link::new());
        // Loads between key presses must not outlast the strip on slow machines.
        app.flip_window = Duration::from_secs(60);
        app.open(Target {
            dir: dir.to_path_buf(),
            base: None,
            repo: gix::discover(dir).is_ok(),
        });
        let mut h = Self {
            app,
            rx,
            term: Terminal::new(TestBackend::new(100, 30)).unwrap(),
            scratch: Vec::new(),
        };
        h.settle();
        h
    }

    /// Apply messages until no load is in flight.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.app.loading() && Instant::now() < deadline {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.app.dispatch(m);
            }
        }
        assert!(!self.app.loading(), "load finished in time");
    }

    /// Press keys: plain characters, or `<esc>`, `<enter>`, `<down>`, `<up>`,
    /// `<c-k>` style names.
    fn keys(&mut self, keys: &str) {
        let mut rest = keys;
        while !rest.is_empty() {
            let (key, len) = if let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
                let name = &rest[1..=end];
                let key = match name {
                    "esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                    "enter" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    "down" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    "up" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                    "bs" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                    "a-up" => KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
                    "a-down" => KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
                    "a-left" => KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
                    "a-right" => KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
                    c if c.starts_with("c-") => KeyEvent::new(
                        KeyCode::Char(c.chars().nth(2).unwrap()),
                        KeyModifiers::CONTROL,
                    ),
                    other => panic!("unknown key <{other}>"),
                };
                (key, end + 2)
            } else {
                let c = rest.chars().next().unwrap();
                (
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                    c.len_utf8(),
                )
            };
            self.app.dispatch(Msg::Input(Event::Key(key)));
            rest = &rest[len..];
        }
        self.settle();
    }

    fn screen(&mut self) -> String {
        self.term.draw(|f| self.app.draw(f)).unwrap();
        let buf = self.term.backend().buffer();
        let w = buf.area.width as usize;
        buf.content
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn status(&mut self) -> String {
        self.screen()
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

/// No pull requests outside the PR tests.
struct NoPrs;

impl crate::pr::Source for NoPrs {
    fn view(&self, spec: &crate::pr::Spec, _: Option<&Path>) -> Result<crate::pr::Info, String> {
        Err(format!("no pull request #{}", spec.number))
    }
}

fn cleanup(dir: PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_status_bar_says_what_is_shown_in_words() {
    let dir = branch_with_history("ui-status");
    let mut h = Harness::open(&dir);
    let status = h.status();
    assert!(
        status.starts_with("feature · 2 commits and uncommitted work since main"),
        "{status}"
    );
    assert!(status.contains("5 files"), "{status}");
    assert!(!status.contains("found via"), "{status}");
    cleanup(dir);
}

#[test]
fn the_palette_lists_commands_with_their_keys_and_runs_them() {
    let dir = branch_with_history("ui-palette");
    let mut h = Harness::open(&dir);

    h.keys(":");
    let screen = h.screen();
    assert!(screen.contains("Commands ›"), "{screen}");
    let commits = screen.lines().find(|l| l.contains("Commits…")).unwrap();
    assert!(
        commits.trim_end().ends_with("Commits… [c]"),
        "the shortcut follows the name: {commits}"
    );
    assert!(
        !screen.contains("Everything since branch point"),
        "commands that do not apply are hidden"
    );

    h.keys("next h");
    let screen = h.screen();
    let first = screen.lines().find(|l| l.starts_with("› ")).unwrap();
    assert!(first.trim_end().ends_with("Next hunk [n]"), "{first}");
    h.keys("<esc>:uncom<enter>");
    assert!(h.status().starts_with("feature · uncommitted changes"));
    assert!(h.status().contains("2 files"), "{}", h.status());

    h.keys("<esc>");
    assert!(
        h.status()
            .starts_with("feature · 2 commits and uncommitted work since main"),
        "Esc returns to everything: {}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn recently_used_commands_come_first() {
    let dir = branch_with_history("ui-recent");
    let mut h = Harness::open(&dir);
    h.keys(":reload<enter>");
    h.keys(":");
    let screen = h.screen();
    let first = screen
        .lines()
        .find(|l| l.starts_with("› "))
        .expect("a selected row");
    assert!(first.contains("Reload"), "{screen}");
    cleanup(dir);
}

#[test]
fn esc_never_quits_and_q_does() {
    let dir = branch_with_history("ui-quit");
    let mut h = Harness::open(&dir);
    h.keys("<esc><esc>");
    assert!(!h.app.quit, "Esc backs out, it does not quit");
    h.keys("<c-k>");
    assert!(
        h.screen().contains("Commands ›"),
        "Ctrl-K opens the palette"
    );
    h.keys("<esc>");
    assert!(!h.screen().contains("Commands ›"), "Esc closes the palette");
    assert!(!h.app.quit);
    h.keys("q");
    assert!(h.app.quit);
    cleanup(dir);
}

#[test]
fn a_directory_outside_any_repository_says_so() {
    let dir = fresh_dir("ui-norepo");
    let mut h = Harness::open(&dir);
    let screen = h.screen();
    assert!(screen.contains("not inside a git repository"), "{screen}");
    assert!(h.status().starts_with("no repository ·"));
    cleanup(dir);
}

#[test]
fn edits_show_up_without_moving_the_view() {
    let dir = branch_with_history("ui-live");
    let mut h = Harness::open(&dir);
    h.keys("NN");
    let before = h.status();
    std::fs::write(dir.join("zzz.txt"), "late\n").unwrap();
    // The watcher notices, the loader reloads, the view stays on its file.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !h.status().contains("6 files") && Instant::now() < deadline {
        if let Ok(m) = h.rx.recv_timeout(Duration::from_millis(50)) {
            h.app.dispatch(m);
        }
    }
    let after = h.status();
    assert!(after.contains("6 files"), "{after}");
    assert!(
        before.contains("3/5") && after.contains("3/6"),
        "{before} / {after}"
    );
    cleanup(dir);
}

#[test]
fn the_file_list_jumps_in_diff_order_with_live_preview() {
    let dir = branch_with_history("ui-files");
    let mut h = Harness::open(&dir);
    assert!(h.status().contains("1/5"), "{}", h.status());

    h.keys("f");
    let screen = h.screen();
    assert!(screen.contains("Files ›"), "{screen}");
    // Panel rows: a selection marker, then a status letter.
    let rows: Vec<&str> = screen
        .lines()
        .filter(|l| {
            let c: Vec<char> = l.chars().take(4).collect();
            c.len() == 4 && "MADR".contains(c[2]) && c[3] == ' '
        })
        .collect();
    assert!(
        rows[0].contains("M a.txt") && rows[0].contains("+1 −1"),
        "{rows:?}"
    );
    assert!(
        rows[4].contains("A src/deep/one.rs"),
        "same order as the diff: {rows:?}"
    );

    h.keys("<down><down>");
    assert!(
        h.status().contains("3/5"),
        "moving the selection previews: {}",
        h.status()
    );
    h.keys("<esc>");
    assert!(
        h.status().contains("1/5"),
        "Esc returns to where you were: {}",
        h.status()
    );

    h.keys("fone<enter>");
    assert!(
        h.status().contains("5/5"),
        "typing filters, Enter jumps: {}",
        h.status()
    );
    assert!(!h.status().contains("only"), "jumping does not narrow");
    cleanup(dir);
}

#[test]
fn z_narrows_to_one_file_and_esc_brings_everything_back() {
    let dir = branch_with_history("ui-narrow");
    let mut h = Harness::open(&dir);
    h.keys("N");
    h.keys("z");
    let screen = h.screen();
    assert!(
        h.status().contains("b.txt only · Esc for all"),
        "{}",
        h.status()
    );
    assert!(h.status().contains("file 2 of 5"), "{}", h.status());
    let headers = screen
        .lines()
        .filter(|l| l.starts_with(" M ") || l.starts_with(" A ") || l.starts_with(" D "))
        .count();
    assert_eq!(headers, 1, "only one file is shown: {screen}");

    h.keys("N");
    assert!(
        h.status().contains("gone.txt only"),
        "next file moves the narrowing: {}",
        h.status()
    );
    h.keys("f<down><enter>");
    assert!(
        h.status().contains("new.txt only"),
        "the file list switches the narrowed file: {}",
        h.status()
    );

    h.keys("<esc>");
    assert!(!h.status().contains("only"), "{}", h.status());
    assert!(
        h.status().contains("4/5"),
        "back at the same file: {}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn narrowing_survives_a_reload_until_the_file_has_no_changes() {
    let dir = branch_with_history("ui-narrow-live");
    let mut h = Harness::open(&dir);
    h.keys("Nz");
    assert!(h.status().contains("b.txt only"), "{}", h.status());

    let wait_for = |h: &mut Harness, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !h.status().contains(what) && Instant::now() < deadline {
            if let Ok(m) = h.rx.recv_timeout(Duration::from_millis(50)) {
                h.app.dispatch(m);
            }
        }
        h.status()
    };
    std::fs::write(dir.join("zzz.txt"), "late\n").unwrap();
    let status = wait_for(&mut h, "of 6");
    assert!(
        status.contains("b.txt only") && status.contains("file 2 of 6"),
        "{status}"
    );

    std::fs::write(dir.join("b.txt"), "b.txt\n").unwrap();
    let status = wait_for(&mut h, "showing all files");
    assert!(
        status.contains("b.txt has no changes here; showing all files"),
        "{status}"
    );
    cleanup(dir);
}

#[test]
fn the_commit_list_previews_each_commit_and_esc_reverts() {
    let dir = branch_with_history("ui-commits");
    let mut h = Harness::open(&dir);
    h.keys("c");
    let screen = h.screen();
    assert!(screen.contains("Commits ›"), "{screen}");
    let rows: Vec<&str> = screen
        .lines()
        .skip_while(|l| !l.contains("Commits ›"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(
        rows[0].contains("Everything since main") && rows[0].contains("2 commits"),
        "{rows:?}"
    );
    assert!(
        rows[1].ends_with("just now") && rows[1].contains(" one "),
        "oldest first: {rows:?}"
    );
    assert!(rows[2].contains(" two "), "{rows:?}");
    assert!(
        rows[3].contains("Uncommitted changes"),
        "dirty state last: {rows:?}"
    );
    assert!(rows[0].starts_with("› "), "starts on the current view");

    h.keys("<down>");
    let status = h.status();
    assert!(
        status.contains("commit 1 of 2 · "),
        "moving previews: {status}"
    );
    assert!(h.screen().contains("src/deep/one.rs"));

    h.keys("<esc>");
    assert!(
        h.status().contains("2 commits and uncommitted work"),
        "Esc reverts: {}",
        h.status()
    );

    h.keys("c<down><down><enter>");
    assert!(
        h.status().contains("commit 2 of 2 · "),
        "Enter keeps: {}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn a_commit_shows_its_message_above_the_diff_and_d_hides_it() {
    let dir = branch_with_history("ui-desc");
    let mut h = Harness::open(&dir);
    h.keys("<a-down>");
    let screen = h.screen();
    let top: Vec<&str> = screen.lines().take(4).collect();
    assert!(
        top[0].starts_with("  ") && top[0].contains("· t ·") && !top[0].contains('│'),
        "header with author, no ruler: {top:?}"
    );
    assert!(top[1].contains("one"), "subject: {top:?}");
    assert!(top[3].contains("Adds the first function."), "body: {top:?}");

    h.keys("d");
    let screen = h.screen();
    assert!(!screen.contains("Adds the first function."), "{screen}");
    assert!(
        screen.lines().next().unwrap().contains("src/deep/one.rs"),
        "{screen}"
    );
    assert!(
        h.status().contains("commit 1 of 2 · "),
        "the subject stays in the status bar"
    );

    h.keys("<a-down>");
    assert!(
        !h.screen().contains("· t ·"),
        "hidden stays hidden for the session"
    );
    h.keys("d");
    assert!(
        h.screen().lines().next().unwrap().contains("· t ·"),
        "and comes back"
    );
    cleanup(dir);
}

#[test]
fn an_amended_commit_falls_back_to_everything() {
    let dir = branch_with_history("ui-amend");
    let mut h = Harness::open(&dir);
    h.keys("<a-up><a-up>");
    assert!(h.status().contains("commit 2 of 2 · "), "{}", h.status());
    crate::testutil::git(&dir, &["commit", "-q", "--amend", "-m", "two, amended"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !h.status().contains("no longer on this branch") && Instant::now() < deadline {
        if let Ok(m) = h.rx.recv_timeout(Duration::from_millis(50)) {
            h.app.dispatch(m);
        }
    }
    assert!(
        h.status().contains("is no longer on this branch"),
        "{}",
        h.status()
    );
    h.settle();
    h.keys("j");
    assert!(
        h.status()
            .contains("2 commits and uncommitted work since main"),
        "{}",
        h.status()
    );
    cleanup(dir);
}

/// A repository on main with one uncommitted file.
fn small_repo(name: &str) -> PathBuf {
    let dir = fresh_dir(name);
    crate::testutil::git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("readme.md"), "hello\n").unwrap();
    crate::testutil::git(&dir, &["add", "-A"]);
    crate::testutil::git(&dir, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.join("readme.md"), "hello again\n").unwrap();
    dir
}

#[test]
fn open_a_typed_path_and_flip_back_with_dash() {
    let a = branch_with_history("ui-loc-a");
    let b = small_repo("ui-loc-b");
    let mut h = Harness::open(&a);
    h.keys("<a-up><a-up>");
    assert!(h.status().contains("commit 2 of 2 · "), "{}", h.status());

    h.keys(&format!("o{}<enter>", b.display()));
    assert!(
        h.status().starts_with("main · uncommitted changes"),
        "{}",
        h.status()
    );

    h.keys("-");
    assert!(
        h.status().contains("ui-loc-a"),
        "the strip names the locations: {}",
        h.status()
    );
    h.keys("j");
    assert!(
        h.status().contains("commit 2 of 2 · "),
        "a location keeps its range: {}",
        h.status()
    );
    h.keys("-");
    h.keys("j");
    assert!(
        h.status().starts_with("main · uncommitted changes"),
        "{}",
        h.status()
    );
    cleanup(a);
    cleanup(b);
}

#[test]
fn quick_repeated_dashes_go_further_back() {
    let a = small_repo("ui-flip-a");
    let b = small_repo("ui-flip-b");
    let c = small_repo("ui-flip-c");
    let mut h = Harness::open(&a);
    h.keys(&format!("o{}<enter>", b.display()));
    h.keys(&format!("o{}<enter>", c.display()));
    h.keys("--");
    let status = h.status();
    assert!(
        status.contains("ui-flip-c")
            && status.contains("ui-flip-b")
            && status.contains("ui-flip-a"),
        "{status}"
    );
    assert_eq!(
        h.app.cur().unwrap().dir,
        a,
        "two quick presses land two back"
    );
    cleanup(a);
    cleanup(b);
    cleanup(c);
}

#[test]
fn tab_completes_directories_and_a_bad_path_says_so() {
    let a = small_repo("ui-tab");
    std::fs::create_dir_all(a.join("projects/codesnob")).unwrap();
    let mut h = Harness::open(&a);
    h.keys("oproj");
    h.app.dispatch(Msg::Input(Event::Key(KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::NONE,
    ))));
    assert!(h.screen().contains("Open › projects/"), "{}", h.screen());
    h.app.dispatch(Msg::Input(Event::Key(KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::NONE,
    ))));
    assert!(
        h.screen().contains("Open › projects/codesnob/"),
        "{}",
        h.screen()
    );

    h.keys("<c-u>nowhere<enter>");
    let screen = h.screen();
    assert!(
        screen.contains("no such directory, PR or revision: nowhere"),
        "{screen}"
    );
    assert!(
        screen.contains("Open ›"),
        "the prompt stays open to fix the path"
    );
    cleanup(a);
}

#[test]
fn recent_locations_survive_a_restart() {
    let a = small_repo("ui-recent-a");
    let b = small_repo("ui-recent-b");
    let state = fresh_dir("ui-recent-state");
    {
        let mut h = Harness::open_with_state(&a, state.clone());
        h.keys(&format!("o{}<enter>", b.display()));
    }
    let mut h = Harness::open_with_state(&a, state.clone());
    h.keys("o");
    let screen = h.screen();
    let rows: Vec<&str> = screen
        .lines()
        .skip_while(|l| !l.contains("Open ›"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(
        rows[0].contains("ui-recent-a") && rows[0].ends_with("open"),
        "{rows:?}"
    );
    assert!(rows[1].contains("ui-recent-b"), "{rows:?}");

    h.keys("<down><enter>");
    assert_eq!(h.app.cur().unwrap().dir, b, "Enter on a recent opens it");
    cleanup(a);
    cleanup(b);
    cleanup(state);
}

#[test]
fn the_location_list_switches_and_close_drops_one() {
    let a = small_repo("ui-list-a");
    let b = small_repo("ui-list-b");
    let mut h = Harness::open(&a);
    h.keys(&format!("o{}<enter>", b.display()));
    h.keys(":locations<enter>");
    let screen = h.screen();
    assert!(screen.contains("Locations ›"), "{screen}");
    assert!(
        screen.contains("main · everything"),
        "rows show branch and range: {screen}"
    );
    h.keys("<enter>");
    assert_eq!(
        h.app.cur().unwrap().dir,
        a,
        "starts on the previous location"
    );

    h.keys(":close loc<enter>");
    assert_eq!(h.app.locations.len(), 1);
    assert_eq!(h.app.cur().unwrap().dir, b);
    h.keys(":");
    assert!(
        !h.screen().contains("Close location"),
        "nothing left to close"
    );
    cleanup(a);
    cleanup(b);
}

/// Stands in for `gh pr view`: knows PR #7 of acme/widget.
struct FakeGitHub {
    info: crate::pr::Info,
}

impl crate::pr::Source for FakeGitHub {
    fn view(&self, spec: &crate::pr::Spec, _: Option<&Path>) -> Result<crate::pr::Info, String> {
        if spec.number == self.info.number {
            Ok(self.info.clone())
        } else {
            Err(format!("no pull request #{}", spec.number))
        }
    }
}

/// A local repository playing github.com/acme/widget, with PR #7 at
/// refs/pull/7/head, and a clone of it whose remote says github.com while a
/// URL rewrite sends fetches to the local one.
fn fake_github(name: &str) -> (PathBuf, PathBuf, FakeGitHub) {
    use crate::testutil::git;
    let origin = fresh_dir(&format!("{name}-origin"));
    git(&origin, &["init", "-q"]);
    std::fs::write(origin.join("a.txt"), "a\n").unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    git(&origin, &["checkout", "-q", "-b", "feature", "main"]);
    std::fs::write(origin.join("a.txt"), "a with retries\n").unwrap();
    git(&origin, &["commit", "-q", "-am", "Add retries"]);
    std::fs::write(origin.join("f.txt"), "tidy\n").unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-q", "-m", "Tidy"]);
    git(&origin, &["checkout", "-q", "main"]);
    std::fs::write(origin.join("b.txt"), "main moved on\n").unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-q", "-m", "main moves on"]);
    git(&origin, &["update-ref", "refs/pull/7/head", "feature"]);
    let head = std::process::Command::new("git")
        .current_dir(&origin)
        .args(["rev-parse", "feature"])
        .output()
        .unwrap();
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();

    let user = fresh_dir(&format!("{name}-user"));
    git(&user, &["clone", "-q", origin.to_str().unwrap(), "."]);
    let github = "https://github.com/acme/widget.git";
    git(&user, &["remote", "set-url", "origin", github]);
    git(
        &user,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.display()),
            github,
        ],
    );
    let info = crate::pr::Info {
        number: 7,
        title: "Add retries".to_string(),
        body: "Retries **twice** now.\n\n- first\n- second".to_string(),
        url: "https://github.com/acme/widget/pull/7".to_string(),
        base_ref_name: "main".to_string(),
        head_ref_name: "feature".to_string(),
        head_ref_oid: head,
        is_draft: true,
        state: "OPEN".to_string(),
        author: Some(crate::pr::Author {
            login: "octo".to_string(),
        }),
        merge_commit: None,
    };
    (origin, user, FakeGitHub { info })
}

fn pr_harness(name: &str) -> (Harness, PathBuf, PathBuf, PathBuf) {
    let (origin, user, fake) = fake_github(name);
    let state = fresh_dir(&format!("{name}-state"));
    let cache = fresh_dir(&format!("{name}-cache"));
    let mut h = Harness::build(
        &user,
        state.clone(),
        cache.clone(),
        std::sync::Arc::new(fake),
    );
    h.scratch.push(state);
    (h, origin, user, cache)
}

#[test]
fn a_pr_opens_as_a_checkout_diffed_against_its_base() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr");
    h.keys("o#7<enter>");
    let status = h.status();
    assert!(
        status.starts_with("#7 Add retries · into main · draft"),
        "{status}"
    );
    assert!(
        status.contains("2 files"),
        "the PR's files, not main's later work: {status}"
    );
    let screen = h.screen();
    assert!(
        screen.contains("a.txt") && screen.contains("f.txt") && !screen.contains("b.txt"),
        "{screen}"
    );
    assert_eq!(
        h.app.cur().unwrap().dir,
        std::fs::canonicalize(cache.join("pr/github.com/acme/widget/7")).unwrap(),
        "checked out in the cache"
    );

    assert!(!screen.contains("Retries"), "the description starts hidden");
    h.keys("d");
    let screen = h.screen();
    assert!(
        screen.contains("#7 · @octo · feature into main"),
        "{screen}"
    );
    assert!(
        screen.contains("Retries twice now.") && screen.contains("• first"),
        "{screen}"
    );

    h.keys("c");
    let screen = h.screen();
    assert!(screen.contains("Everything in #7"), "{screen}");
    assert!(
        screen.contains("Add retries") && screen.contains("Tidy"),
        "{screen}"
    );
    h.keys("<esc>-");
    assert_eq!(h.app.cur().unwrap().dir, user, "- flips back to the clone");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn a_pr_open_twice_is_switched_to_and_an_unknown_one_says_so() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr-twice");
    h.keys("o#7<enter>");
    h.keys("-");
    h.keys("ohttps://github.com/acme/widget/pull/7/files<enter>");
    assert_eq!(h.app.locations.len(), 2, "the open PR is reused");
    assert!(h.status().starts_with("#7 Add retries"), "{}", h.status());

    h.keys("o#8<enter>");
    assert!(h.screen().contains("no pull request #8"), "{}", h.screen());
    h.keys("<esc>");
    assert_eq!(h.app.locations.len(), 2, "Esc drops the failed PR");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn pr_checkouts_are_remembered_and_removed_on_request() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr-prune");
    h.keys("o#7<enter>");
    let checkout = h.app.cur().unwrap().dir.clone();

    assert_eq!(crate::pr::prune(&cache, Duration::ZERO), 0);
    assert!(checkout.exists(), "an open PR is kept");

    h.keys("-o");
    assert!(
        h.screen().contains("acme/widget#7 Add retries"),
        "recents list PRs: {}",
        h.screen()
    );
    h.keys("<esc>-:close loc<enter>");
    assert_eq!(crate::pr::prune(&cache, Duration::ZERO), 1);
    assert!(!checkout.exists());
    let refs = std::process::Command::new("git")
        .current_dir(&user)
        .args(["for-each-ref", "refs/codesnob"])
        .output()
        .unwrap();
    assert!(refs.stdout.is_empty(), "its refs go too");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn a_merged_pr_still_shows_its_own_changes() {
    use crate::testutil::git;
    let (origin, user, mut fake) = fake_github("ui-pr-merged");
    // Merge the PR on "GitHub", then main moves on again.
    git(
        &origin,
        &["merge", "-q", "--no-ff", "-m", "Merge #7", "feature"],
    );
    let merge = std::process::Command::new("git")
        .current_dir(&origin)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    std::fs::write(origin.join("c.txt"), "later\n").unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-q", "-m", "later work"]);
    fake.info.state = "MERGED".to_string();
    fake.info.is_draft = false;
    fake.info.merge_commit = Some(crate::pr::Oid {
        oid: String::from_utf8_lossy(&merge.stdout).trim().to_string(),
    });
    let state = fresh_dir("ui-pr-merged-state");
    let cache = fresh_dir("ui-pr-merged-cache");
    let mut h = Harness::build(
        &user,
        state.clone(),
        cache.clone(),
        std::sync::Arc::new(fake),
    );
    h.scratch.push(state);
    h.keys("o#7<enter>");
    let status = h.status();
    assert!(
        status.starts_with("#7 Add retries · into main · merged"),
        "{status}"
    );
    assert!(status.contains("2 files"), "the PR's own files: {status}");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

// ---- regressions found in review ------------------------------------------

fn wait_until(h: &mut Harness, what: &dyn Fn(&mut Harness) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !what(h) && Instant::now() < deadline {
        if let Ok(m) = h.rx.recv_timeout(Duration::from_millis(50)) {
            h.app.dispatch(m);
        }
    }
}

#[test]
fn the_file_list_follows_paths_across_a_reload() {
    let dir = branch_with_history("ui-files-reload");
    let mut h = Harness::open(&dir);
    h.keys("f");
    std::fs::write(dir.join("0first.txt"), "new\n").unwrap();
    wait_until(&mut h, &|h| h.status().contains("6 files"));
    h.keys("gone<enter>");
    let loc = h.app.cur().unwrap();
    let fi = loc.view.current_file().unwrap();
    assert_eq!(loc.view.files[fi].path, "gone.txt");
    cleanup(dir);
}

#[test]
fn a_reverted_preview_does_not_leak_its_position_into_the_next_range() {
    let dir = branch_with_history("ui-anchor-leak");
    let mut h = Harness::open(&dir);
    h.keys("jjjjjjjjjjjj");
    h.keys("c<down><esc>");
    h.keys("<a-up>");
    assert!(h.status().contains("uncommitted changes"), "{}", h.status());
    assert_eq!(
        h.app.cur().unwrap().view.scroll,
        0,
        "a new range starts at the top"
    );
    cleanup(dir);
}

#[test]
fn choosing_a_commit_without_the_narrowed_file_shows_all_and_says_so() {
    let dir = branch_with_history("ui-focus-sync");
    let mut h = Harness::open(&dir);
    h.keys("Nz");
    assert!(h.status().contains("b.txt only"), "{}", h.status());
    h.keys("c<down><enter>");
    let status = h.status();
    assert!(!status.contains(" only"), "{status}");
    assert!(h.app.cur().unwrap().focus == Focus::All);
    cleanup(dir);
}

#[test]
fn leaving_a_location_mid_preview_puts_its_range_back() {
    let a = branch_with_history("ui-show-preview-a");
    let b = small_repo("ui-show-preview-b");
    let mut h = Harness::open(&a);
    h.keys("c<down>");
    h.app.dispatch(Msg::Show(Target {
        dir: b.clone(),
        base: None,
        repo: true,
    }));
    h.settle();
    h.keys("-");
    assert_eq!(h.app.cur().unwrap().dir, a);
    assert!(h.app.cur().unwrap().range == Range::Everything);
    cleanup(a);
    cleanup(b);
}

#[test]
fn a_tiny_terminal_does_not_panic() {
    let dir = branch_with_history("ui-tiny");
    let mut h = Harness::open(&dir);
    for (w, ht) in [(60, 4), (60, 3), (20, 2)] {
        h.term = Terminal::new(TestBackend::new(w, ht)).unwrap();
        h.keys(":zzzz");
        h.screen();
        h.keys("<esc>");
    }
    cleanup(dir);
}

#[test]
fn the_pr_description_does_not_depend_on_the_commit_toggle() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr-desc-toggle");
    h.keys("o#7<enter>");
    h.keys("<a-up>");
    h.keys("d<esc>");
    assert!(!h.screen().contains("Retries twice"));
    h.keys("d");
    assert!(h.screen().contains("Retries twice now."), "{}", h.screen());
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn an_open_pr_survives_another_viewer_pruning() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr-held");
    h.keys("o#7<enter>");
    let checkout = h.app.cur().unwrap().dir.clone();
    assert_eq!(crate::pr::prune(&cache, Duration::ZERO), 0);
    assert!(checkout.exists(), "held open, so kept");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn opening_a_pr_leaves_the_clones_other_worktrees_registered() {
    use crate::testutil::git;
    let (mut h, origin, user, cache) = pr_harness("ui-pr-keep-wt");
    let side = fresh_dir("ui-pr-keep-wt-side");
    std::fs::remove_dir(&side).unwrap();
    git(
        &user,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            side.to_str().unwrap(),
        ],
    );
    std::fs::rename(&side, side.with_extension("moved")).unwrap();
    h.keys("o#7<enter>");
    let list = std::process::Command::new("git")
        .current_dir(&user)
        .args(["worktree", "list"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("[side]"),
        "a missing worktree of the user's stays registered"
    );
    cleanup(side.with_extension("moved"));
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn reopening_a_pr_from_another_clone_shows_its_new_head() {
    use crate::testutil::git;
    let (mut h, origin, user, cache) = pr_harness("ui-pr-reopen");
    h.keys("o#7<enter>");
    h.keys(":close loc<enter>");

    // The PR gains a commit; open it again from a second clone.
    git(&origin, &["checkout", "-q", "feature"]);
    std::fs::write(origin.join("a.txt"), "a with retries, again\n").unwrap();
    git(&origin, &["commit", "-q", "-am", "More retries"]);
    git(&origin, &["checkout", "-q", "main"]);
    git(&origin, &["update-ref", "refs/pull/7/head", "feature"]);
    let second = fresh_dir("ui-pr-reopen-second");
    git(&second, &["clone", "-q", origin.to_str().unwrap(), "."]);
    let github = "https://github.com/acme/widget.git";
    git(&second, &["remote", "set-url", "origin", github]);
    git(
        &second,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.display()),
            github,
        ],
    );
    h.keys(&format!("o{}<enter>", second.display()));
    h.keys("o#7<enter>");
    let checkout = h.app.cur().unwrap().dir.clone();
    let a = std::fs::read_to_string(checkout.join("a.txt")).unwrap();
    assert_eq!(
        a, "a with retries, again\n",
        "the checkout moved to the new head"
    );
    cleanup(second);
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn a_merged_pr_whose_base_branch_is_gone_still_opens() {
    use crate::testutil::git;
    let (origin, user, mut fake) = fake_github("ui-pr-gone-base");
    // #7 targeted "stack", which merged and was deleted; main took it in.
    git(&origin, &["branch", "stack", "main"]);
    git(&origin, &["checkout", "-q", "stack"]);
    git(
        &origin,
        &["merge", "-q", "--no-ff", "-m", "Merge #7", "feature"],
    );
    let merge = std::process::Command::new("git")
        .current_dir(&origin)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    git(&origin, &["checkout", "-q", "main"]);
    git(
        &origin,
        &["merge", "-q", "--no-ff", "-m", "Land the stack", "stack"],
    );
    git(&origin, &["branch", "-q", "-D", "stack"]);
    fake.info.base_ref_name = "stack".to_string();
    fake.info.state = "MERGED".to_string();
    fake.info.merge_commit = Some(crate::pr::Oid {
        oid: String::from_utf8_lossy(&merge.stdout).trim().to_string(),
    });
    let state = fresh_dir("ui-pr-gone-base-state");
    let cache = fresh_dir("ui-pr-gone-base-cache");
    let mut h = Harness::build(
        &user,
        state.clone(),
        cache.clone(),
        std::sync::Arc::new(fake),
    );
    h.scratch.push(state);
    h.keys("o#7<enter>");
    let status = h.status();
    assert!(
        status.starts_with("#7 Add retries · into stack"),
        "{status}\n{}",
        h.screen()
    );
    assert!(status.contains("2 files"), "{status}");
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

// ---- walkthrough notes --------------------------------------------------------

#[test]
fn alt_arrows_walk_the_commits_with_the_position_first() {
    let dir = branch_with_history("ui-alt-step");
    let mut h = Harness::open(&dir);
    h.keys("<a-up>");
    assert!(h.status().contains("uncommitted changes"), "{}", h.status());
    h.keys("<a-up>");
    let status = h.status();
    assert!(
        status.starts_with(&format!("{COMMIT_ICON} commit 2 of 2 · ")) && status.contains(" two"),
        "icon and position lead: {status}"
    );
    h.keys("<a-up>");
    assert!(h.status().contains("commit 1 of 2 · "), "{}", h.status());
    h.keys("<a-down><a-down>");
    assert!(h.status().contains("uncommitted changes"), "{}", h.status());
    h.keys("<esc>");
    assert!(h.status().contains("since main"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn stepping_on_a_branch_without_commits_says_so() {
    let dir = small_repo("ui-no-commits");
    crate::testutil::git(&dir, &["checkout", "-q", "-b", "feature", "main"]);
    let mut h = Harness::open(&dir);
    h.keys("<a-up>");
    assert!(h.status().contains("uncommitted changes"), "{}", h.status());
    h.keys("<a-up>");
    assert!(
        h.status().contains("no commits since the branch point"),
        "{}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn question_mark_no_longer_opens_the_palette_and_keys_follow_names() {
    let dir = branch_with_history("ui-keycaps");
    let mut h = Harness::open(&dir);
    h.keys("?");
    assert!(!h.screen().contains("Commands ›"));
    assert!(h.status().ends_with(": commands"), "{}", h.status());
    h.keys(":older");
    let screen = h.screen();
    let row = screen.lines().find(|l| l.contains("Older commit")).unwrap();
    assert!(row.trim_end().ends_with("Older commit [Alt-↑]"), "{row}");
    // The key is colored, with no background of its own.
    let y = screen
        .lines()
        .position(|l| l.contains("Older commit"))
        .unwrap() as u16;
    let buf = h.term.backend().buffer();
    let x = (0..buf.area.width)
        .find(|&x| buf[(x, y)].symbol() == "A")
        .unwrap();
    assert_eq!(buf[(x, y)].fg, Palette::dark().key_fg);
    assert_eq!(buf[(x, y)].bg, buf[(1, y)].bg, "same background as the row");
    cleanup(dir);
}

#[test]
fn fetching_a_pr_again_reports_when_it_is_done() {
    let (mut h, origin, user, cache) = pr_harness("ui-pr-refetch");
    h.keys("o#7<enter>");
    h.keys("r");
    let status = h.status();
    assert!(!status.contains("fetching"), "{status}");
    assert!(
        status.contains("no changes since the last load"),
        "{status}"
    );
    cleanup(origin);
    cleanup(user);
    cleanup(cache);
}

#[test]
fn u_toggles_the_uncommitted_changes_and_capital_l_lists_locations() {
    let a = branch_with_history("ui-u-key");
    let b = small_repo("ui-l-key");
    let mut h = Harness::open(&a);
    h.keys("u");
    assert!(
        h.status().starts_with("feature · uncommitted changes"),
        "{}",
        h.status()
    );
    h.keys("u");
    assert!(
        h.status().contains("since main"),
        "u again goes back: {}",
        h.status()
    );

    h.keys("L");
    assert!(
        h.status().contains("no other location open"),
        "{}",
        h.status()
    );
    h.keys(&format!("o{}<enter>", b.display()));
    h.keys("L");
    assert!(h.screen().contains("Locations ›"), "{}", h.screen());
    cleanup(a);
    cleanup(b);
}

#[test]
fn the_palette_lists_keys_users_know_and_no_housekeeping() {
    let dir = branch_with_history("ui-palette-keys");
    let mut h = Harness::open(&dir);
    h.keys(":top");
    assert!(h.screen().contains("Top [Home]"), "{}", h.screen());
    h.keys("<c-u>bottom");
    assert!(h.screen().contains("Bottom [End]"), "{}", h.screen());
    h.keys("<c-u>uncommitted");
    assert!(
        h.screen().contains("Uncommitted changes only [u]"),
        "{}",
        h.screen()
    );
    h.keys("<esc>");
    for gone in ["How the base was chosen", "Remove PR checkouts"] {
        h.keys(":");
        let mut all = String::new();
        for _ in 0..30 {
            all.push_str(&h.screen());
            h.keys("<down>");
        }
        assert!(!all.contains(gone), "{gone} is not a palette command");
        h.keys("<esc>");
    }
    cleanup(dir);
}

#[test]
fn alt_left_and_right_walk_the_locations_both_ways() {
    let a = small_repo("ui-alt-loc-a");
    let b = small_repo("ui-alt-loc-b");
    let c = small_repo("ui-alt-loc-c");
    let mut h = Harness::open(&a);
    h.keys(&format!("o{}<enter>", b.display()));
    h.keys(&format!("o{}<enter>", c.display()));
    let at = |h: &Harness| h.app.cur().unwrap().dir.clone();

    h.keys("<a-left>");
    assert_eq!(at(&h), b, "back one");
    h.keys("<a-left>");
    assert_eq!(at(&h), a, "and one further");
    h.keys("<a-right>");
    assert_eq!(at(&h), b, "Alt-Right walks back along the same strip");
    assert!(
        h.status().contains("ui-alt-loc-c"),
        "the strip is showing: {}",
        h.status()
    );

    // Settled, Alt-Right starts at the least recently used location.
    h.keys("j<a-right>");
    assert_eq!(at(&h), c, "most recent order is b, a, c");
    h.keys("j:next loc");
    assert!(
        h.screen().contains("Next location [Alt-→]"),
        "{}",
        h.screen()
    );
    cleanup(a);
    cleanup(b);
    cleanup(c);
}

#[test]
fn invalid_locations_go_once_a_valid_one_is_open() {
    let n1 = fresh_dir("ui-invalid-n1");
    let n2 = fresh_dir("ui-invalid-n2");
    let a = small_repo("ui-invalid-a");
    let mut h = Harness::open(&n1);
    h.keys(&format!("o{}<enter>", n2.display()));
    assert_eq!(h.app.locations.len(), 2, "with nothing valid, both stay");
    h.keys(&format!("o{}<enter>", a.display()));
    let dirs: Vec<PathBuf> = h.app.locations.iter().map(|l| l.dir.clone()).collect();
    assert_eq!(
        dirs,
        std::slice::from_ref(&a),
        "only the repository is left"
    );
    h.keys("<a-left>");
    assert!(
        h.status().contains("no other location open"),
        "{}",
        h.status()
    );
    cleanup(n1);
    cleanup(n2);
    cleanup(a);
}

#[test]
fn leaving_an_invalid_location_drops_it_and_the_walk_goes_on() {
    let a = small_repo("ui-leave-invalid-a");
    let b = small_repo("ui-leave-invalid-b");
    let n = fresh_dir("ui-leave-invalid-n");
    let mut h = Harness::open(&a);
    h.keys(&format!("o{}<enter>", b.display()));
    h.keys(&format!("o{}<enter>", n.display()));
    assert!(
        h.screen().contains("not inside a git repository"),
        "{}",
        h.screen()
    );
    assert_eq!(h.app.locations.len(), 3, "it stays while on screen");
    let at = |h: &Harness| h.app.cur().unwrap().dir.clone();

    h.keys("<a-left>");
    assert_eq!(at(&h), b);
    assert_eq!(h.app.locations.len(), 2, "gone once left");
    assert!(
        !h.status().contains("ui-leave-invalid-n"),
        "the strip forgets it: {}",
        h.status()
    );
    h.keys("<a-left>");
    assert_eq!(at(&h), a, "the walk goes on");
    h.keys("<a-right>");
    assert_eq!(at(&h), b, "and back");
    cleanup(a);
    cleanup(b);
    cleanup(n);
}

#[test]
fn a_pull_request_failing_out_of_sight_says_so_and_goes() {
    let a = small_repo("ui-pr-fail-behind");
    let mut h = Harness::open(&a);
    // Messages wait until the keys are in, so the fetch fails behind.
    h.keys("o#99<enter><a-left>");
    wait_until(&mut h, &|h| h.app.locations.len() == 1);
    assert_eq!(h.app.locations.len(), 1, "the failed PR is gone");
    assert_eq!(h.app.cur().unwrap().dir, a);
    // The status line says so once the location strip is gone.
    assert_eq!(
        h.app.message.as_deref(),
        Some("fetching #99 failed: no pull request #99")
    );
    cleanup(a);
}

// ---- comparisons and agent views --------------------------------------------

fn view(range: Option<&str>, files: &[&str]) -> crate::proto::ViewSpec {
    crate::proto::ViewSpec {
        range: range.map(str::to_owned),
        files: files.iter().map(|f| f.to_string()).collect(),
        ..Default::default()
    }
}

fn target(dir: &Path) -> Target {
    Target {
        dir: dir.to_path_buf(),
        base: None,
        repo: true,
    }
}

#[test]
fn the_open_prompt_takes_comparisons_in_git_syntax() {
    let dir = branch_with_history("ui-open-compare");
    crate::testutil::git(&dir, &["add", "b.txt"]);
    let mut h = Harness::open(&dir);

    h.keys("ostaged<enter>");
    assert!(
        h.status().starts_with("feature · staged changes"),
        "{}",
        h.status()
    );
    assert!(
        h.status().contains("1 file"),
        "only the staged b.txt: {}",
        h.status()
    );

    h.keys("ounstaged<enter>");
    assert!(
        h.status().starts_with("feature · unstaged changes"),
        "{}",
        h.status()
    );
    assert!(
        !h.screen().contains("new.txt"),
        "untracked files are not unstaged changes"
    );

    h.keys("omain..feature<enter>");
    assert!(
        h.status().starts_with("feature · main..feature"),
        "{}",
        h.status()
    );
    assert!(h.status().contains("3 files"), "{}", h.status());

    h.keys("<esc>");
    assert!(
        h.status().contains("since main"),
        "Esc returns to everything: {}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn tab_completes_branch_names_after_dots() {
    let dir = branch_with_history("ui-ref-tab");
    crate::testutil::git(&dir, &["branch", "release-1"]);
    let mut h = Harness::open(&dir);
    let tab = |h: &mut Harness| {
        h.app.dispatch(Msg::Input(Event::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        ))))
    };
    h.keys("omain...rel");
    tab(&mut h);
    assert!(
        h.screen().contains("Open › main...release-1"),
        "{}",
        h.screen()
    );
    cleanup(dir);
}

#[test]
fn an_agent_updates_the_location_in_place_and_esc_returns() {
    let dir = branch_with_history("ui-agent-here");
    let mut h = Harness::open(&dir);
    h.keys("NN");
    let before = h.app.cur().unwrap().view.scroll;

    h.app.show_view(
        target(&dir.join("src")),
        view(Some("main..feature"), &["a.txt", "gone.txt"]),
    );
    h.settle();
    assert_eq!(
        h.app.locations.len(),
        1,
        "the same checkout, updated in place"
    );
    let status = h.status();
    assert!(
        status.starts_with("2 files only · feature · main..feature · Esc returns"),
        "{status}"
    );
    assert!(!h.screen().contains("one.rs"), "only the two files show");
    assert!(
        status.contains("2 of 3 files"),
        "counts the files shown: {status}"
    );

    h.keys("<esc>");
    let status = h.status();
    assert!(
        status.starts_with("feature · 2 commits and uncommitted work since main"),
        "{status}"
    );
    assert!(!status.contains("Esc returns"));
    assert_eq!(
        h.app.cur().unwrap().view.scroll,
        before,
        "back at the same spot"
    );
    cleanup(dir);
}

#[test]
fn an_agent_view_of_another_repo_returns_to_the_previous_location() {
    let a = branch_with_history("ui-agent-a");
    let b = small_repo("ui-agent-b");
    let mut h = Harness::open(&a);
    h.app.show_view(target(&b), view(None, &["readme.md"]));
    h.settle();
    assert_eq!(h.app.cur().unwrap().dir, b);
    assert!(
        h.status().starts_with("readme.md only · main · "),
        "{}",
        h.status()
    );
    assert!(h.status().contains("Esc returns"), "{}", h.status());
    h.keys("<esc>");
    assert_eq!(
        h.app.cur().unwrap().dir,
        a,
        "Esc goes back to what was on screen"
    );
    assert_eq!(h.app.locations.len(), 2);
    cleanup(a);
    cleanup(b);
}

#[test]
fn an_agent_view_without_a_range_shows_everything_and_a_branch_name_sets_the_base() {
    let dir = branch_with_history("ui-agent-base");
    crate::testutil::git(&dir, &["branch", "older", "main"]);
    let mut h = Harness::open(&dir);
    h.keys("u");
    h.app.show_view(target(&dir), view(None, &[]));
    h.settle();
    let scope = h.app.scope();
    assert!(scope.ends_with("since main · Esc returns"), "{scope}");
    h.app.show_view(target(&dir), view(Some("older"), &[]));
    h.settle();
    assert!(h.app.scope().contains("since older"), "{}", h.app.scope());
    h.keys("<esc>");
    assert!(
        h.status().starts_with("feature · uncommitted changes"),
        "the first view before the agent: {}",
        h.status()
    );
    cleanup(dir);
}

// ---- files as they are ----------------------------------------------------------

fn code(rev: Option<&str>, files: &[(&str, Option<usize>)]) -> crate::proto::ViewSpec {
    crate::proto::ViewSpec {
        code: Some(crate::proto::CodeSpec {
            rev: rev.map(str::to_owned),
            files: files
                .iter()
                .map(|(p, l)| crate::proto::CodeFile {
                    path: p.to_string(),
                    line: *l,
                })
                .collect(),
        }),
        ..Default::default()
    }
}

/// A repo whose src/long.rs has 60 numbered lines.
fn repo_with_long_file(name: &str) -> PathBuf {
    let dir = branch_with_history(name);
    let body: String = (1..=60).map(|i| format!("// line {i}\n")).collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    crate::testutil::git(&dir, &["add", "src/long.rs"]);
    crate::testutil::git(&dir, &["commit", "-q", "-m", "long"]);
    dir
}

#[test]
fn show_code_opens_a_whole_file_at_its_line_and_esc_returns() {
    let dir = repo_with_long_file("ui-code-one");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", Some(40))]));
    h.settle();
    let status = h.app.scope();
    assert_eq!(status, "src/long.rs · working tree · Esc returns");
    let screen = h.screen();
    let marked = screen
        .lines()
        .find(|l| l.contains('▶'))
        .expect("the line is marked");
    assert!(
        marked.contains("40") && marked.contains("// line 40"),
        "{marked}"
    );
    let row = screen.lines().position(|l| l.contains('▶')).unwrap();
    assert!(
        (5..15).contains(&row),
        "about a third of the way down: row {row}\n{screen}"
    );
    assert!(!screen.contains("@@"), "a whole file has no hunk headers");
    assert!(h.status().contains("src/long.rs"), "{}", h.status());

    h.keys("<esc>");
    assert!(h.app.scope().contains("since main"), "{}", h.app.scope());
    assert!(!h.screen().contains('▶'));
    cleanup(dir);
}

#[test]
fn show_code_with_several_files_shows_excerpts() {
    let dir = repo_with_long_file("ui-code-many");
    let mut h = Harness::open(&dir);
    h.app.show_view(
        target(&dir),
        code(
            None,
            &[("src/long.rs", Some(30)), ("src/deep/one.rs", Some(1))],
        ),
    );
    h.settle();
    assert!(
        h.app.scope().starts_with("2 files · working tree"),
        "{}",
        h.app.scope()
    );
    let screen = h.screen();
    assert!(
        screen.contains("⋯ lines 22–38"),
        "an excerpt around line 30: {screen}"
    );
    assert!(!screen.contains("// line 50"), "not the whole file");
    cleanup(dir);
}

#[test]
fn show_code_at_a_revision_reads_it_from_there() {
    let dir = branch_with_history("ui-code-rev");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(Some("main"), &[("gone.txt", None)]));
    h.settle();
    let scope = h.app.scope();
    assert!(scope.starts_with("gone.txt @ "), "{scope}");
    assert!(!scope.contains("working tree"));
    assert!(h.screen().contains("gone.txt"), "{}", h.screen());
    cleanup(dir);
}

#[test]
fn the_open_prompt_shows_a_file_and_diff_keys_leave_it() {
    let dir = repo_with_long_file("ui-code-open");
    let mut h = Harness::open(&dir);
    h.keys(&format!("o{}<enter>", dir.join("src/long.rs").display()));
    assert_eq!(h.app.scope(), "src/long.rs · working tree");
    assert_eq!(h.app.locations.len(), 1, "in the repository's location");
    h.keys("u");
    assert!(
        h.app.scope().contains("uncommitted changes"),
        "{}",
        h.app.scope()
    );
    h.keys(&format!("o{}<enter>", dir.join("src/long.rs").display()));
    h.keys("<esc>");
    assert!(
        h.app.scope().contains("uncommitted changes"),
        "Esc leaves the file view: {}",
        h.app.scope()
    );
    cleanup(dir);
}

#[test]
fn a_file_view_follows_edits_without_moving() {
    let dir = repo_with_long_file("ui-code-live");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", Some(40))]));
    h.settle();
    h.keys("jjj");
    let first_numbered = |h: &mut Harness| {
        h.screen()
            .lines()
            .find(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .unwrap()
            .to_string()
    };
    let before = first_numbered(&mut h);
    let body: String = (1..=60).map(|i| format!("// line {i} edited\n")).collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| h.screen().contains("edited"));
    let after = first_numbered(&mut h);
    assert!(after.contains("edited"), "{after}");
    assert_eq!(
        before.split_whitespace().next(),
        after.split_whitespace().next(),
        "same line number at the top: {before} / {after}"
    );
    cleanup(dir);
}

// ---- highlights ---------------------------------------------------------------

fn hl(path: &str, start: usize, end: usize, note: Option<&str>) -> crate::proto::HighlightSpec {
    crate::proto::HighlightSpec {
        path: path.to_string(),
        start,
        end,
        note: note.map(str::to_owned),
    }
}

#[test]
fn highlights_are_numbered_tinted_and_stepped_through() {
    let dir = repo_with_long_file("ui-marks");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.highlights = vec![
        hl("src/long.rs", 20, 22, Some("the loop")),
        hl("src/long.rs", 45, 45, None),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    let note = screen.lines().find(|l| l.contains('①')).expect("numbered");
    assert!(note.contains("the loop"), "{note}");
    let row = screen.lines().position(|l| l.contains('①')).unwrap();
    assert_eq!(
        row, 1,
        "① sits at the top, under the file header:\n{screen}"
    );
    let marked: Vec<&str> = screen
        .lines()
        .filter(|l| l.starts_with('▌') && l.contains("// line 2"))
        .collect();
    assert_eq!(marked.len(), 3, "lines 20-22 carry the bar: {screen}");
    assert!(
        marked.iter().all(|l| l.contains(" 2")),
        "the bar sits beside the number, not on it: {marked:?}"
    );
    assert!(h.status().contains("highlight 1 of 2"), "{}", h.status());

    h.keys(">");
    assert_eq!(
        h.screen().lines().position(|l| l.contains('②')),
        Some(1),
        "every jump puts the note at the top"
    );
    assert!(h.status().contains("highlight 2 of 2"), "{}", h.status());
    h.keys("<");
    assert!(h.status().contains("highlight 1 of 2"), "{}", h.status());
    h.keys(":");
    assert!(h.screen().contains("Next highlight [>]"), "{}", h.screen());
    h.keys("<esc><esc>");
    assert!(
        !h.screen().contains('①'),
        "Esc returns and the highlights go"
    );
    cleanup(dir);
}

#[test]
fn a_highlight_on_an_unchanged_line_shows_it_in_the_diff() {
    let dir = repo_with_long_file("ui-marks-diff");
    let body: String = (1..=60)
        .map(|i| {
            if i == 5 {
                "// line 5 changed\n".to_string()
            } else {
                format!("// line {i}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    let mut h = Harness::open(&dir);
    let mut spec = view(Some("worktree"), &["src/long.rs"]);
    spec.highlights = vec![hl("src/long.rs", 50, 50, Some("not changed, but look"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(screen.contains("not changed, but look"), "{screen}");
    let line50 = screen
        .lines()
        .find(|l| l.contains("// line 50"))
        .expect("line 50 shows");
    assert!(line50.starts_with('▌'), "{line50}");
    cleanup(dir);
}

#[test]
fn highlights_follow_their_code_and_go_when_it_does() {
    let dir = repo_with_long_file("ui-marks-follow");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.highlights = vec![hl("src/long.rs", 30, 31, Some("watch me"))];
    h.app.show_view(target(&dir), spec);
    h.settle();

    // Two lines inserted at the top: the highlight moves down with its text.
    let body: String = std::iter::once("// new a\n// new b\n".to_string())
        .chain((1..=60).map(|i| format!("// line {i}\n")))
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| {
        h.screen().contains("new a") || h.app.cur().unwrap().marks[0].start == 32
    });
    let m = &h.app.cur().unwrap().marks[0];
    assert_eq!(
        (m.start, m.end),
        (32, 33),
        "follows lines 30-31 to their new place"
    );

    // Its text replaced: the highlight is removed, and the status bar says so.
    let body: String = (1..=60).map(|i| format!("// other {i}\n")).collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| h.app.cur().unwrap().marks.is_empty());
    assert!(h.app.cur().unwrap().marks.is_empty());
    assert!(
        h.status()
            .contains("a highlight no longer matches its code; removed"),
        "{}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn angle_brackets_move_between_hunks_without_highlights() {
    let dir = repo_with_long_file("ui-marks-none");
    let body: String = (1..=60)
        .map(|i| {
            if i == 5 || i == 50 {
                format!("// line {i} changed\n")
            } else {
                format!("// line {i}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    let mut h = Harness::open(&dir);
    h.keys("u");
    let first = h.app.cur().unwrap().view.scroll;
    h.keys(">");
    assert!(
        h.app.cur().unwrap().view.scroll > first,
        "> goes to the next hunk"
    );
    h.keys("<");
    assert!(!h.status().contains("highlight"));
    cleanup(dir);
}

#[test]
fn highlights_are_numbered_by_where_they_sit_not_the_order_given() {
    let dir = repo_with_long_file("ui-marks-order");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    // Given out of order; numbered top to bottom.
    spec.highlights = vec![
        hl("src/long.rs", 45, 45, Some("lowest")),
        hl("src/long.rs", 10, 11, Some("highest")),
        hl("src/long.rs", 30, 30, Some("middle")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let at_top = |h: &mut Harness| h.screen().lines().nth(1).unwrap_or("").to_string();
    assert!(
        at_top(&mut h).contains("① highest"),
        "opens at ①: {}",
        at_top(&mut h)
    );
    h.keys(">");
    assert!(at_top(&mut h).contains("② middle"), "{}", at_top(&mut h));
    h.keys(">");
    assert!(at_top(&mut h).contains("③ lowest"), "{}", at_top(&mut h));
    h.keys(">");
    assert!(at_top(&mut h).contains("③ lowest"), "stays on the last");
    h.keys("jjj<");
    assert!(
        at_top(&mut h).contains("② middle"),
        "scrolling does not change the order: {}",
        at_top(&mut h)
    );
    assert!(h.status().contains("highlight 2 of 3"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn highlights_across_files_go_in_the_order_the_files_are_shown() {
    let dir = repo_with_long_file("ui-marks-files");
    let mut h = Harness::open(&dir);
    // Listed long.rs first, so its highlights come first though its path sorts last.
    let mut spec = code(None, &[("src/long.rs", None), ("src/deep/one.rs", None)]);
    spec.highlights = vec![
        hl("src/deep/one.rs", 1, 1, Some("in one")),
        hl("src/long.rs", 50, 50, Some("long low")),
        hl("src/long.rs", 5, 5, Some("long high")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    // Excerpts: all three are on screen, top to bottom in number order.
    let screen = h.screen();
    let row = |want: &str| {
        screen
            .lines()
            .position(|l| l.contains(want))
            .unwrap_or_else(|| panic!("{want} on screen:\n{screen}"))
    };
    assert!(row("① long high") < row("② long low"), "{screen}");
    assert!(row("② long low") < row("③ in one"), "{screen}");
    h.keys(">>");
    assert!(h.status().contains("highlight 3 of 3"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn a_line_inside_a_highlight_opens_at_that_highlight() {
    let dir = repo_with_long_file("ui-marks-line-in");
    let mut h = Harness::open(&dir);
    // As an agent sent it: the line, and its highlight listed first.
    let mut spec = code(None, &[("src/long.rs", Some(40))]);
    spec.highlights = vec![
        hl("src/long.rs", 38, 42, Some("the point")),
        hl("src/long.rs", 5, 6, Some("setup")),
        hl("src/long.rs", 20, 20, Some("helper")),
        hl("src/long.rs", 55, 55, Some("after")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen.lines().nth(1).unwrap_or("").contains("③ the point"),
        "its note at the top:\n{screen}"
    );
    assert!(
        screen
            .lines()
            .any(|l| l.contains('▶') && l.contains("// line 40")),
        "the line is marked:\n{screen}"
    );
    assert!(h.status().contains("highlight 3 of 4"), "{}", h.status());
    h.keys(">");
    assert!(h.status().contains("highlight 4 of 4"), "{}", h.status());
    h.keys("<<");
    assert!(h.status().contains("highlight 2 of 4"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn a_line_outside_the_highlights_opens_at_the_line() {
    let dir = repo_with_long_file("ui-marks-line-out");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", Some(30))]);
    spec.highlights = vec![
        hl("src/long.rs", 5, 5, Some("early")),
        hl("src/long.rs", 55, 55, Some("late")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen
            .lines()
            .any(|l| l.contains('▶') && l.contains("// line 30")),
        "{screen}"
    );
    assert!(
        h.status().contains("2 highlights") && !h.status().contains(" of 2"),
        "no highlight is current yet: {}",
        h.status()
    );
    h.keys(">");
    assert_eq!(
        h.screen().lines().position(|l| l.contains("① early")),
        Some(1)
    );
    assert!(h.status().contains("highlight 1 of 2"), "{}", h.status());
    cleanup(dir);
}

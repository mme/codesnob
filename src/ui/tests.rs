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
            term: Terminal::new(TestBackend::new(120, 30)).unwrap(),
            scratch: Vec::new(),
        };
        h.settle();
        // Drawn once, as the real viewer is before any key arrives, so the
        // view knows how much is in sight.
        h.screen();
        h
    }

    /// Apply messages until no load is in flight and the location's watcher
    /// is live. A file written before the watcher starts is never reported:
    /// FSEvents only reports what happens after its stream starts, and
    /// events arriving before the worktree's directories are listed are
    /// dropped as irrelevant.
    fn settle(&mut self) {
        let busy = |app: &App| {
            app.loading()
                || app
                    .cur()
                    .is_some_and(|l| l._watch.is_some() && l.mode == crate::watch::Mode::Off)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while busy(&self.app) && Instant::now() < deadline {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.app.dispatch(m);
            }
        }
        assert!(!self.app.loading(), "load finished in time");
        assert!(!busy(&self.app), "the watcher started in time");
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
                    "s-enter" => KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
                    "a-enter" => KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
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

    /// The screen row drawn as the cursor's, and its text.
    /// The background of the first cell of the first row showing `needle`.
    fn bg_of(&mut self, needle: &str) -> Color {
        let screen = self.screen();
        let y = screen
            .lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} on screen:\n{screen}"));
        self.term.backend().buffer()[(0, y as u16)].bg
    }

    fn cursor(&mut self) -> (usize, String) {
        let screen = self.screen();
        let p = Palette::dark();
        let bases = [
            p.add_bg,
            p.del_bg,
            p.mark_bg,
            p.user_bg,
            p.header_bg,
            p.add_emph,
            p.del_emph,
        ];
        let cursor_bgs: Vec<Color> = std::iter::once(p.cursor_bg)
            .chain(bases.iter().map(|&c| p.cursor_over(c)))
            .collect();
        let buf = self.term.backend().buffer();
        let (w, h) = (buf.area.width, buf.area.height);
        let rows: Vec<usize> = (0..h - 1)
            .filter(|&y| cursor_bgs.contains(&buf[(w - 1, y)].bg))
            .map(|y| y as usize)
            .collect();
        assert_eq!(rows.len(), 1, "exactly one cursor row:\n{screen}");
        let text = screen.lines().nth(rows[0]).unwrap_or("").to_string();
        (rows[0], text)
    }

    /// What the daemon does for an agent: apply `f` to a copy of the
    /// location on screen's marks and hand the result back, as the daemon
    /// does after every change.
    fn agent<T>(
        &mut self,
        note: Option<String>,
        f: impl FnOnce(&mut Marks) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self.app.cur().unwrap().state().expect("a keyed location");
        let r = f(&mut state.marks)?;
        self.app.dispatch(Msg::Location {
            state: Box::new(state),
            note,
        });
        self.settle();
        Ok(r)
    }

    /// `get_review`: every mark, and how many of the user's words were
    /// unread before.
    fn review(&mut self) -> (Vec<crate::proto::MarkInfo>, usize) {
        self.agent(None, |m| {
            let unread = m.unread();
            Ok((m.read(), unread))
        })
        .unwrap()
    }

    fn reply(&mut self, letter: &str, text: &str) -> Result<String, String> {
        let letter = letter.to_string();
        let text = text.to_string();
        let mut state = self.app.cur().unwrap().state().expect("a keyed location");
        let l = state.marks.reply(&letter, &text)?;
        self.app.dispatch(Msg::Location {
            state: Box::new(state),
            note: Some(format!("agent replied under [{l}]")),
        });
        self.settle();
        Ok(l)
    }

    fn clear(
        &mut self,
        which: &str,
        letters: &[&str],
        user_asked: bool,
    ) -> Result<Vec<String>, String> {
        let letters: Vec<String> = letters.iter().map(|l| l.to_string()).collect();
        let mut state = self.app.cur().unwrap().state().expect("a keyed location");
        let cleared = state.marks.clear(which, &letters, user_asked)?;
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
        self.app.dispatch(Msg::Location {
            state: Box::new(state),
            note,
        });
        self.settle();
        Ok(cleared)
    }

    /// `mark`: a mark of the agent's with words, without a show.
    fn mark(&mut self, path: &str, start: usize, end: usize, text: &str) -> String {
        self.marks(&[hl(path, start, end, Some(text))]).remove(0)
    }

    /// `mark` with several: one event, as the daemon sends it.
    fn marks(&mut self, specs: &[crate::proto::MarkSpec]) -> Vec<String> {
        let mut state = self.app.cur().unwrap().state().expect("a keyed location");
        let placed = state.marks.mark(specs).unwrap();
        let result = crate::proto::MarkResult {
            location: String::new(),
            marks: placed,
            mine: Vec::new(),
            unread: 0,
        };
        let labels = result.marks.iter().map(|m| m.label.clone()).collect();
        self.app.dispatch(Msg::Location {
            state: Box::new(state),
            note: Some(format!("agent marked {}", result.placed())),
        });
        self.settle();
        labels
    }

    fn move_comment(
        &mut self,
        letter: &str,
        path: Option<&str>,
        start: usize,
        end: usize,
    ) -> Result<String, String> {
        let mut state = self.app.cur().unwrap().state().expect("a keyed location");
        let m = state
            .marks
            .move_to(letter, path.map(str::to_owned), start, end)?;
        let lines = if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        };
        let note = format!("agent moved [{}] to {} {lines}", m.label, m.path);
        let label = m.label.clone();
        self.app.dispatch(Msg::Location {
            state: Box::new(state),
            note: Some(note),
        });
        self.settle();
        Ok(label)
    }

    fn click(&mut self, row: u16) {
        self.app
            .dispatch(Msg::Input(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 10,
                row,
                modifiers: KeyModifiers::NONE,
            })));
        self.settle();
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
fn the_empty_screen_shows_the_sticker_above_one_line() {
    let dir = fresh_dir("ui-sticker");
    let mut h = Harness::open(&dir);
    let text_row = |screen: &str| {
        screen
            .lines()
            .position(|l| l.contains("not inside a git repository · "))
            .unwrap_or_else(|| panic!("{screen}"))
    };
    // Without images: the line alone, in the middle of the 29 rows above
    // the status bar.
    let screen = h.screen();
    assert_eq!(text_row(&screen), 14, "{screen}");
    let used = screen.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(used, 2, "one line, and the status bar:\n{screen}");

    // With images: the sticker, a blank row, the line, together in the
    // middle.
    let picker = ratatui_image::picker::Picker::halfblocks();
    h.app.logo = Some(logo::Logo::new(picker, false));
    let screen = h.screen();
    let row = text_row(&screen);
    let lines: Vec<&str> = screen.lines().collect();
    let drawn: Vec<usize> = (0..row)
        .filter(|&i| lines[i].contains(['▀', '▄', '█']))
        .collect();
    assert!(drawn.len() >= 3, "the sticker is drawn:\n{screen}");
    assert_eq!(drawn.last().copied(), Some(row - 2), "{screen}");
    assert!(lines[row - 1].trim().is_empty(), "{screen}");
    let (first, height) = (drawn[0], row + 1 - drawn[0]);
    assert!(
        first.abs_diff((29 - height) / 2) <= 1,
        "centered as a whole, from row {first} over {height} rows:\n{screen}"
    );

    // Too small a screen for it: the line alone again.
    h.term.backend_mut().resize(24, 10);
    let screen = h.screen();
    assert!(!screen.contains(['▀', '▄', '█']), "{screen}");
    assert!(screen.contains("not inside a git"), "{screen}");
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
    h.app.dispatch(Msg::Show(
        Target {
            dir: b.clone(),
            base: None,
            repo: true,
        },
        None,
    ));
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
                    ..Default::default()
                })
                .collect(),
        }),
        ..Default::default()
    }
}

/// A repo whose src/long.rs has 60 numbered lines.
#[test]
fn the_cursor_moves_within_the_view_and_the_view_follows_near_the_edge() {
    let dir = repo_with_long_file("ui-cursor");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    let (row, text) = h.cursor();
    assert_eq!(row, 0, "opens with the cursor on the header: {text}");
    let buf = h.term.backend().buffer();
    assert!(
        !buf[(0, 1)].modifier.contains(Modifier::BOLD),
        "other rows keep plain line numbers"
    );

    h.keys(&"j".repeat(20));
    let (row, text) = h.cursor();
    assert!(text.contains("// line 20"), "{text}");
    assert_eq!(
        row, 20,
        "the view stays put while the cursor is clear of the edge"
    );
    let buf = h.term.backend().buffer();
    assert!(
        buf[(0, 20)].modifier.contains(Modifier::BOLD),
        "the cursor row's line number is bold"
    );

    h.keys(&"j".repeat(8));
    let (row, text) = h.cursor();
    assert!(text.contains("// line 28"), "{text}");
    assert_eq!(
        row, 28,
        "the cursor reaches the bottom row before anything scrolls"
    );
    assert!(h.screen().contains("// line 1\n"), "{}", h.screen());

    h.keys("jj");
    let (row, text) = h.cursor();
    assert!(text.contains("// line 30"), "{text}");
    let screen = h.screen();
    assert!(
        !screen.contains("// line 1\n") && screen.contains("// line 30"),
        "then the view follows:\n{screen}"
    );
    assert!(
        screen.lines().next().unwrap().contains("src/long.rs"),
        "the header sticks once scrolled:\n{screen}"
    );
    assert_eq!(row, 28, "the cursor stays on the bottom row:\n{screen}");

    h.keys("G");
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 60"),
        "G lands on the last row: {text}"
    );
    assert!(
        h.screen().lines().nth(28).unwrap().contains("// line 60"),
        "with the last row at the bottom:\n{}",
        h.screen()
    );
    h.keys(&"k".repeat(31));
    let (row, text) = h.cursor();
    assert!(text.contains("// line 29"), "{text}");
    assert_eq!(
        row, 1,
        "the view scrolls up once the cursor would leave it at the top"
    );

    h.keys("g");
    assert_eq!(h.cursor().0, 0);
    cleanup(dir);
}

#[test]
fn paging_moves_the_view_and_keeps_the_cursor_in_sight() {
    let dir = repo_with_long_file("ui-cursor-page");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.keys(" ");
    let screen = h.screen();
    let (_, text) = h.cursor();
    assert!(
        !screen.contains("// line 2\n"),
        "a page down moves the view:\n{screen}"
    );
    assert!(
        text.contains("// line 27"),
        "the cursor keeps its place on screen, now the top row: {text}"
    );
    h.keys("<c-u>");
    let (row, text) = h.cursor();
    assert!(
        row == 1 && text.contains("// line 13"),
        "half a page up moves view and cursor alike: {row} {text}"
    );
    cleanup(dir);
}

#[test]
fn the_cursor_skips_blank_rows_and_follows_a_click() {
    let dir = branch_with_history("ui-cursor-blank");
    let mut h = Harness::open(&dir);
    let mut seen = Vec::new();
    for _ in 0..40 {
        h.keys("j");
        let (_, text) = h.cursor();
        assert!(!text.trim().is_empty(), "never rests on a blank row");
        seen.push(text);
    }
    assert!(
        seen.iter().filter(|t| t.contains("/")).count() >= 2,
        "steps onto the file headers on the way: {seen:?}"
    );
    h.keys("g");
    h.click(5);
    let (row, text) = h.cursor();
    assert_eq!(row, 5, "{text}");
    h.click(0);
    assert_eq!(h.cursor().0, 0, "the header counts too");
    cleanup(dir);
}

#[test]
fn a_reload_keeps_the_cursor_on_its_line_and_screen_row() {
    let dir = branch_with_history("ui-cursor-reload");
    let mut h = Harness::open(&dir);
    h.keys("jjjjj");
    let before = h.cursor();
    std::fs::write(dir.join("fresh.txt"), "new file\n").unwrap();
    h.keys("r");
    assert!(h.screen().contains("fresh.txt"), "{}", h.screen());
    assert_eq!(h.cursor(), before);
    cleanup(dir);
}

#[test]
fn enter_comments_on_the_cursors_line() {
    let dir = repo_with_long_file("ui-comment-line");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjj<enter>");
    assert!(
        h.status()
            .starts_with("src/long.rs 5 · Enter saves · Shift-Enter breaks the line · Esc cancels"),
        "the status bar names the line: {}",
        h.status()
    );
    h.keys("looks go");
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let row = lines
        .iter()
        .position(|l| l.contains("[A] you: looks go"))
        .unwrap_or_else(|| panic!("typed where it will go:\n{screen}"));
    assert!(
        lines[row - 1].contains("// line 5"),
        "under its line:\n{screen}"
    );
    assert!(
        !screen.contains(" Comment "),
        "no panel while typing:\n{screen}"
    );
    h.keys("od<enter>");
    let screen = h.screen();
    assert!(
        screen.contains("[A] you: looks good"),
        "the comment sits under its line, lettered:\n{screen}"
    );
    let (_, text) = h.cursor();
    assert!(
        text.contains("[A] you: looks good"),
        "saved, the cursor rests on it: {text}"
    );
    let line5 = screen.lines().find(|l| l.contains("// line 5")).unwrap();
    assert!(line5.starts_with("A "), "{line5}");
    assert!(h.status().contains("[A] saved"), "{}", h.status());
    h.keys("j");
    assert!(
        h.status().contains("1 yours") && h.status().contains("1 mark"),
        "{}",
        h.status()
    );

    // Esc in the prompt keeps nothing.
    h.keys("jjj<enter>nope<esc>");
    assert!(!h.screen().contains("nope") && !h.screen().contains("[B]"));
    assert_eq!(h.app.cur().unwrap().marks.marks.len(), 1);

    // Enter on the comment's own row answers it; e edits it.
    h.keys("<");
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 5"),
        "the jump lands on the first line: {text}"
    );
    h.keys("j<enter>");
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("<esc>e");
    let screen = h.screen();
    assert!(screen.contains("[A] src/long.rs 5"), "edits [A]:\n{screen}");
    h.keys("<c-u>still good<enter>");
    assert!(h.screen().contains("[A] you: still good"), "{}", h.screen());
    assert_eq!(
        h.app.cur().unwrap().marks.marks.len(),
        1,
        "edited, not added"
    );
    cleanup(dir);
}

#[test]
fn ctrl_space_marks_a_region_for_a_comment() {
    let dir = repo_with_long_file("ui-comment-region");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjjjjjjj<c- >");
    assert!(
        h.status()
            .starts_with("marking line 10 · Enter or Ctrl-Space to comment · Esc cancels"),
        "{}",
        h.status()
    );
    h.keys("jjj");
    assert!(
        h.status().starts_with("marking lines 10-13"),
        "{}",
        h.status()
    );
    // The cursor's own row (13) is shaded; the rest show the tint.
    let tinted: Vec<usize> = (10..=14)
        .filter(|n| h.bg_of(&format!("// line {n}")) == Palette::dark().user_bg)
        .collect();
    assert_eq!(tinted, [10, 11, 12], "the lines being marked are tinted");
    h.keys("kkkkkk");
    assert!(
        h.status().starts_with("marking lines 7-10"),
        "upwards too: {}",
        h.status()
    );
    h.keys("<esc>");
    assert!(h.status().contains("marking cancelled"), "{}", h.status());
    assert_ne!(h.bg_of("// line 10"), Palette::dark().user_bg);

    // v starts one too; a second Ctrl-Space ends it.
    h.keys("vjj<c- >");
    assert!(h.screen().contains("src/long.rs 7-9"), "{}", h.screen());
    h.keys("three lines<enter>");
    let screen = h.screen();
    assert!(screen.contains("[A] you: three lines"), "{screen}");
    let m = &h.app.cur().unwrap().marks.marks[0];
    assert_eq!((m.start, m.end), (7, 9));
    assert_eq!(
        m.text.as_deref(),
        Some(
            &[
                "// line 7".to_string(),
                "// line 8".to_string(),
                "// line 9".to_string()
            ][..]
        )
    );
    assert!(
        h.screen().contains("[A] you: three lines"),
        "{}",
        h.screen()
    );
    h.keys(":");
    assert!(
        h.screen().contains("Mark lines to comment on [Ctrl-Space]"),
        "{}",
        h.screen()
    );
    cleanup(dir);
}

#[test]
fn comments_are_deleted_one_by_one_or_cleared_after_a_question() {
    let dir = repo_with_long_file("ui-comment-delete");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 2, 3, Some("theirs"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    // Opens at [A], the cursor on line 2.
    h.keys("jjjj<enter>one<enter>");
    h.keys("jjjj<enter>two<enter>");
    h.keys("kj");
    assert!(
        h.status().contains("2 yours") && h.status().contains("3 marks"),
        "{}",
        h.status()
    );
    h.keys("x");
    assert!(h.status().contains("[C] removed"), "{}", h.status());
    assert!(
        !h.screen().contains("[C] you") && h.screen().contains("[B] you: one"),
        "{}",
        h.screen()
    );
    h.keys("X");
    assert!(
        h.status().contains("clear 2 marks here (1 yours)? y/n"),
        "{}",
        h.status()
    );
    h.keys("n");
    assert!(h.status().contains("kept") && h.screen().contains("[B] you: one"));
    h.keys("gjjjj");
    let (_, line) = h.cursor();
    assert!(line.contains("[A] theirs"), "{line}");
    h.keys("x");
    assert!(
        h.status().contains("[A] removed"),
        "the agent's mark goes too: {}",
        h.status()
    );
    assert!(!h.screen().contains("[A] theirs"), "{}", h.screen());
    h.keys("Xy");
    assert!(h.status().contains("1 mark cleared"), "{}", h.status());
    assert!(!h.screen().contains("[B]"), "{}", h.screen());
    h.keys("X");
    assert!(h.status().contains("no marks here"), "{}", h.status());
    // The letters go on where they left off.
    h.keys("jjjjjjj<enter>again<enter>");
    assert!(h.screen().contains("[D] you: again"), "{}", h.screen());
    cleanup(dir);
}

#[test]
fn a_comment_on_deleted_lines_speaks_in_old_line_numbers() {
    let dir = branch_with_history("ui-comment-deleted");
    let mut h = Harness::open(&dir);
    // gone.txt is deleted: its one line is on the old side only.
    h.keys("fgone<enter>");
    let (_, text) = h.cursor();
    assert!(text.contains("gone.txt"), "{text}");
    h.keys("jj<enter>");
    assert!(h.screen().contains("gone.txt old 1"), "{}", h.screen());
    h.keys("why<enter>");
    let m = &h.app.cur().unwrap().marks.marks[0];
    assert!(m.old_side && (m.start, m.end) == (1, 1) && !m.with_deletions);
    assert!(h.screen().contains("[A] you: why"), "{}", h.screen());
    // Deleted and added lines together: the new side's numbers, flagged.
    h.keys("fa.txt<enter>");
    h.keys("jj<c- >j<enter>");
    assert!(h.screen().contains("a.txt 1"), "{}", h.screen());
    h.keys("both<enter>");
    let m = &h.app.cur().unwrap().marks.marks[1];
    assert!(
        !m.old_side && m.with_deletions && (m.start, m.end) == (1, 1),
        "{:?}",
        (m.start, m.end)
    );
    cleanup(dir);
}

#[test]
fn a_comment_follows_its_lines_through_an_edit() {
    let dir = repo_with_long_file("ui-comment-follow");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjjjjjjj<enter>here<enter>");
    assert_eq!(h.app.cur().unwrap().marks.marks[0].start, 10);
    let body: String = std::iter::once("// new\n".to_string())
        .chain((1..=60).map(|i| format!("// line {i}\n")))
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| h.app.cur().unwrap().marks.marks[0].start == 11);
    assert!(h.screen().contains("[A] you: here"), "{}", h.screen());
    cleanup(dir);
}

#[test]
fn an_agents_call_is_answered_with_letters_location_and_unread_count() {
    let dir = repo_with_long_file("ui-ack");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 30, 30, Some("later")),
        hl("src/long.rs", 10, 11, None),
    ];
    let ack = h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    assert_eq!(ack.location, dir.display().to_string(), "named by its path");
    assert_eq!(ack.dir, dir);
    let letters: Vec<(String, usize)> = ack
        .marks
        .iter()
        .map(|m| (m.label.clone(), m.start))
        .collect();
    assert_eq!(
        letters,
        [("A".to_string(), 10), ("B".to_string(), 30)],
        "lettered by position, as the viewer shows them"
    );
    assert!(ack.applied && ack.unread == 0);
    assert!(!h.status().contains("L1"), "no ids: {}", h.status());

    h.keys("jjjjj<enter>hm<enter>k");
    assert!(h.status().contains("1 yours (1 unread)"), "{}", h.status());
    let ack = h
        .app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    assert_eq!(ack.unread, 1, "the reply nudges the agent");
    assert!(ack.marks.is_empty());
    cleanup(dir);
}

#[test]
fn get_review_lists_every_mark_and_counts_comments_as_read() {
    let dir = repo_with_long_file("ui-review");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 20, 22, Some("the loop"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jjjj<enter>needs a name<enter>k");
    assert!(h.status().contains("1 unread"), "{}", h.status());
    let (marks, unread) = h.review();
    assert_eq!(unread, 1);
    assert_eq!(marks.len(), 2);
    let a = &marks[0];
    assert_eq!(
        (a.label.as_str(), a.author, a.start, a.end),
        ("A", Author::Agent, 20, 22)
    );
    assert_eq!(a.text.as_deref(), Some("the loop"));
    assert!(a.lines.is_empty(), "no code for the agent's own highlights");
    assert!(!a.theirs);
    let b = &marks[1];
    assert_eq!(
        (b.label.as_str(), b.author, b.start, b.end),
        ("B", Author::User, 23, 23)
    );
    assert!(b.new && b.theirs, "not read before");
    assert_eq!(b.lines, ["// line 23"]);
    assert_eq!(b.text.as_deref(), Some("needs a name"));
    assert_eq!(
        b.render(),
        "[B] user mark src/long.rs 23 (new): needs a name\n    23 | // line 23"
    );

    // Read, the viewer no longer counts it unread, and a comment stays
    // read until edited.
    h.keys("j");
    assert!(!h.status().contains("unread"), "{}", h.status());
    let (marks, unread) = h.review();
    assert!(!marks[1].new && unread == 0, "{marks:?}");
    h.keys("<");
    h.keys(">");
    h.keys("je<c-u>better name<enter>");
    let (marks, unread) = h.review();
    assert!(
        marks[1].new && unread == 1,
        "an edit makes it new again: {marks:?}"
    );
    cleanup(dir);
}

#[test]
fn clear_marks_takes_letters_or_an_author() {
    let dir = repo_with_long_file("ui-clear");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 10, Some("a")),
        hl("src/long.rs", 20, 20, Some("b")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jj<enter>one<enter>");
    h.keys("jjjjj<enter>two<enter>");
    let cleared = h.clear("mine", &["a"], false).unwrap();
    assert_eq!(cleared, ["A"]);
    let loc = h.app.cur().unwrap();
    assert_eq!((loc.marks.marks.len(), loc.unread()), (3, 2));
    assert!(h.status().contains("agent cleared [A]"), "{}", h.status());
    assert!(!h.screen().contains("[A] a"), "{}", h.screen());
    let err = h.clear("mine", &["Z"], false).unwrap_err();
    assert_eq!(err, "no mark [Z]");
    let cleared = h.clear("mine", &[], false).unwrap();
    assert_eq!(cleared, ["B"]);
    assert!(h.screen().contains("[D] you: two") && !h.screen().contains("[B] b"));
    let cleared = h.clear("user", &[], true).unwrap();
    assert_eq!(cleared, ["C", "D"]);
    assert!(h.app.cur().unwrap().marks.marks.is_empty());
    let cleared = h.clear("all", &[], false).unwrap();
    assert!(cleared.is_empty());
    cleanup(dir);
}

#[test]
fn the_users_comments_go_only_when_the_user_asked() {
    let dir = repo_with_long_file("ui-clear-user");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 10, 10, Some("a"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jj<enter>one<enter>");
    h.keys("jjjjj<enter>two<enter>");
    h.reply("B", "noted").unwrap();

    // By letter, by author, or along with everything: refused, and nothing
    // goes, not even the agent's own marks named with them.
    let err = h.clear("mine", &["A", "b"], false).unwrap_err();
    assert!(
        err.starts_with("[B] is the user's: it goes only when the user explicitly asks for that")
            && err.contains("user_explicitly_asked_to_delete_comments")
            && err.ends_with("reply to it"),
        "{err}"
    );
    let err = h.clear("user", &[], false).unwrap_err();
    assert!(err.starts_with("[B] [C] are the user's"), "{err}");
    assert!(h.clear("all", &[], false).is_err());
    let left: Vec<String> = h
        .app
        .cur()
        .unwrap()
        .marks
        .marks
        .iter()
        .map(|m| format!("{}{}", m.label, m.thread.len()))
        .collect();
    assert_eq!(left, ["A0", "B1", "C0"], "nothing went");

    // The agent's own reply is part of the user's thread now: it cannot
    // be picked out, and replies have no letters to name it by.
    let err = h.clear("mine", &["B1"], false).unwrap_err();
    assert!(
        err.starts_with("no mark [B1]; replies have no letters"),
        "{err}"
    );

    // When the user asked, the mark goes whole, thread and all.
    let cleared = h.clear("mine", &["B"], true).unwrap();
    assert_eq!(cleared, ["B"]);
    assert_eq!(h.app.cur().unwrap().marks.marks.len(), 2);
    let screen = h.screen();
    assert!(
        !screen.contains("[B] you") && !screen.contains("agent: noted"),
        "{screen}"
    );
    cleanup(dir);
}

#[test]
fn the_agent_answers_a_comment_under_its_letter() {
    let dir = repo_with_long_file("ui-reply");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 10, 10, Some("look"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jjj<enter>why<enter>");
    assert_eq!(h.reply("b", "  because  ").unwrap(), "B");
    assert!(
        h.status().contains("agent replied under [B]"),
        "{}",
        h.status()
    );
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let b = lines
        .iter()
        .position(|l| l.contains("[B] you: why"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        lines[b - 1].contains("// line 12")
            && lines[b + 1].contains("agent: because")
            && lines[b + 2].contains("// line 13"),
        "the reply sits under its comment, under the code, with no letter of its own:\n{screen}"
    );

    // Replies go at the end of the thread; nothing else to address.
    assert_eq!(h.reply("B", "and more").unwrap(), "B");
    let err = h.reply("A", "x").unwrap_err();
    assert!(err.contains("your own"), "{err}");
    assert_eq!(h.reply("B", " ").unwrap_err(), "a reply needs text");
    assert_eq!(h.reply("Q", "x").unwrap_err(), "no mark [Q]");
    assert!(
        h.reply("B1", "x")
            .unwrap_err()
            .starts_with("no mark [B1]; replies have no letters"),
    );

    // The review carries the thread under the comment.
    let (marks, _) = h.review();
    let b = &marks[1];
    let authors: Vec<Author> = b.thread.iter().map(|t| t.author).collect();
    assert_eq!(authors, [Author::Agent, Author::Agent]);
    assert!(!b.thread[0].new, "the agent's own reply is never new to it");
    let r = crate::proto::ReviewResult {
        location: dir.display().to_string(),
        dir: dir.clone(),
        showing: String::new(),
        marks,
        unread: 0,
    };
    let text = r.render();
    assert!(
        text.contains("1 comment by the user, 2 replies")
            && text.contains("\n    agent: because\n    agent: and more"),
        "{text}"
    );

    // < and > step from mark to mark, over the thread: from [B]'s line
    // back to [A], and on to [B]'s line again.
    h.keys("<");
    let (_, line) = h.cursor();
    assert!(line.contains("// line 10"), "{line}");
    h.keys(">");
    let (_, line) = h.cursor();
    assert!(line.contains("// line 12"), "{line}");
    cleanup(dir);
}

#[test]
fn the_user_answers_the_agent_in_the_same_thread() {
    let dir = repo_with_long_file("ui-reply-user");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjj<enter>hm<enter>");
    h.review();
    h.reply("A", "fixed").unwrap();

    // Enter on the agent's reply, where the cursor landed, answers it at
    // the end of the thread, and the answer is typed right there.
    let (_, line) = h.cursor();
    assert!(line.contains("agent: fixed"), "{line}");
    h.keys("<enter>");
    assert!(
        h.status().starts_with(
            "reply under [A] · Enter saves · Shift-Enter breaks the line · Esc cancels"
        ),
        "{}",
        h.status()
    );
    h.keys("than");
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let fixed = lines
        .iter()
        .position(|l| l.contains("agent: fixed"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        lines[fixed + 1].contains("you: than") && lines[fixed + 2].contains("// line"),
        "typed under the thread, above the code:\n{screen}"
    );
    h.keys("ks<enter>");
    assert!(
        h.status().contains("reply under [A] saved"),
        "{}",
        h.status()
    );
    assert!(h.screen().contains("you: thanks"), "{}", h.screen());
    let loc = h.app.cur().unwrap();
    assert_eq!((loc.users_words(), loc.unread()), (2, 1));
    let (marks, unread) = h.review();
    assert!(marks[0].thread[1].new && unread == 1, "{marks:?}");
    assert!(
        marks[0]
            .render()
            .ends_with("\n    agent: fixed\n    user (new): thanks"),
        "{}",
        marks[0].render()
    );

    // e on the user's own reply, where the cursor rests once it is saved,
    // edits it, and it is new again.
    let (_, line) = h.cursor();
    assert!(line.contains("you: thanks"), "{line}");
    h.keys("e");
    assert!(
        h.screen().contains("your reply under [A]"),
        "{}",
        h.screen()
    );
    h.keys("<c-u>thanks!<enter>");
    assert!(h.screen().contains("you: thanks!"), "{}", h.screen());
    assert_eq!(h.app.cur().unwrap().unread(), 1);
    let (_, line) = h.cursor();
    assert!(
        line.contains("you: thanks!"),
        "the cursor stays on the reply: {line}"
    );

    // x removes the user's own reply, never the agent's.
    h.keys("x");
    assert!(
        h.status().contains("your reply under [A] removed"),
        "{}",
        h.status()
    );
    let (_, line) = h.cursor();
    assert!(line.contains("agent: fixed"), "{line}");
    h.keys("x");
    assert!(
        h.status()
            .contains("the agent's reply; x on the mark removes the thread"),
        "{}",
        h.status()
    );
    h.keys("e");
    assert!(h.status().contains("Enter answers it"), "{}", h.status());

    // Enter on the comment's own row answers it too.
    h.keys("k<enter>again<enter>");
    assert!(
        h.status().contains("reply under [A] saved"),
        "{}",
        h.status()
    );
    let thread: Vec<usize> = h.app.cur().unwrap().marks.marks[0]
        .thread
        .iter()
        .map(|t| t.n)
        .collect();
    assert_eq!(thread, [1, 3], "numbers are not reused");
    cleanup(dir);
}

#[test]
fn the_agent_moves_a_comment_to_the_code_it_is_about() {
    let dir = repo_with_long_file("ui-move");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 30, 30, Some("mine"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("gjjjjj<enter>here<enter>");
    assert_eq!(h.app.cur().unwrap().marks.marks[1].start, 5);
    h.reply("B", "moving it").unwrap();
    h.keys("kk");

    assert_eq!(h.move_comment("b", None, 40, 41).unwrap(), "B");
    assert!(
        h.status().contains("agent moved [B] to src/long.rs 40-41"),
        "{}",
        h.status()
    );
    let (_, line) = h.cursor();
    assert!(
        line.contains("// line 5"),
        "the user's cursor stays: {line}"
    );
    let m = &h.app.cur().unwrap().marks.marks[1];
    assert_eq!((m.label.as_str(), m.start, m.end), ("B", 40, 41));
    assert_eq!(
        m.text.as_deref(),
        Some(&["// line 40".to_string(), "// line 41".to_string()][..]),
        "it takes the text of its new lines"
    );
    h.keys("G");
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let b = lines
        .iter()
        .position(|l| l.contains("[B] you: here"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        lines[b - 1].contains("// line 41") && lines[b + 1].contains("agent: moving it"),
        "{screen}"
    );

    // By their letter; replies have none.
    let err = h.move_comment("B1", None, 1, 1).unwrap_err();
    assert!(
        err.starts_with("no mark [B1]; replies have no letters"),
        "{err}"
    );
    let err = h.move_comment("B", None, 5, 4).unwrap_err();
    assert!(err.contains("run forward"), "{err}");

    // It follows its new lines through an edit, as any comment does.
    let body: String = std::iter::once("// new\n".to_string())
        .chain((1..=60).map(|i| format!("// line {i}\n")))
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| h.app.cur().unwrap().marks.marks[1].start == 41);
    assert_eq!(h.app.cur().unwrap().marks.marks[1].end, 42);
    cleanup(dir);
}

#[test]
fn a_show_adds_marks_and_removes_none() {
    let dir = repo_with_long_file("ui-shows-add");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 10, 10, Some("first"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jjj<enter>mine<enter>");
    let mut spec = code(None, &[("a.txt", None)]);
    spec.marks = vec![hl("a.txt", 1, 1, Some("second"))];
    let ack = h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert_eq!(ack.marks[0].label, "C");
    let mine: Vec<&str> = ack.mine.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(mine, ["A", "C"], "the agent hears about all of its marks");
    assert!(
        screen.contains("[C] second") && !screen.contains("[A]"),
        "the view changed:\n{screen}"
    );
    assert_eq!(
        h.app.cur().unwrap().marks.marks.len(),
        3,
        "every mark stays, off screen"
    );
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 20, 20, Some("third"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("<<");
    let screen = h.screen();
    assert!(
        screen.contains("[A] first") && screen.contains("[B] you: mine"),
        "back on the file, the earlier marks are there:\n{screen}"
    );
    cleanup(dir);
}

#[test]
fn a_switch_waits_behind_a_banner_while_comments_are_unread() {
    let a = repo_with_long_file("ui-gate-a");
    let b = small_repo("ui-gate-b");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>look<enter>k");
    assert!(h.status().contains("1 unread"), "{}", h.status());
    h.app.dispatch(Msg::View(
        target(&b),
        Box::new(view(None, &["readme.md"])),
        None,
    ));
    h.settle();
    let screen = h.screen();
    assert_eq!(h.app.cur().unwrap().dir, a, "still here");
    let banner = screen.lines().next().unwrap();
    assert!(
        banner.starts_with(" the agent wants to show codesnob-ui-gate-b-")
            && banner.contains(": readme.md · y switches · Esc drops it"),
        "the banner:\n{screen}"
    );
    assert!(
        screen.contains("[A] you: look"),
        "the review stays on screen:\n{screen}"
    );
    h.keys("j");
    assert!(
        h.screen().contains("the agent wants"),
        "other keys leave the banner up"
    );
    h.keys("y");
    assert_eq!(h.app.cur().unwrap().dir, b, "y switches");
    assert!(!h.screen().contains("the agent wants"), "{}", h.screen());
    assert!(
        h.status().contains("switched to codesnob-ui-gate-b-")
            && h.status().contains(": readme.md, as the agent asked"),
        "{}",
        h.status()
    );
    h.keys("-");
    assert_eq!(h.app.cur().unwrap().dir, a);
    assert_eq!(
        h.app.cur().unwrap().unread(),
        1,
        "the comment is still there"
    );
    cleanup(a);
    cleanup(b);
}

#[test]
fn the_viewer_tells_the_daemon_what_it_has_open() {
    let a = repo_with_long_file("ui-locations-a");
    let b = small_repo("ui-locations-b");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>look<enter>k");
    h.app.dispatch(Msg::View(
        target(&b),
        Box::new(view(None, &["readme.md"])),
        None,
    ));
    h.settle();
    h.keys("y");
    h.settle();
    let entry = h.app.link.entry();
    assert_eq!(entry.location, Some(LocationKey::Repo { dir: b.clone() }));
    assert_eq!(entry.target.as_ref().map(|t| &t.dir), Some(&b));
    let open: Vec<(String, &str)> = entry
        .open
        .iter()
        .map(|o| (o.key.name(), o.showing.as_str()))
        .collect();
    assert_eq!(open.len(), 2, "both locations, the one on screen first");
    assert_eq!(open[0].0, b.display().to_string());
    assert!(open[0].1.starts_with("readme.md only · "), "{}", open[0].1);
    assert_eq!(open[1].0, a.display().to_string());
    assert!(!open[1].1.is_empty(), "described off screen too");

    // The comment left behind is still unread there, in this viewer's copy.
    assert_eq!(h.app.locations[1].unread(), 1);
    h.keys("-");
    assert_eq!(
        h.app.link.entry().location,
        Some(LocationKey::Repo { dir: a.clone() })
    );
    // With two open, the status bar says which this is, in a short name.
    h.keys("j");
    assert!(
        h.status().contains("codesnob-ui-locatio…  ·"),
        "{}",
        h.status()
    );
    cleanup(a);
    cleanup(b);
}

#[test]
fn a_held_request_is_dropped_with_esc_or_replaced_by_a_newer_one() {
    let a = repo_with_long_file("ui-gate-drop");
    let b = small_repo("ui-gate-drop-b");
    let c = small_repo("ui-gate-drop-c");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>look<enter>");
    let ack = h.app.show_view(target(&b), view(None, &[]));
    assert!(ack.applied, "show_view itself never holds; dispatch does");
    h.keys("-");
    assert_eq!(h.app.cur().unwrap().dir, a);
    h.app.dispatch(Msg::Show(target(&b), None));
    h.app.dispatch(Msg::Show(target(&c), None));
    h.settle();
    assert!(
        h.screen().contains("wants to show codesnob-ui-gate-drop-c"),
        "the newer one waits: {}",
        h.screen()
    );
    h.keys("<esc>");
    assert!(!h.screen().contains("wants to show"));
    assert!(
        h.status().contains("the agent's request was dropped"),
        "{}",
        h.status()
    );
    assert_eq!(h.app.cur().unwrap().dir, a);
    cleanup(a);
    cleanup(b);
    cleanup(c);
}

#[test]
fn a_held_request_applies_once_the_agent_reads_the_comments() {
    let a = repo_with_long_file("ui-gate-read");
    let b = small_repo("ui-gate-read-b");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>look<enter>");
    h.app.dispatch(Msg::View(
        target(&b),
        Box::new(view(None, &["readme.md"])),
        None,
    ));
    h.settle();
    assert_eq!(h.app.cur().unwrap().dir, a);
    let (_, unread) = h.review();
    assert_eq!(unread, 1);
    assert_eq!(
        h.app.cur().unwrap().dir,
        b,
        "read, the request goes through"
    );
    assert!(!h.screen().contains("wants to show"));
    cleanup(a);
    cleanup(b);
}

#[test]
fn typing_holds_a_switch_even_without_unread_comments() {
    let a = repo_with_long_file("ui-gate-typing");
    let b = small_repo("ui-gate-typing-b");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>half a thou");
    h.app.dispatch(Msg::Show(target(&b), None));
    h.settle();
    assert_eq!(h.app.cur().unwrap().dir, a);
    h.keys("ght<enter>");
    assert!(
        h.screen().contains("[A] you: half a thought"),
        "{}",
        h.screen()
    );
    assert!(
        h.screen()
            .contains("wants to show codesnob-ui-gate-typing-b"),
        "{}",
        h.screen()
    );
    // Reading while the user types changes nothing; afterwards it does.
    h.keys("kv");
    h.review();
    assert_eq!(
        h.app.cur().unwrap().dir,
        a,
        "a marking in progress keeps it held"
    );
    h.keys("<esc>");
    h.review();
    assert_eq!(h.app.cur().unwrap().dir, b);
    cleanup(a);
    cleanup(b);
}

#[test]
fn an_update_of_the_same_location_goes_through_and_says_what_it_added() {
    let dir = repo_with_long_file("ui-gate-inplace");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjjjjjjj<enter>here<enter>");
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 30, 30, Some("one")),
        hl("src/long.rs", 40, 40, Some("two")),
    ];
    h.app
        .dispatch(Msg::View(target(&dir), Box::new(spec), None));
    h.settle();
    assert!(h.status().contains("agent added [B] [C]"), "{}", h.status());
    let (_, text) = h.cursor();
    assert!(text.contains("// line 10"), "the cursor stays put: {text}");
    assert!(!h.screen().contains("wants to show"));
    cleanup(dir);
}

#[test]
fn without_a_review_in_progress_a_switch_is_immediate() {
    let a = repo_with_long_file("ui-gate-free");
    let b = small_repo("ui-gate-free-b");
    let mut h = Harness::open(&a);
    h.keys("jj<enter>look<enter>");
    h.review();
    h.app.dispatch(Msg::View(
        target(&b),
        Box::new(view(None, &["readme.md"])),
        None,
    ));
    h.settle();
    assert_eq!(h.app.cur().unwrap().dir, b);
    cleanup(a);
    cleanup(b);
}

#[test]
fn shift_enter_breaks_the_line_and_enter_on_a_marked_line_continues_the_thread() {
    let dir = repo_with_long_file("ui-multiline");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jjjjj<enter>first line<s-enter>second<a-enter>third");
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let row = lines
        .iter()
        .position(|l| l.contains("[A] you: first line"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        lines[row + 1].trim() == "second" && lines[row + 2].trim() == "third",
        "every line of the text has a row, indented under the first:\n{screen}"
    );
    assert!(
        lines[row + 1].find("second") == lines[row].find("first line"),
        "lined up with the text:\n{screen}"
    );
    assert!(lines[row + 3].contains("// line 6"), "{screen}");
    h.keys("<enter>");
    assert_eq!(
        h.app.cur().unwrap().marks.marks[0].comment.as_deref(),
        Some("first line\nsecond\nthird")
    );
    let (_, text) = h.cursor();
    assert!(text.contains("[A] you: first line"), "{text}");
    // A reply is typed under the whole text, with the cursor on it.
    h.keys("<enter>");
    let (row, text) = h.cursor();
    assert!(
        text.trim_end().ends_with("you:"),
        "on the row being typed: {text}"
    );
    let screen = h.screen();
    assert!(
        screen.lines().nth(row - 1).unwrap().trim() == "third",
        "under the last line of the text:\n{screen}"
    );
    h.keys("ok<enter>");
    let (_, text) = h.cursor();
    assert!(
        text.contains("you: ok"),
        "saved, the cursor stays on it: {text}"
    );
    h.keys("<enter>nope<esc>");
    let (_, text) = h.cursor();
    assert!(
        text.contains("you: ok"),
        "cancelled, the cursor goes back: {text}"
    );
    h.keys("j");
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 6"),
        "the cursor steps over the text's rows: {text}"
    );
    // Enter on the thread's last turn, on the mark's row, or on the marked
    // line goes on in the thread, not into a new mark; Esc goes back to
    // where Enter was pressed.
    h.keys("k<enter>");
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("<esc>");
    let (_, text) = h.cursor();
    assert!(text.contains("you: ok"), "{text}");
    h.keys("k<enter>");
    let (_, text) = h.cursor();
    assert!(
        text.trim_end().ends_with("you:"),
        "on the row being typed: {text:?}"
    );
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("<esc>");
    let (_, text) = h.cursor();
    assert!(text.contains("[A] you: first line"), "{text}");
    h.keys("k<enter>");
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("<esc>");
    assert_eq!(h.app.cur().unwrap().marks.marks.len(), 1);
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 5"),
        "cancelled, back on the line: {text}"
    );
    // v marks the same line again, on purpose.
    h.keys("v");
    assert!(h.status().starts_with("marking line 5"), "{}", h.status());
    h.keys("<enter>another<enter>");
    assert_eq!(h.app.cur().unwrap().marks.marks.len(), 2);
    let (marks, _) = h.review();
    assert_eq!(
        marks[0].render(),
        "[A] user mark src/long.rs 5 (new): first line\n    second\n    third\n    5 | // line 5\n    user (new): ok"
    );
    cleanup(dir);
}

#[test]
fn an_agents_mark_on_a_diff_shows_its_text_under_the_lines() {
    let dir = repo_with_long_file("ui-mark-on-diff");
    let body: String = (1..=60)
        .map(|i| {
            if (30..=33).contains(&i) {
                format!("// line {i} changed\n")
            } else {
                format!("// line {i}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), view(Some("worktree"), &["src/long.rs"]));
    h.settle();
    h.screen();
    let labels = h.marks(&[hl("src/long.rs", 30, 31, Some("one\ntwo"))]);
    assert_eq!(labels, ["A"]);
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let a = lines
        .iter()
        .position(|l| l.contains("[A] one"))
        .unwrap_or_else(|| panic!("the text shows:\n{screen}"));
    assert!(lines[a - 1].contains("// line 31 changed"), "{screen}");
    assert!(lines[a + 1].trim() == "two", "{screen}");
    assert!(lines[a + 2].contains("// line 32 changed"), "{screen}");
    // Line 30 holds the cursor, whose row is shaded; 31 shows the tint.
    let p = Palette::dark();
    assert_eq!(
        h.bg_of("// line 31 changed"),
        p.mark_bg,
        "the mark's tint takes over the added line"
    );
    assert_eq!(
        h.bg_of("// line 32 changed"),
        p.add_bg,
        "the diff colour past it"
    );
    cleanup(dir);
}

#[test]
fn the_previous_mark_from_an_inner_one_is_the_mark_around_it() {
    let dir = repo_with_long_file("ui-marks-nested");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 20, Some("outer")),
        hl("src/long.rs", 15, 16, Some("inner")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys(">");
    assert!(h.status().contains("[B] 2 of 2"), "{}", h.status());
    h.keys("<");
    assert!(h.status().contains("[A] 1 of 2"), "{}", h.status());
    let (_, text) = h.cursor();
    assert!(text.contains("// line 10"), "{text}");
    cleanup(dir);
}

#[test]
fn marks_starting_on_the_same_line_are_stepped_through_one_by_one() {
    let dir = repo_with_long_file("ui-marks-same-line");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 12, Some("short")),
        hl("src/long.rs", 10, 20, Some("long")),
        hl("src/long.rs", 30, 30, Some("later")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    assert!(h.status().contains("[A] 1 of 3"), "{}", h.status());
    h.keys(">");
    assert!(h.status().contains("[B] 2 of 3"), "{}", h.status());
    h.keys(">");
    assert!(h.status().contains("[C] 3 of 3"), "{}", h.status());
    h.keys("<");
    assert!(h.status().contains("[B] 2 of 3"), "{}", h.status());
    h.keys("<");
    assert!(h.status().contains("[A] 1 of 3"), "{}", h.status());
    h.keys("<");
    assert!(
        h.status().contains("[A] 1 of 3"),
        "nothing before: {}",
        h.status()
    );
    cleanup(dir);
}

#[test]
fn a_mark_on_a_line_the_diff_did_not_show_is_jumped_to_once_it_is() {
    let dir = repo_with_long_file("ui-jump-later");
    let body: String = (1..=60)
        .map(|i| {
            if i == 50 {
                "// line 50 changed\n".to_string()
            } else {
                format!("// line {i}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), view(Some("worktree"), &["src/long.rs"]));
    h.settle();
    h.screen();
    assert!(
        !h.screen().contains("// line 5\n"),
        "line 5 is outside the hunk"
    );
    assert_eq!(h.mark("src/long.rs", 5, 5, "up here"), "A");
    wait_until(&mut h, &|h| h.cursor().1.contains("// line 5"));
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 5"),
        "jumped once the line came in: {text}"
    );
    cleanup(dir);
}

#[test]
fn a_tall_block_at_the_end_scrolls_into_view_row_by_row() {
    let dir = repo_with_long_file("ui-tall-block");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    let text: String = (1..=60)
        .map(|i| format!("row {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(h.mark("src/long.rs", 60, 60, &text), "A");
    h.keys("G");
    let screen = h.screen();
    let (_, line) = h.cursor();
    assert!(
        line.contains("row 60"),
        "G lands on the last row of the text: {line}\n{screen}"
    );
    h.keys(&"k".repeat(59));
    let (_, line) = h.cursor();
    assert!(line.contains("[A] row 1"), "k walks up through it: {line}");
    assert!(!h.screen().contains("row 60"), "{}", h.screen());
    h.keys(&"j".repeat(59));
    let (_, line) = h.cursor();
    assert!(line.contains("row 60"), "j walks down through it: {line}");
    assert!(
        h.screen().contains("row 60"),
        "the tail came into view:\n{}",
        h.screen()
    );
    cleanup(dir);
}

#[test]
fn a_mermaid_fence_in_a_mark_is_drawn_as_a_diagram() {
    let dir = repo_with_long_file("ui-mermaid");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    let text =
        "How it flows:\n```mermaid\nflowchart LR\n  V[Viewer] --> D[Daemon]\n```\nThat is all.";
    assert_eq!(h.mark("src/long.rs", 10, 10, text), "A");
    let screen = h.screen();
    assert!(
        screen.contains("[A] How it flows:")
            && (screen.contains("drawing the diagram…") || screen.contains('┌')),
        "the words show at once, the drawing is on its way or there:\n{screen}"
    );
    wait_until(&mut h, &|h| h.screen().contains('┌'));
    let screen = h.screen();
    let rows: Vec<&str> = screen.lines().collect();
    let a = rows
        .iter()
        .position(|l| l.contains("[A] How it flows:"))
        .unwrap_or_else(|| panic!("{screen}"));
    let drawing: Vec<&str> = rows[a + 1..]
        .iter()
        .take_while(|l| !l.contains("That is all."))
        .copied()
        .collect();
    assert!(
        drawing.iter().any(|l| l.contains("Viewer"))
            && drawing.iter().any(|l| l.contains("Daemon")),
        "the nodes are drawn under the words:\n{screen}"
    );
    assert!(
        !screen.contains("```") && !screen.contains("drawing the diagram"),
        "the fence is gone:\n{screen}"
    );
    assert!(
        rows[a + 1 + drawing.len()].contains("That is all."),
        "the text goes on after the drawing:\n{screen}"
    );
    // The agent reads its own fence back, not the drawing.
    let (marks, _) = h.review();
    assert!(marks[0].text.as_deref().unwrap().contains("```mermaid"));
    // The cursor steps over the drawing's rows as over any text.
    let (_, text) = h.cursor();
    assert!(text.contains("// line 10"), "{text}");
    h.keys("jj");
    let (_, text) = h.cursor();
    assert!(text.contains("// line 11"), "{text}");
    cleanup(dir);
}

#[test]
fn deleting_a_comment_from_its_own_row_leaves_the_cursor_on_its_line() {
    let dir = repo_with_long_file("ui-comment-row-delete");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys(&"j".repeat(40));
    h.keys("<enter>gone soon<enter>");
    let (_, text) = h.cursor();
    assert!(
        text.contains("[A] you: gone soon"),
        "on the comment's row: {text}"
    );
    h.keys("e<c-u>still here<enter>");
    let (_, text) = h.cursor();
    assert!(
        text.contains("[A] you: still here"),
        "an edit keeps the cursor on the row: {text}"
    );
    h.keys("x");
    let (_, text) = h.cursor();
    assert!(
        text.contains("// line 40"),
        "the cursor lands on the line it was about: {text}"
    );
    assert!(
        h.screen().contains("// line 39"),
        "and the view stays put: {}",
        h.screen()
    );
    cleanup(dir);
}

#[test]
fn the_agents_news_moves_the_cursor_unless_the_user_is_busy() {
    let dir = repo_with_long_file("ui-agent-news");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    h.keys("jj<enter>why<enter>");
    h.keys("G");
    let (_, text) = h.cursor();
    assert!(text.contains("// line 60"), "{text}");
    // A reply of the agent's: the cursor lands on it, the mark at the top.
    h.reply("A", "because").unwrap();
    let (row, text) = h.cursor();
    assert!(text.contains("agent: because"), "{text}");
    let screen = h.screen();
    assert!(
        screen
            .lines()
            .nth(row - 1)
            .unwrap()
            .contains("[A] you: why"),
        "{screen}"
    );
    // Typing holds the cursor where it is.
    h.keys("G<enter>");
    assert!(h.screen().contains("you:"), "{}", h.screen());
    assert_eq!(h.mark("src/long.rs", 40, 40, "later"), "B");
    assert!(h.app.composing.is_some(), "still typing");
    h.keys("<esc>");
    let (_, text) = h.cursor();
    assert!(text.contains("// line 60"), "{text}");
    assert!(h.status().contains("agent marked [B]"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn several_marks_in_one_call_jump_once_and_separate_calls_wait_for_a_move() {
    let dir = repo_with_long_file("ui-agent-burst");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    // One call, three marks: the cursor goes to the first.
    let labels = h.marks(&[
        hl("src/long.rs", 20, 20, Some("one")),
        hl("src/long.rs", 40, 40, Some("two")),
        hl("src/long.rs", 50, 50, Some("three")),
    ]);
    assert_eq!(labels, ["A", "B", "C"]);
    let (_, text) = h.cursor();
    assert!(text.contains("// line 20"), "{text}");
    assert!(
        h.status().contains("agent marked [A] src/long.rs 20, [B]"),
        "{}",
        h.status()
    );
    // Another call while the cursor still sits there: no jump.
    assert_eq!(h.mark("src/long.rs", 30, 30, "four"), "D");
    let (_, text) = h.cursor();
    assert!(text.contains("// line 20"), "still on the first: {text}");
    // Once the user has moved, the next one jumps again.
    h.keys("j");
    assert_eq!(h.mark("src/long.rs", 45, 45, "five"), "E");
    let (_, text) = h.cursor();
    assert!(text.contains("// line 45"), "{text}");
    // Given bottom first, they are lettered top to bottom and the cursor
    // goes to the topmost.
    h.keys("j");
    let labels = h.marks(&[
        hl("src/long.rs", 58, 58, Some("low")),
        hl("src/long.rs", 48, 48, Some("high")),
    ]);
    assert_eq!(labels, ["F", "G"]);
    let (_, text) = h.cursor();
    assert!(text.contains("// line 48"), "{text}");
    let f = h.app.cur().unwrap().marks.get(5).unwrap();
    assert_eq!((f.label.as_str(), f.start), ("F", 48));
    cleanup(dir);
}

#[test]
fn an_agent_marks_lines_without_a_show() {
    let dir = repo_with_long_file("ui-agent-comment");
    let mut h = Harness::open(&dir);
    h.app
        .show_view(target(&dir), code(None, &[("src/long.rs", None)]));
    h.settle();
    h.screen();
    assert_eq!(h.mark("src/long.rs", 30, 31, "unused"), "A");
    // The user was idle, so the view goes to it.
    let (_, text) = h.cursor();
    assert!(text.contains("// line 30"), "{text}");
    let screen = h.screen();
    let a = screen
        .lines()
        .position(|l| l.contains("[A] unused"))
        .unwrap_or_else(|| panic!("the text is drawn under its lines:\n{screen}"));
    assert!(
        screen.lines().nth(a - 1).unwrap().contains("// line 31"),
        "{screen}"
    );
    let loc = h.app.cur().unwrap();
    assert_eq!(
        loc.marks.marks[0].text.as_deref().map(|t| t.len()),
        Some(2),
        "the viewer took its lines"
    );
    assert_eq!(loc.users_words(), 0, "not the user's");
    // The next show adds to it.
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 5, 5, None)];
    let ack = h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    let mine: Vec<&str> = ack.mine.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(mine, ["A", "B"]);
    h.keys(">");
    assert!(h.screen().contains("[A] unused"), "{}", h.screen());
    let (marks, _) = h.review();
    let a = marks.iter().find(|m| m.label == "A").unwrap();
    assert_eq!(a.render(), "[A] agent mark src/long.rs 30-31: unused");
    // The user answers it like any mark: Enter on its row, below its lines.
    h.keys("jj<enter>");
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("why?<enter>");
    assert!(h.screen().contains("you: why?"), "{}", h.screen());
    assert!(h.app.cur().unwrap().marks.marks[0].is_users());
    cleanup(dir);
}

#[test]
fn enter_on_an_agents_highlight_answers_it() {
    let dir = repo_with_long_file("ui-comment-reply");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 20, 22, Some("the loop"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    h.keys("jjj");
    let (_, text) = h.cursor();
    assert!(text.contains("[A] the loop"), "{text}");
    h.keys("<enter>");
    assert!(h.screen().contains("reply under [A]"), "{}", h.screen());
    h.keys("not a loop<enter>");
    let screen = h.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let a = lines
        .iter()
        .position(|l| l.contains("[A] the loop"))
        .unwrap();
    assert!(
        lines[a + 1].contains("you: not a loop"),
        "the answer sits in the thread under the highlight:\n{screen}"
    );
    let loc = h.app.cur().unwrap();
    assert_eq!(loc.marks.marks.len(), 1, "no mark of its own");
    assert!(
        loc.marks.marks[0].is_users(),
        "the thread is the user's now"
    );
    assert_eq!((loc.users_words(), loc.unread()), (1, 1));
    let (marks, _) = h.review();
    assert_eq!(
        marks[0].render(),
        "[A] agent mark src/long.rs 20-22: the loop\n    user (new): not a loop"
    );
    assert!(marks[0].theirs);
    // The agent may answer there now.
    h.reply("A", "it is").unwrap();
    assert!(h.screen().contains("agent: it is"), "{}", h.screen());
    cleanup(dir);
}

#[test]
fn angle_brackets_step_from_the_cursor_not_from_the_last_jump() {
    let dir = repo_with_long_file("ui-marks-from-cursor");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 10, Some("a")),
        hl("src/long.rs", 30, 30, Some("b")),
        hl("src/long.rs", 50, 50, Some("c")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    h.screen();
    assert!(h.status().contains("[A] 1 of 3"), "{}", h.status());
    // Twenty-five lines down, plus [A]'s and [B]'s own rows on the way.
    h.keys(&"j".repeat(27));
    let (_, text) = h.cursor();
    assert!(text.contains("// line 35"), "{text}");
    h.keys(">");
    assert!(
        h.status().contains("[C] 3 of 3"),
        "the next below the cursor: {}",
        h.status()
    );
    h.keys(&"k".repeat(16));
    h.keys("<");
    assert!(
        h.status().contains("[B] 2 of 3"),
        "the one above the cursor: {}",
        h.status()
    );
    h.keys("<");
    assert!(
        h.status().contains("[A] 1 of 3"),
        "from a mark's first line, the one before it: {}",
        h.status()
    );
    h.keys("k<");
    assert!(
        h.status().contains("[A] 1 of 3"),
        "from the row itself, nothing above: {}",
        h.status()
    );
    h.keys("G<");
    assert!(h.status().contains("[C] 3 of 3"), "{}", h.status());
    cleanup(dir);
}

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

fn hl(path: &str, start: usize, end: usize, text: Option<&str>) -> crate::proto::MarkSpec {
    crate::proto::MarkSpec {
        path: path.to_string(),
        start,
        end,
        text: text.map(str::to_owned),
    }
}

#[test]
fn highlights_are_numbered_tinted_and_stepped_through() {
    let dir = repo_with_long_file("ui-marks");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 20, 22, Some("the loop")),
        hl("src/long.rs", 45, 45, None),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    let note = screen
        .lines()
        .find(|l| l.contains("[A]"))
        .expect("lettered");
    assert!(note.contains("the loop"), "{note}");
    let row = screen.lines().position(|l| l.contains("[A]")).unwrap();
    assert_eq!(
        row, 4,
        "[A]'s first line sits at the top, its text under its three lines:\n{screen}"
    );
    assert!(
        screen.lines().nth(1).unwrap().contains("// line 20"),
        "{screen}"
    );
    // Line 20 holds the cursor, whose row is shaded.
    let tinted: Vec<usize> = (21..=24)
        .filter(|n| h.bg_of(&format!("// line {n}")) == Palette::dark().mark_bg)
        .collect();
    assert_eq!(tinted, [21, 22], "lines 20-22 carry the mark's tint");
    let line = |n: usize| {
        screen
            .lines()
            .find(|l| l.contains(&format!("// line {n}")))
            .unwrap()
            .to_string()
    };
    assert!(
        line(20).starts_with("A ") && line(21).starts_with("  ") && line(22).starts_with("  "),
        "the letter on the first line, nothing beside the others: {screen}"
    );
    assert!(
        line(20).contains(" 2"),
        "the letter sits beside the number, not on it: {}",
        line(20)
    );
    assert!(h.status().contains("[A] 1 of 2"), "{}", h.status());

    h.keys(">");
    assert_eq!(
        h.screen().lines().position(|l| l.contains("[B]")),
        Some(2),
        "every jump puts the first line at the top, the text under it"
    );
    assert!(h.status().contains("[B] 2 of 2"), "{}", h.status());
    h.keys("<");
    assert!(h.status().contains("[A] 1 of 2"), "{}", h.status());
    h.keys(":");
    assert!(h.screen().contains("Next mark [>]"), "{}", h.screen());
    h.keys("<esc><esc>");
    assert!(
        h.status().contains("of 2") && h.screen().contains("+60"),
        "Esc returns to the diff and the marks stay with their code: {}",
        h.status()
    );
    h.keys("><");
    assert!(h.screen().contains("[A] the loop"), "{}", h.screen());
    cleanup(dir);
}

#[test]
fn letters_are_fixed_and_a_new_call_continues_the_sequence() {
    let dir = repo_with_long_file("ui-marks-letters");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 11, Some("first")),
        hl("src/long.rs", 30, 30, Some("second")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen.contains("[A] first") && screen.contains("[B] second"),
        "{screen}"
    );

    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 20, 20, Some("third"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen.contains("[C] third"),
        "the next call goes on from where the letters were:\n{screen}"
    );
    assert!(h.status().contains("[C] 2 of 3"), "{}", h.status());
    h.keys("<");
    let screen = h.screen();
    assert!(
        screen.contains("[A] first") && screen.contains("[B] second"),
        "and keeps the earlier marks:\n{screen}"
    );

    // Twenty-six more and the letters go on past Z.
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = (1..=20).map(|i| hl("src/long.rs", i, i, None)).collect();
    h.app.show_view(target(&dir), spec);
    h.settle();
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = (40..=43).map(|i| hl("src/long.rs", i, i, None)).collect();
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen.contains("[X]") && screen.contains("[AA]"),
        "{screen}"
    );
    let line40 = screen.lines().find(|l| l.contains("// line 40")).unwrap();
    assert!(
        line40.starts_with("X  40"),
        "a two-letter column, padded: {line40}"
    );
    let line43 = screen.lines().find(|l| l.contains("// line 43")).unwrap();
    assert!(line43.starts_with("AA 43"), "{line43}");
    cleanup(dir);
}

#[test]
fn overlapping_marks_show_the_newest_letter_and_all_of_them_under_the_cursor() {
    let dir = repo_with_long_file("ui-marks-overlap");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![
        hl("src/long.rs", 10, 20, Some("outer")),
        hl("src/long.rs", 15, 16, Some("inner")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    let line = |n: usize| {
        screen
            .lines()
            .find(|l| l.contains(&format!("// line {n}")))
            .unwrap()
            .to_string()
    };
    assert!(line(10).starts_with("A "), "{}", line(10));
    assert!(line(12).starts_with("  "), "{}", line(12));
    assert!(
        line(15).starts_with("B "),
        "the newer mark wins the gutter: {}",
        line(15)
    );
    assert!(line(16).starts_with("  "), "{}", line(16));
    assert!(line(17).starts_with("  "), "{}", line(17));
    // Line 10 holds the cursor, whose row is shaded.
    let tinted: Vec<usize> = (11..=21)
        .filter(|n| h.bg_of(&format!("// line {n}")) == Palette::dark().mark_bg)
        .collect();
    assert_eq!(
        tinted,
        (11..=20).collect::<Vec<_>>(),
        "every marked line is tinted"
    );
    assert!(!h.status().contains("[A] [B]"), "{}", h.status());
    // Five lines down.
    h.keys("jjjjj");
    assert!(
        h.cursor().1.contains("// line 15") && h.status().contains("[A] [B]"),
        "both letters under the cursor: {}",
        h.status()
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
    spec.marks = vec![hl("src/long.rs", 50, 50, Some("not changed, but look"))];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(screen.contains("not changed, but look"), "{screen}");
    let line50 = screen
        .lines()
        .find(|l| l.contains("// line 50"))
        .expect("line 50 shows");
    assert!(line50.starts_with('A'), "{line50}");
    cleanup(dir);
}

#[test]
fn highlights_follow_their_code_and_go_when_it_does() {
    let dir = repo_with_long_file("ui-marks-follow");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.marks = vec![hl("src/long.rs", 30, 31, Some("watch me"))];
    h.app.show_view(target(&dir), spec);
    h.settle();

    // Two lines inserted at the top: the highlight moves down with its text.
    let body: String = std::iter::once("// new a\n// new b\n".to_string())
        .chain((1..=60).map(|i| format!("// line {i}\n")))
        .collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| {
        h.screen().contains("new a") || h.app.cur().unwrap().marks.marks[0].start == 32
    });
    let m = &h.app.cur().unwrap().marks.marks[0];
    assert_eq!(
        (m.start, m.end),
        (32, 33),
        "follows lines 30-31 to their new place"
    );

    // Its text replaced: the highlight goes stale, keeps its letter, and
    // the status bar says so.
    let body: String = (1..=60).map(|i| format!("// other {i}\n")).collect();
    std::fs::write(dir.join("src/long.rs"), body).unwrap();
    wait_until(&mut h, &|h| h.app.cur().unwrap().marks.marks[0].stale);
    assert!(
        h.status().contains("[A] no longer matches its code"),
        "{}",
        h.status()
    );
    let screen = h.screen();
    assert!(
        screen.contains("[A] stale · watch me"),
        "the row stays, flagged:\n{screen}"
    );
    assert!(
        !screen.lines().any(|l| l.starts_with('A')),
        "no line is lettered:\n{screen}"
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
    assert!(!h.status().contains("mark"));
    cleanup(dir);
}

#[test]
fn highlights_are_numbered_by_where_they_sit_not_the_order_given() {
    let dir = repo_with_long_file("ui-marks-order");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    // Given out of order; numbered top to bottom.
    spec.marks = vec![
        hl("src/long.rs", 45, 45, Some("lowest")),
        hl("src/long.rs", 10, 11, Some("highest")),
        hl("src/long.rs", 30, 30, Some("middle")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    // The first rows under the header: a mark's lines, then its text.
    let at_top = |h: &mut Harness| {
        h.screen()
            .lines()
            .skip(1)
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        at_top(&mut h).contains("[A] highest"),
        "opens at [A]: {}",
        at_top(&mut h)
    );
    h.keys(">");
    assert!(at_top(&mut h).contains("[B] middle"), "{}", at_top(&mut h));
    h.keys(">");
    assert!(at_top(&mut h).contains("[C] lowest"), "{}", at_top(&mut h));
    h.keys(">");
    assert!(at_top(&mut h).contains("[C] lowest"), "stays on the last");
    h.keys("jjj<");
    assert!(
        at_top(&mut h).contains("[C] lowest"),
        "< goes to the mark above the cursor, which is still [C]: {}",
        at_top(&mut h)
    );
    h.keys("<");
    assert!(at_top(&mut h).contains("[B] middle"), "{}", at_top(&mut h));
    assert!(h.status().contains("[B] 2 of 3"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn highlights_across_files_go_in_the_order_the_files_are_shown() {
    let dir = repo_with_long_file("ui-marks-files");
    let mut h = Harness::open(&dir);
    // Listed long.rs first, so its highlights come first though its path sorts last.
    let mut spec = code(None, &[("src/long.rs", None), ("src/deep/one.rs", None)]);
    spec.marks = vec![
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
    assert!(row("[A] long high") < row("[B] long low"), "{screen}");
    assert!(row("[B] long low") < row("[C] in one"), "{screen}");
    h.keys(">>");
    assert!(h.status().contains("[C] 3 of 3"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn a_line_inside_a_highlight_opens_at_that_highlight() {
    let dir = repo_with_long_file("ui-marks-line-in");
    let mut h = Harness::open(&dir);
    // As an agent sent it: the line, and its highlight listed first.
    let mut spec = code(None, &[("src/long.rs", Some(40))]);
    spec.marks = vec![
        hl("src/long.rs", 38, 42, Some("the point")),
        hl("src/long.rs", 5, 6, Some("setup")),
        hl("src/long.rs", 20, 20, Some("helper")),
        hl("src/long.rs", 55, 55, Some("after")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let screen = h.screen();
    assert!(
        screen.lines().nth(1).unwrap_or("").contains("// line 38"),
        "its first line at the top:\n{screen}"
    );
    let rows: Vec<&str> = screen.lines().collect();
    let c = rows
        .iter()
        .position(|l| l.contains("[C] the point"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        rows[c - 1].contains("// line 42"),
        "its text under its lines:\n{screen}"
    );
    assert!(
        screen
            .lines()
            .any(|l| l.contains('▶') && l.contains("// line 40")),
        "the line is marked:\n{screen}"
    );
    assert!(h.status().contains("[C] 3 of 4"), "{}", h.status());
    h.keys(">");
    assert!(h.status().contains("[D] 4 of 4"), "{}", h.status());
    h.keys("<<");
    assert!(h.status().contains("[B] 2 of 4"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn a_line_outside_the_highlights_opens_at_the_line() {
    let dir = repo_with_long_file("ui-marks-line-out");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", Some(30))]);
    spec.marks = vec![
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
        h.status().contains("2 marks") && !h.status().contains(" of 2"),
        "no highlight is current yet: {}",
        h.status()
    );
    h.keys(">");
    assert_eq!(
        h.screen().lines().position(|l| l.contains("[B] late")),
        Some(2),
        "the next mark below line 30, its line at the top"
    );
    assert!(h.status().contains("[B] 2 of 2"), "{}", h.status());
    h.keys("<<");
    assert!(h.status().contains("[A] 1 of 2"), "{}", h.status());
    cleanup(dir);
}

#[test]
fn one_file_can_show_just_its_highlights_like_a_diff() {
    let dir = repo_with_long_file("ui-code-excerpts");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None)]);
    spec.code.as_mut().unwrap().files[0].excerpts = Some(true);
    spec.marks = vec![
        hl("src/long.rs", 10, 11, Some("first")),
        hl("src/long.rs", 45, 45, Some("second")),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let view = &h.app.cur().unwrap().view;
    let shown = |line| view.row_of_line("src/long.rs", line).is_some();
    // Three lines of context, as in a diff.
    for line in [7, 8, 10, 11, 14, 42, 45, 48] {
        assert!(shown(line), "line {line} is shown");
    }
    for line in [6, 15, 30, 41, 49, 60] {
        assert!(!shown(line), "line {line} is left out");
    }
    cleanup(dir);
}

#[test]
fn one_of_several_files_can_show_whole() {
    let dir = repo_with_long_file("ui-code-whole");
    let mut h = Harness::open(&dir);
    let mut spec = code(None, &[("src/long.rs", None), ("src/deep/one.rs", None)]);
    spec.code.as_mut().unwrap().files[0].excerpts = Some(false);
    spec.marks = vec![
        hl("src/long.rs", 5, 5, None),
        hl("src/deep/one.rs", 1, 1, None),
    ];
    h.app.show_view(target(&dir), spec);
    h.settle();
    let view = &h.app.cur().unwrap().view;
    assert!(
        (1..=60).all(|l| view.row_of_line("src/long.rs", l).is_some()),
        "long.rs shows whole"
    );
    assert!(view.row_of_line("src/deep/one.rs", 1).is_some());
    cleanup(dir);
}

#[test]
fn the_open_prompt_shows_a_file_as_of_a_revision() {
    let dir = repo_with_long_file("ui-open-at-rev");
    let mut h = Harness::open(&dir);
    // a.txt changed on the branch; main still has the first version.
    h.keys("omain:a.txt<enter>");
    assert!(h.app.scope().starts_with("a.txt @ "), "{}", h.app.scope());
    let screen = h.screen();
    assert!(screen.contains("a.txt"), "{screen}");
    assert!(!screen.contains("a changed"), "the old text: {screen}");
    h.keys("<esc>");
    assert!(
        !h.app.scope().contains('@'),
        "Esc leaves the file view: {}",
        h.app.scope()
    );

    h.keys("omain:nope.txt<enter>");
    assert!(
        h.screen().contains("nope.txt does not exist at main"),
        "{}",
        h.screen()
    );
    cleanup(dir);
}

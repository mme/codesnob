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
}

impl Harness {
    fn open(dir: &Path) -> Self {
        let (tx, rx) = channel();
        let hl = Highlighter::new("base16").unwrap();
        let mut app = App::new(Options { context: 3 }, Palette::dark(), hl, tx, Link::new());
        app.open(Target {
            dir: dir.to_path_buf(),
            base: None,
            repo: gix::discover(dir).is_ok(),
        });
        let mut h = Self {
            app,
            rx,
            term: Terminal::new(TestBackend::new(100, 30)).unwrap(),
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
    assert!(screen.contains("Next hunk"), "{screen}");
    let next_hunk = screen.lines().find(|l| l.contains("Next hunk")).unwrap();
    assert!(
        next_hunk.trim_end().ends_with('n'),
        "the shortcut is shown: {next_hunk}"
    );
    assert!(
        !screen.contains("Everything since branch point"),
        "commands that do not apply are hidden"
    );

    h.keys("uncom<enter>");
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
    h.keys(":how<enter>");
    h.keys("?");
    let screen = h.screen();
    let first = screen
        .lines()
        .find(|l| l.starts_with("› "))
        .expect("a selected row");
    assert!(first.contains("How the base was chosen"), "{screen}");
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
fn the_base_explanation_is_one_command_away() {
    let dir = branch_with_history("ui-explain");
    let mut h = Harness::open(&dir);
    h.keys(":how the base<enter>");
    let status = h.status();
    assert!(
        status.contains("Base: feature was created from main"),
        "{status}"
    );
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

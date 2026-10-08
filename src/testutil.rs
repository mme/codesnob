//! Repositories for tests, built with the git CLI.

use std::path::{Path, PathBuf};

pub fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
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
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An empty, canonicalized scratch directory unique to this test.
pub fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("codesnob-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// main: "base" with a.txt, b.txt, gone.txt.
/// feature: "one" adds src/deep/one.rs; "two" edits a.txt and deletes gone.txt.
/// Uncommitted: b.txt edited, new.txt untracked.
pub fn branch_with_history(name: &str) -> PathBuf {
    let dir = fresh_dir(name);
    git(&dir, &["init", "-q"]);
    for f in ["a.txt", "b.txt", "gone.txt"] {
        std::fs::write(dir.join(f), format!("{f}\n")).unwrap();
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "feature", "main"]);
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    std::fs::write(dir.join("src/deep/one.rs"), "fn one() {}\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &["commit", "-q", "-m", "one\n\nAdds the first function."],
    );
    std::fs::write(dir.join("a.txt"), "a changed\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "two"]);
    std::fs::write(dir.join("b.txt"), "b changed\n").unwrap();
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();
    dir
}

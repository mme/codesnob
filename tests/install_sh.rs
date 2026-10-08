//! The installer, run for real against a mock of GitHub's release URLs.
//!
//! install.sh executes on machines codesnob has never touched, so it is
//! tested the way it runs: `sh install.sh` against a local server speaking the
//! URLs the script knows, into a scratch HOME, with a `gh` shim first on PATH
//! so the real gh is never reached. Runs on Linux and macOS, exercising the
//! platform the test runs on.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TAG: &str = "v9.9.9";

fn suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        "aarch64-darwin"
    } else {
        "x86_64-linux"
    }
}

fn tarball_name() -> String {
    format!("codesnob-{TAG}-{}.tar.gz", suffix())
}

/// A scratch directory removed when dropped, even when a test fails.
struct Dir(PathBuf);

impl std::ops::Deref for Dir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("codesnob-install-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// One fake release: the tarball publish.yml would build, and its sidecar.
struct Fixture {
    tarball: Vec<u8>,
    sha256_line: String,
    dir: PathBuf,
}

fn fixture(dir: &Path) -> Fixture {
    // The fake binary records every invocation; the installer is expected to
    // ask it for its version and, after an upgrade, to start its daemon.
    executable(
        &dir.join("codesnob"),
        concat!(
            "#!/bin/sh\n",
            "echo \"$@\" >> \"$HOME/calls.log\"\n",
            "if [ \"$1\" = \"--version\" ]; then echo \"codesnob 9.9.9\"; fi\n",
            "exit 0\n",
        ),
    );
    std::fs::write(dir.join("LICENSE"), "Apache-2.0\n").unwrap();
    let name = tarball_name();
    assert!(
        Command::new("tar")
            .current_dir(dir)
            .args(["-czf", &name, "codesnob", "LICENSE"])
            .status()
            .unwrap()
            .success()
    );
    let out = Command::new("sh")
        .current_dir(dir)
        .arg("-c")
        .arg(format!(
            "if command -v sha256sum >/dev/null; then sha256sum {name}; else shasum -a 256 {name}; fi"
        ))
        .output()
        .unwrap();
    std::fs::write(dir.join(format!("{name}.sha256")), &out.stdout).unwrap();
    Fixture {
        tarball: std::fs::read(dir.join(&name)).unwrap(),
        sha256_line: String::from_utf8(out.stdout).unwrap(),
        dir: dir.to_path_buf(),
    }
}

/// Serves the release URLs on an ephemeral port, recording every path hit.
/// With `private`, every URL answers 404, as GitHub does for a private repo.
fn serve(
    fixture: Arc<Fixture>,
    sha256_line: String,
    private: bool,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let log = hits.clone();
    let base_for_thread = base.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok() && !header.trim().is_empty() {
                header.clear();
            }
            log.lock().unwrap().push(path.clone());
            let tarball_path = format!("/releases/download/{TAG}/{}", tarball_name());
            let ok = |body: &[u8]| {
                let mut r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                r.extend_from_slice(body);
                r
            };
            let response: Vec<u8> = if private {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            } else if path == "/releases/latest" {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {base_for_thread}/releases/tag/{TAG}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes()
            } else if path == format!("/releases/tag/{TAG}") {
                ok(b"")
            } else if path == tarball_path {
                ok(&fixture.tarball)
            } else if path == format!("{tarball_path}.sha256") {
                ok(sha256_line.as_bytes())
            } else {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            };
            let _ = stream.write_all(&response);
        }
    });
    (base, hits)
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    home: PathBuf,
    scratch: PathBuf,
    _root: Dir,
}

/// A `gh` that fails unless `works` is given: then it serves the fixture the
/// way `gh release view` and `gh release download` would.
fn gh_shim(shims: &Path, works: Option<&Fixture>) {
    let body = match works {
        None => "#!/bin/sh\necho \"gh $*\" >> \"$HOME/gh.log\"\nexit 1\n".to_string(),
        Some(fx) => format!(
            concat!(
                "#!/bin/sh\n",
                "echo \"gh $*\" >> \"$HOME/gh.log\"\n",
                "case \"$1 $2\" in\n",
                "  'release view') echo {tag} ;;\n",
                "  'release download')\n",
                "    while [ $# -gt 0 ]; do\n",
                "      case \"$1\" in --pattern) p=\"$2\"; shift ;; --dir) d=\"$2\"; shift ;; esac; shift\n",
                "    done\n",
                "    cp \"{dir}/$p\" \"$d/$p\" ;;\n",
                "  *) exit 1 ;;\n",
                "esac\n",
            ),
            tag = TAG,
            dir = fx.dir.display()
        ),
    };
    executable(&shims.join("gh"), &body);
}

fn run_installer(
    name: &str,
    base: &str,
    env: &[(&str, &str)],
    gh: Option<&Fixture>,
    prepare: &dyn Fn(&Path),
) -> Run {
    let root = tempdir(&format!("run-{name}"));
    let home = root.join("home");
    let scratch = root.join("scratch");
    let shims = root.join("shims");
    for d in [&home, &scratch, &shims] {
        std::fs::create_dir_all(d).unwrap();
    }
    gh_shim(&shims, gh);
    prepare(&home);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let path = format!("{}:{}", shims.display(), std::env::var("PATH").unwrap());
    let mut cmd = Command::new("sh");
    cmd.arg(script)
        .env("HOME", &home)
        .env("TMPDIR", &scratch)
        .env("PATH", path)
        .env("CODESNOB_BASE_URL", base)
        .env_remove("CODESNOB_PREFIX")
        .env_remove("CODESNOB_VERSION")
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    Run {
        status: child.wait().unwrap(),
        stdout,
        stderr,
        home,
        scratch,
        _root: root,
    }
}

fn scratch_is_clean(run: &Run) -> bool {
    std::fs::read_dir(&run.scratch).unwrap().next().is_none()
}

/// What the installed binary was asked to do. The daemon start is backgrounded,
/// so wait briefly for `expect` to show up.
fn calls(run: &Run, expect: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let log = std::fs::read_to_string(run.home.join("calls.log")).unwrap_or_default();
        if expect.is_empty() || log.lines().any(|l| l == expect) || Instant::now() > deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn previous_install(home: &Path, version: &str) {
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("codesnob"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"codesnob {version}\"; fi\nexit 0\n"
        ),
    );
}

fn uname_shim(env_path: &Path, os: &str, arch: &str) -> String {
    let shims = env_path.join("uname-shims");
    std::fs::create_dir_all(&shims).unwrap();
    executable(
        &shims.join("uname"),
        &format!("#!/bin/sh\ncase \"$1\" in -m) echo {arch};; *) echo {os};; esac\n"),
    );
    format!("{}:{}", shims.display(), std::env::var("PATH").unwrap())
}

#[test]
fn installs_the_latest_release_into_local_bin() {
    let dir = tempdir("latest");
    let fx = Arc::new(fixture(&dir));
    let (base, hits) = serve(fx.clone(), fx.sha256_line.clone(), false);

    let run = run_installer("latest", &base, &[], None, &|_| {});

    assert!(run.status.success(), "stderr: {}", run.stderr);
    let binary = run.home.join(".local/bin/codesnob");
    let mode = std::fs::metadata(&binary)
        .expect("installed")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "and executable");
    assert!(run.home.join(".local/share/codesnob/LICENSE").exists());
    assert!(hits.lock().unwrap().iter().any(|p| p == "/releases/latest"));
    assert!(run.stdout.contains("installed"), "stdout: {}", run.stdout);
    assert!(
        run.stdout.contains("claude mcp add codesnob"),
        "names the next step"
    );
    assert!(
        !calls(&run, "").lines().any(|l| l == "daemon"),
        "no daemon on a fresh install"
    );
    assert!(
        !run.home.join("gh.log").exists(),
        "gh is not needed when curl works"
    );
    assert!(scratch_is_clean(&run), "the download scratch is cleaned up");
}

#[test]
fn a_private_repository_is_installed_through_gh() {
    let dir = tempdir("private");
    let fx = Arc::new(fixture(&dir));
    let (base, _) = serve(fx.clone(), fx.sha256_line.clone(), true);

    let run = run_installer("private", &base, &[], Some(&fx), &|_| {});

    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert!(run.home.join(".local/bin/codesnob").exists());
    let gh = std::fs::read_to_string(run.home.join("gh.log")).unwrap();
    assert!(
        gh.contains("gh release view --repo mme/codesnob"),
        "gh log: {gh}"
    );
    assert!(
        gh.contains("gh release download v9.9.9 --repo mme/codesnob"),
        "gh log: {gh}"
    );
}

#[test]
fn a_private_repository_without_gh_says_what_to_do() {
    let dir = tempdir("nogh");
    let fx = Arc::new(fixture(&dir));
    let (base, _) = serve(fx.clone(), fx.sha256_line.clone(), true);

    let run = run_installer("nogh", &base, &[], None, &|_| {});

    assert!(!run.status.success());
    assert!(
        run.stderr.contains("gh auth login"),
        "stderr: {}",
        run.stderr
    );
    assert!(!run.home.join(".local/bin/codesnob").exists());
}

#[test]
fn a_pinned_version_never_asks_what_is_latest() {
    let dir = tempdir("pinned");
    let fx = Arc::new(fixture(&dir));
    let (base, hits) = serve(fx.clone(), fx.sha256_line.clone(), false);

    let run = run_installer(
        "pinned",
        &base,
        &[("CODESNOB_VERSION", TAG)],
        None,
        &|_| {},
    );

    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert!(!hits.lock().unwrap().iter().any(|p| p == "/releases/latest"));
}

#[test]
fn a_wrong_checksum_installs_nothing_and_cleans_up() {
    let dir = tempdir("checksum");
    let fx = Arc::new(fixture(&dir));
    let lying = fx
        .sha256_line
        .replacen(|c: char| c.is_ascii_hexdigit(), "0", 4);
    let (base, _) = serve(fx, lying, false);

    let run = run_installer("checksum", &base, &[], None, &|_| {});

    assert!(!run.status.success(), "a bad checksum must refuse");
    assert!(
        run.stderr.contains("checksum mismatch"),
        "stderr: {}",
        run.stderr
    );
    assert!(!run.home.join(".local/bin/codesnob").exists());
    assert!(scratch_is_clean(&run));
}

#[test]
fn a_foreign_architecture_is_turned_away_before_downloading() {
    let dir = tempdir("arch");
    let fx = Arc::new(fixture(&dir));
    let (base, hits) = serve(fx.clone(), fx.sha256_line.clone(), false);
    let path = uname_shim(&dir, "Linux", "armv7l");

    let run = run_installer("arch", &base, &[("PATH", &path)], None, &|_| {});

    assert!(!run.status.success());
    assert!(
        run.stderr.contains("x86_64 Linux and Apple Silicon"),
        "stderr: {}",
        run.stderr
    );
    assert!(run.stderr.contains("build from source"));
    assert!(
        hits.lock().unwrap().is_empty(),
        "nothing is downloaded first"
    );
}

#[test]
fn an_intel_mac_is_turned_away_before_downloading() {
    let dir = tempdir("intel");
    let fx = Arc::new(fixture(&dir));
    let (base, hits) = serve(fx.clone(), fx.sha256_line.clone(), false);
    let path = uname_shim(&dir, "Darwin", "x86_64");

    let run = run_installer("intel", &base, &[("PATH", &path)], None, &|_| {});

    assert!(!run.status.success());
    assert!(
        run.stderr.contains("Apple Silicon only"),
        "stderr: {}",
        run.stderr
    );
    assert!(hits.lock().unwrap().is_empty());
}

#[test]
fn a_prefix_off_path_is_said_out_loud() {
    let dir = tempdir("prefix");
    let fx = Arc::new(fixture(&dir));
    let (base, _) = serve(fx.clone(), fx.sha256_line.clone(), false);
    let elsewhere = dir.join("elsewhere");

    let run = run_installer(
        "prefix",
        &base,
        &[("CODESNOB_PREFIX", elsewhere.to_str().unwrap())],
        None,
        &|_| {},
    );

    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert!(
        elsewhere.join("codesnob").exists(),
        "CODESNOB_PREFIX is honored"
    );
    assert!(run.stdout.contains("not on PATH"), "stdout: {}", run.stdout);
}

#[test]
fn an_upgrade_names_both_versions_and_replaces_the_old_daemon() {
    let dir = tempdir("upgrade");
    let fx = Arc::new(fixture(&dir));
    let (base, _) = serve(fx.clone(), fx.sha256_line.clone(), false);

    let run = run_installer("upgrade", &base, &[], None, &|home| {
        previous_install(home, "0.0.1")
    });

    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("upgraded codesnob 0.0.1 -> 9.9.9"),
        "stdout: {}",
        run.stdout
    );
    assert!(
        calls(&run, "daemon").lines().any(|l| l == "daemon"),
        "the new binary's daemon takes over the old one"
    );
}

#[test]
fn a_reinstall_of_the_same_version_leaves_the_daemon_alone() {
    let dir = tempdir("reinstall");
    let fx = Arc::new(fixture(&dir));
    let (base, _) = serve(fx.clone(), fx.sha256_line.clone(), false);

    let run = run_installer("reinstall", &base, &[], None, &|home| {
        previous_install(home, "9.9.9")
    });

    assert!(run.status.success(), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("reinstalled codesnob 9.9.9"),
        "stdout: {}",
        run.stdout
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!calls(&run, "").lines().any(|l| l == "daemon"));
}

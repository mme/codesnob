//! Pull requests: recognise what was typed, ask `gh` about the PR, and check
//! it out as a detached worktree in the cache. The PR's commits are fetched
//! into private refs of an existing clone of that repository when this
//! viewer has one open, otherwise into a blobless clone kept in the cache.
//! The diff base is the merge-base with the base branch GitHub reports, so
//! the view matches GitHub's Files tab.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::watch::Msg;

/// Which repository on which host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoId {
    fn same(&self, other: &RepoId) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A PR as typed: a URL, `owner/repo#123`, or `#123` (the current repo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub repo: Option<RepoId>,
    pub number: u64,
}

impl Spec {
    /// What to hand `gh pr view`.
    fn gh_args(&self) -> Vec<String> {
        let mut args = vec![self.number.to_string()];
        if let Some(r) = &self.repo {
            args.push("--repo".to_string());
            args.push(format!("{}/{}/{}", r.host, r.owner, r.name));
        }
        args
    }
}

pub fn parse(input: &str) -> Option<Spec> {
    let input = input.trim();
    if let Some(n) = input.strip_prefix('#') {
        return Some(Spec {
            repo: None,
            number: n.parse().ok()?,
        });
    }
    if let Some(rest) = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 5 && parts[3] == "pull" {
            return Some(Spec {
                repo: Some(RepoId {
                    host: parts[0].to_string(),
                    owner: parts[1].to_string(),
                    name: parts[2].to_string(),
                }),
                number: parts[4].split(['#', '?']).next()?.parse().ok()?,
            });
        }
        return None;
    }
    let (slug, n) = input.split_once('#')?;
    let (owner, name) = slug.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(Spec {
        repo: Some(RepoId {
            host: "github.com".to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
        }),
        number: n.parse().ok()?,
    })
}

/// `https://host/owner/repo(.git)`, `git@host:owner/repo(.git)`, or
/// `ssh://git@host/owner/repo(.git)`.
pub fn parse_remote_url(url: &str) -> Option<RepoId> {
    let url = url.trim();
    let rest = if let Some(r) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
    {
        let r = r.split_once('@').map(|(_, h)| h).unwrap_or(r);
        r.to_string()
    } else {
        let (user_host, path) = url.split_once(':')?;
        let host = user_host
            .split_once('@')
            .map(|(_, h)| h)
            .unwrap_or(user_host);
        format!("{host}/{path}")
    };
    let mut parts = rest.trim_end_matches('/').splitn(3, '/');
    let host = parts.next()?.split(':').next()?.to_string();
    let owner = parts.next()?.to_string();
    let name = parts.next()?.trim_end_matches(".git").to_string();
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some(RepoId {
        host,
        owner,
        name,
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Oid {
    pub oid: String,
}

/// What `gh pr view --json` reports.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub url: String,
    pub base_ref_name: String,
    #[serde(default)]
    pub head_ref_name: String,
    pub head_ref_oid: String,
    #[serde(default)]
    pub is_draft: bool,
    /// OPEN, CLOSED or MERGED.
    pub state: String,
    pub author: Option<Author>,
    /// For a merged PR: the commit that landed it on the base branch.
    #[serde(default)]
    pub merge_commit: Option<Oid>,
}

impl Info {
    pub fn repo(&self) -> Option<RepoId> {
        parse(&self.url)?.repo
    }
}

/// Where PR details come from: `gh`, or a stand-in in tests.
pub trait Source: Send + Sync {
    fn view(&self, spec: &Spec, cwd: Option<&Path>) -> Result<Info, String>;
}

pub struct Gh;

impl Source for Gh {
    fn view(&self, spec: &Spec, cwd: Option<&Path>) -> Result<Info, String> {
        let mut cmd = Command::new("gh");
        cmd.arg("pr").arg("view").args(spec.gh_args()).args([
            "--json",
            "number,title,body,url,baseRefName,headRefName,headRefOid,isDraft,state,author,mergeCommit",
        ]);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let out = cmd.output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "gh is not installed".to_string(),
            _ => format!("gh failed: {e}"),
        })?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("gh failed");
            return Err(if line.contains("auth login") {
                "gh is not logged in".to_string()
            } else {
                line.trim().to_string()
            });
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected gh output: {e}"))
    }
}

pub enum Progress {
    /// The PR's details are known; fetching continues.
    Info(Info),
    Ready {
        info: Info,
        /// The worktree to show.
        dir: PathBuf,
        /// The ref the diff base is the merge-base with.
        base: String,
        /// Something worth saying, e.g. that local edits kept the old head.
        note: Option<String>,
    },
    Failed(String),
}

/// Where PR checkouts and clones live: `$XDG_CACHE_HOME/codesnob`, else
/// `~/.cache/codesnob`, with `-dev` for debug builds. Never `/tmp`, which
/// can be RAM-backed.
pub fn default_cache() -> Option<PathBuf> {
    let name = if cfg!(debug_assertions) {
        "codesnob-dev"
    } else {
        "codesnob"
    };
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join(name))
}

/// Look the PR up and check it out on a background thread, reporting
/// progress to location `loc`.
pub fn spawn_fetch(
    spec: Spec,
    cwd: Option<PathBuf>,
    clones: Vec<PathBuf>,
    cache: PathBuf,
    source: Arc<dyn Source>,
    tx: Sender<Msg>,
    loc: u64,
) {
    std::thread::spawn(move || {
        let send = |p: Progress| {
            let _ = tx.send(Msg::Pr {
                loc,
                progress: Box::new(p),
            });
        };
        let info = match source.view(&spec, cwd.as_deref()) {
            Ok(i) => i,
            Err(e) => return send(Progress::Failed(e)),
        };
        send(Progress::Info(info.clone()));
        match checkout(&info, &clones, &cache) {
            Ok((dir, base, note)) => send(Progress::Ready {
                info,
                dir,
                base,
                note,
            }),
            Err(e) => send(Progress::Failed(e)),
        }
    });
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git failed: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("git failed");
        Err(line.trim().trim_start_matches("fatal: ").to_string())
    }
}

/// A clone of `repo` among `clones`, and the name of its remote for it.
fn find_clone(repo: &RepoId, clones: &[PathBuf]) -> Option<(PathBuf, String)> {
    for dir in clones {
        let Ok(out) = git(dir, &["config", "--get-regexp", r"^remote\..*\.url$"]) else {
            continue;
        };
        for line in out.lines() {
            let Some((key, url)) = line.split_once(' ') else {
                continue;
            };
            let Some(remote) = key
                .strip_prefix("remote.")
                .and_then(|k| k.strip_suffix(".url"))
            else {
                continue;
            };
            if parse_remote_url(url).is_some_and(|r| r.same(repo)) {
                return Some((dir.clone(), remote.to_string()));
            }
        }
    }
    None
}

fn checkout_dir(cache: &Path, repo: &RepoId, number: u64) -> PathBuf {
    cache
        .join("pr")
        .join(&repo.host)
        .join(&repo.owner)
        .join(&repo.name)
        .join(number.to_string())
}

/// The repository behind an existing checkout.
fn common_dir(dir: &Path) -> Option<PathBuf> {
    let out = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    Some(PathBuf::from(out.trim()))
}

fn checkout(
    info: &Info,
    clones: &[PathBuf],
    cache: &Path,
) -> Result<(PathBuf, String, Option<String>), String> {
    let repo = info
        .repo()
        .ok_or_else(|| format!("cannot read the repository from {}", info.url))?;
    let n = info.number;
    // Checkouts share the cache; a prune neither runs during one nor races it.
    let _cache = lock(&cache.join(".lock"), false, true);

    let dir = checkout_dir(cache, &repo, n);
    // Fetch into the repository the checkout already lives in, so the
    // checkout sees the new head; otherwise into a clone open here, else the
    // cache's own clone.
    let existing = if dir.join(".git").exists() {
        let found = common_dir(&dir).and_then(|c| find_clone(&repo, &[c]));
        if found.is_none() {
            remove_checkout(&dir);
        }
        found
    } else {
        None
    };
    let fresh = existing.is_none();
    let (clone, remote) = match existing {
        Some(found) => found,
        None => {
            let cache_clone = cache
                .join("repos")
                .join(&repo.host)
                .join(&repo.owner)
                .join(&repo.name);
            let mut candidates = clones.to_vec();
            candidates.push(cache_clone.clone());
            match find_clone(&repo, &candidates) {
                Some(found) => found,
                None => {
                    std::fs::create_dir_all(cache_clone.parent().unwrap_or(cache))
                        .map_err(|e| e.to_string())?;
                    let out = Command::new("gh")
                        .args(["repo", "clone"])
                        .arg(format!("{}/{}/{}", repo.host, repo.owner, repo.name))
                        .arg(&cache_clone)
                        .args(["--", "--filter=blob:none", "--no-checkout", "--quiet"])
                        .output()
                        .map_err(|e| format!("gh failed: {e}"))?;
                    if !out.status.success() {
                        let err = String::from_utf8_lossy(&out.stderr);
                        return Err(err.lines().last().unwrap_or("clone failed").to_string());
                    }
                    (cache_clone, "origin".to_string())
                }
            }
        }
    };

    let head_ref = format!("refs/codesnob/pr/{n}");
    let base_ref = format!("refs/codesnob/pr/{n}-base");
    let fetch = |refspec: &str| git(&clone, &["fetch", "--quiet", "--no-tags", &remote, refspec]);
    fetch(&format!("+refs/pull/{n}/head:{head_ref}"))?;
    // The base branch may be gone, typically after a stacked PR merged.
    let base_fetched = fetch(&format!("+refs/heads/{}:{base_ref}", info.base_ref_name)).is_ok();

    // A merged PR is already part of its base branch, so diffing against the
    // branch shows nothing. Start from the branch as it was just before the
    // merge instead, which is what GitHub shows.
    let mut based = base_fetched;
    if info.state == "MERGED"
        && let Some(m) = &info.merge_commit
    {
        let parent = format!("{}^1", m.oid);
        if git(&clone, &["rev-parse", "--verify", "--quiet", &parent]).is_err() {
            let _ = fetch(&m.oid);
        }
        if let Ok(before) = git(&clone, &["rev-parse", "--verify", "--quiet", &parent]) {
            git(&clone, &["update-ref", &base_ref, before.trim()])?;
            based = true;
        }
    }
    if !based {
        return Err(format!(
            "the base branch {} no longer exists",
            info.base_ref_name
        ));
    }

    let mut note = None;
    let fetched = git(&clone, &["rev-parse", &head_ref]).unwrap_or_default();
    if !info.head_ref_oid.is_empty() && fetched.trim() != info.head_ref_oid {
        note = Some(format!("#{n} moved while fetching; r fetches again"));
    }
    if !fresh {
        if let Err(e) = git(&dir, &["checkout", "--quiet", "--detach", &head_ref]) {
            note = Some(format!(
                "local edits kept the checkout at its old head ({e})"
            ));
        }
    } else {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(dir.parent().unwrap_or(cache)).map_err(|e| e.to_string())?;
        let path = dir.to_string_lossy().into_owned();
        // --force: a checkout deleted by hand can still be registered.
        git(
            &clone,
            &[
                "worktree", "add", "--quiet", "--force", "--detach", &path, &head_ref,
            ],
        )?;
    }
    touch(&dir);
    let dir = std::fs::canonicalize(&dir).map_err(|e| e.to_string())?;
    Ok((dir, base_ref, note))
}

/// Next to each checkout, `<n>.used` records when it was last opened, and a
/// viewer showing the checkout holds a shared lock on it.
fn marker(dir: &Path) -> PathBuf {
    let mut m = dir.as_os_str().to_owned();
    m.push(".used");
    PathBuf::from(m)
}

fn touch(dir: &Path) {
    let _ = std::fs::write(marker(dir), b"");
}

/// An advisory file lock, released when dropped.
pub struct Hold(#[allow(dead_code)] std::fs::File);

fn lock(path: &Path, exclusive: bool, wait: bool) -> Option<Hold> {
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let mut op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if !wait {
        op |= libc::LOCK_NB;
    }
    // SAFETY: flock on a descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
    (rc == 0).then_some(Hold(file))
}

/// Keep the checkout at `dir` from being pruned, by any viewer, while held.
pub fn hold(dir: &Path) -> Option<Hold> {
    lock(&marker(dir), false, true)
}

/// Remove PR checkouts not opened within `unused_for` and not open in any
/// viewer. Returns how many were removed.
pub fn prune(cache: &Path, unused_for: Duration) -> usize {
    // Never during a checkout; another time will do.
    let Some(_cache) = lock(&cache.join(".lock"), true, false) else {
        return 0;
    };
    let mut removed = 0;
    let level = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    // pr/<host>/<owner>/<name>/<n> with <n>.used beside it.
    for host in level(&cache.join("pr")) {
        for owner in level(&host) {
            for name in level(&owner) {
                for path in level(&name) {
                    let file = path.file_name().map(|f| f.to_string_lossy().into_owned());
                    let Some(n) = file.as_deref().and_then(|f| f.strip_suffix(".used")) else {
                        continue;
                    };
                    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                        continue;
                    }
                    let dir = name.join(n);
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .unwrap_or(Duration::MAX);
                    if age < unused_for {
                        continue;
                    }
                    // A viewer showing it holds a shared lock.
                    let Some(_open) = lock(&path, true, false) else {
                        continue;
                    };
                    remove_checkout(&dir);
                    let _ = std::fs::remove_file(&path);
                    remove_empty_parents(&dir, cache);
                    removed += 1;
                }
            }
        }
    }
    prune_clones(cache);
    removed
}

/// Clones in the cache exist only to back PR checkouts; one with none left
/// goes too, and is cloned again when needed. Runs under the cache lock.
fn prune_clones(cache: &Path) {
    let repos = cache.join("repos");
    let Ok(hosts) = std::fs::read_dir(&repos) else {
        return;
    };
    for host in hosts.flatten() {
        for owner in std::fs::read_dir(host.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            for clone in std::fs::read_dir(owner.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let dir = clone.path();
                let Ok(list) = git(&dir, &["worktree", "list", "--porcelain"]) else {
                    continue;
                };
                let linked = list.lines().filter(|l| l.starts_with("worktree ")).count();
                if linked <= 1 {
                    let _ = std::fs::remove_dir_all(&dir);
                    remove_empty_parents(&dir, cache);
                }
            }
        }
    }
}

/// Remove now-empty directories above `dir`, stopping at `cache`.
fn remove_empty_parents(dir: &Path, cache: &Path) {
    let mut d = dir.parent();
    while let Some(p) = d {
        if p == cache || !p.starts_with(cache) || std::fs::remove_dir(p).is_err() {
            break;
        }
        d = p.parent();
    }
}

fn remove_checkout(dir: &Path) {
    if let Ok(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        let common = PathBuf::from(common.trim());
        let n = dir.file_name().map(|n| n.to_string_lossy().into_owned());
        let path = dir.to_string_lossy().into_owned();
        let _ = Command::new("git")
            .arg("--git-dir")
            .arg(&common)
            .args(["worktree", "remove", "--force", &path])
            .output();
        if let Some(n) = n {
            for r in [
                format!("refs/codesnob/pr/{n}"),
                format!("refs/codesnob/pr/{n}-base"),
            ] {
                let _ = Command::new("git")
                    .arg("--git-dir")
                    .arg(&common)
                    .args(["update-ref", "-d", &r])
                    .output();
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh_repo(owner: &str, name: &str) -> Option<RepoId> {
        Some(RepoId {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    #[test]
    fn what_counts_as_a_pr() {
        assert_eq!(
            parse("#42"),
            Some(Spec {
                repo: None,
                number: 42
            })
        );
        assert_eq!(
            parse("https://github.com/acme/widget/pull/7/files").map(|s| (s.repo, s.number)),
            Some((gh_repo("acme", "widget"), 7))
        );
        assert_eq!(
            parse("acme/widget#9").map(|s| (s.repo, s.number)),
            Some((gh_repo("acme", "widget"), 9))
        );
        assert_eq!(parse("~/Projects/x"), None);
        assert_eq!(parse("src/lib.rs"), None);
        assert_eq!(parse("https://github.com/acme/widget"), None);
    }

    #[test]
    fn prune_only_reads_numbered_markers() {
        let cache = crate::testutil::fresh_dir("pr-markers");
        let repo = cache.join("pr/github.com/acme/foo");
        std::fs::create_dir_all(repo.join("keep-me")).unwrap();
        std::fs::create_dir_all(cache.join("pr/github.com/acme/foo.used")).unwrap();
        assert_eq!(prune(&cache, Duration::ZERO), 0);
        assert!(
            repo.join("keep-me").exists(),
            "a repo named foo.used is not a marker"
        );
        let _ = std::fs::remove_dir_all(cache);
    }

    #[test]
    fn remote_urls_name_their_repository() {
        for url in [
            "https://github.com/acme/widget.git",
            "git@github.com:acme/widget.git",
            "ssh://git@github.com/acme/widget",
            "https://github.com/acme/widget/",
        ] {
            assert_eq!(parse_remote_url(url), gh_repo("acme", "widget"), "{url}");
        }
        assert_eq!(parse_remote_url("/srv/git/widget"), None);
    }
}

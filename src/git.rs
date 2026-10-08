//! Repository access: find the branch point, list the branch's commits, and
//! collect the changed files of a range. Every range is two sides, each a
//! commit tree or the working tree, and goes through the same pipeline.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use gix::bstr::{BString, ByteSlice};
use gix::hash::ObjectId;

/// One changed path. `None` on a side means the file does not exist there.
pub struct FileChange {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

/// Which two states a view compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Range {
    /// The branch point (or `--base`) against the working tree.
    Everything,
    /// HEAD against the working tree.
    Uncommitted,
    /// One commit against its first parent.
    Commit(ObjectId),
    /// Any two states, as a comparison like `main...feature` or `staged`.
    Compare(Side, Side),
}

/// One side of a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Rev(ObjectId),
    /// The staging area.
    Index,
    Worktree,
}

/// What a typed comparison means.
#[derive(Debug, PartialEq, Eq)]
pub enum Spec {
    /// A range to show, with words for it.
    Range(Range, String),
    /// A branch or tag on its own: everything since the branch point with it.
    Base(String),
}

/// Read a comparison the way git would, in the repository at `dir`:
/// `worktree`, `staged`, `unstaged`, `a..b` (a against b), `a...b` (b since
/// it diverged from a), a branch or tag (everything since the branch point
/// with it), or any other revision (that commit against its parent).
pub fn parse_spec(dir: &Path, spec: &str) -> Result<Spec> {
    let repo = gix::discover(dir)
        .with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    let s = spec.trim();
    let head = || commit_of(&repo, "HEAD");
    let side = |rev: &str| {
        if rev.is_empty() {
            head()
        } else {
            commit_of(&repo, rev)
        }
    };
    Ok(match s {
        "" => anyhow::bail!("nothing to compare"),
        "worktree" => Spec::Range(Range::Uncommitted, "uncommitted changes".to_string()),
        "staged" => Spec::Range(
            Range::Compare(Side::Rev(head()?), Side::Index),
            "staged changes".to_string(),
        ),
        "unstaged" => Spec::Range(
            Range::Compare(Side::Index, Side::Worktree),
            "unstaged changes".to_string(),
        ),
        _ => {
            if let Some((a, b)) = s.split_once("...") {
                let (a, b) = (side(a)?, side(b)?);
                let base = repo
                    .merge_base(a, b)
                    .with_context(|| format!("{s}: the two sides share no history"))?
                    .detach();
                Spec::Range(Range::Compare(Side::Rev(base), Side::Rev(b)), s.to_string())
            } else if let Some((a, b)) = s.split_once("..") {
                Spec::Range(
                    Range::Compare(Side::Rev(side(a)?), Side::Rev(side(b)?)),
                    s.to_string(),
                )
            } else if s != "HEAD" && repo.try_find_reference(s).ok().flatten().is_some() {
                Spec::Base(s.to_string())
            } else {
                Spec::Range(Range::Commit(commit_of(&repo, s)?), s.to_string())
            }
        }
    })
}

fn commit_of(repo: &gix::Repository, rev: &str) -> Result<ObjectId> {
    Ok(repo
        .rev_parse_single(rev)
        .with_context(|| format!("cannot resolve {rev:?}"))?
        .object()?
        .peel_to_commit()
        .with_context(|| format!("{rev:?} is not a commit"))?
        .id)
}

/// Branch, remote branch and tag names, for completing comparisons.
pub fn ref_names(dir: &Path) -> Vec<String> {
    let Ok(repo) = gix::discover(dir) else {
        return Vec::new();
    };
    let Ok(refs) = repo.references() else {
        return Vec::new();
    };
    let mut names: Vec<String> = refs
        .all()
        .into_iter()
        .flatten()
        .flatten()
        .map(|r| r.name().shorten().to_string())
        .filter(|n| !n.starts_with("refs/") && n != "HEAD" && !n.ends_with("/HEAD"))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub short: String,
    pub subject: String,
    /// The full message, subject included.
    pub message: String,
    pub author: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
}

/// What the branch is compared against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub commit: Option<ObjectId>,
    /// The ref the branch point was found on, e.g. "main". `None` when no
    /// base was found and the view falls back to HEAD.
    pub name: Option<String>,
    /// One sentence on how the base was chosen.
    pub reason: String,
}

pub struct Snapshot {
    /// What HEAD is: a branch name, or "detached 1a2b3c4".
    pub head_label: String,
    /// The repository has no commits yet.
    pub unborn: bool,
    pub base: Base,
    /// Commits between the base and HEAD, oldest first, at most `MAX_COMMITS`.
    pub commits: Vec<CommitInfo>,
    /// More commits exist than `commits` holds; the oldest were dropped.
    pub commits_truncated: bool,
    /// The working tree differs from HEAD, untracked files included.
    pub dirty: bool,
    pub range: Range,
    /// For `Range::Commit`: the commit shown.
    pub commit: Option<CommitInfo>,
    pub files: Vec<FileChange>,
}

impl Snapshot {
    /// Cheap identity of the snapshot's content, to skip no-op refreshes.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.head_label.hash(&mut h);
        self.base.commit.hash(&mut h);
        // A different base on the same commit still changes the words shown.
        self.base.name.hash(&mut h);
        self.range.hash(&mut h);
        self.dirty.hash(&mut h);
        for c in &self.commits {
            c.id.hash(&mut h);
        }
        for f in &self.files {
            f.old_path.hash(&mut h);
            f.new_path.hash(&mut h);
            f.old.hash(&mut h);
            f.new.hash(&mut h);
        }
        h.finish()
    }
}

/// The most commits listed for one branch.
pub const MAX_COMMITS: usize = 500;

const DEFAULT_BRANCHES: &[&str] = &["main", "master", "develop", "trunk"];

pub fn snapshot(dir: &Path, base_override: Option<&str>, range: &Range) -> Result<Snapshot> {
    let repo = gix::discover(dir)
        .with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    let workdir = repo
        .workdir()
        .context("bare repositories have no working tree to diff")?
        .to_path_buf();

    let head_id = repo.head_id().ok().map(|id| id.detach());
    let head_label = head_label(&repo, head_id);
    let base = match base_override {
        Some(spec) => explicit_base(&repo, head_id, spec)?,
        None => detect_base(&repo, head_id)?,
    };
    let (commits, commits_truncated) = match (base.commit, head_id) {
        (base_commit, Some(head)) if base_commit != Some(head) => {
            commits_between(&repo, head, base_commit)?
        }
        _ => (Vec::new(), false),
    };

    let head_tree = match head_id {
        Some(id) => repo.find_commit(id)?.tree()?,
        None => repo.empty_tree(),
    };
    let uncommitted = status_paths(&repo, None, Want::ALL)?;
    let dirty = !uncommitted.is_empty();

    let mut commit = None;
    let files = match range {
        Range::Everything => {
            let base_tree = tree_of(&repo, base.commit)?;
            let mut paths = uncommitted;
            tree_paths(&repo, &base_tree, &head_tree, &mut paths)?;
            collect(
                &repo,
                &paths,
                &Reader::Tree(&base_tree),
                &Reader::Worktree(&workdir),
            )?
        }
        Range::Uncommitted => collect(
            &repo,
            &uncommitted,
            &Reader::Tree(&head_tree),
            &Reader::Worktree(&workdir),
        )?,
        Range::Commit(id) => {
            let c = repo
                .find_commit(*id)
                .with_context(|| format!("commit {} not found", short(id)))?;
            let parent = c.parent_ids().next().map(|p| p.detach());
            let new_tree = c.tree()?;
            let old_tree = tree_of(&repo, parent)?;
            commit = Some(commit_info(&c));
            let mut paths = BTreeSet::new();
            tree_paths(&repo, &old_tree, &new_tree, &mut paths)?;
            collect(
                &repo,
                &paths,
                &Reader::Tree(&old_tree),
                &Reader::Tree(&new_tree),
            )?
        }
        Range::Compare(from, to) => {
            let index = repo.index_or_empty()?;
            let state: &gix::index::State = &index;
            let tree = |side: &Side| -> Result<Option<gix::Tree<'_>>> {
                Ok(match side {
                    Side::Rev(id) => Some(tree_of(&repo, Some(*id))?),
                    _ => None,
                })
            };
            let (old_tree, new_tree) = (tree(from)?, tree(to)?);
            let paths = match (from, to, &old_tree, &new_tree) {
                (Side::Rev(_), Side::Rev(_), Some(a), Some(b)) => {
                    let mut paths = BTreeSet::new();
                    tree_paths(&repo, a, b, &mut paths)?;
                    paths
                }
                (Side::Rev(_), Side::Index, Some(a), _) => {
                    status_paths(&repo, Some(a.id), Want::STAGED)?
                }
                (Side::Index, Side::Worktree, ..) => status_paths(&repo, None, Want::UNSTAGED)?,
                (Side::Rev(_), Side::Worktree, Some(a), _) => {
                    status_paths(&repo, Some(a.id), Want::ALL)?
                }
                _ => anyhow::bail!("cannot compare {from:?} with {to:?}"),
            };
            let reader = |side: &Side, tree: &Option<gix::Tree<'_>>| -> Result<Reader<'_>> {
                Ok(match (side, tree) {
                    (Side::Rev(_), Some(_)) => unreachable!("trees are read below"),
                    (Side::Index, _) => Reader::Index(state),
                    (Side::Worktree, _) => Reader::Worktree(&workdir),
                    (Side::Rev(_), None) => anyhow::bail!("missing tree"),
                })
            };
            let old = match &old_tree {
                Some(t) => Reader::Tree(t),
                None => reader(from, &old_tree)?,
            };
            let new = match &new_tree {
                Some(t) => Reader::Tree(t),
                None => reader(to, &new_tree)?,
            };
            collect(&repo, &paths, &old, &new)?
        }
    };

    Ok(Snapshot {
        head_label,
        unborn: head_id.is_none(),
        base,
        commits,
        commits_truncated,
        dirty,
        range: *range,
        commit,
        files,
    })
}

fn tree_of(repo: &gix::Repository, commit: Option<ObjectId>) -> Result<gix::Tree<'_>> {
    Ok(match commit {
        Some(id) => repo.find_commit(id)?.tree()?,
        None => repo.empty_tree(),
    })
}

fn commit_info(c: &gix::Commit<'_>) -> CommitInfo {
    let message = c.message_raw_sloppy().to_str_lossy().trim_end().to_string();
    let subject = message.lines().next().unwrap_or("").to_string();
    let (author, time) = match c.author() {
        Ok(a) => (
            a.name.to_str_lossy().into_owned(),
            a.time().map(|t| t.seconds).unwrap_or(0),
        ),
        Err(_) => (String::new(), 0),
    };
    CommitInfo {
        id: c.id,
        short: short(&c.id),
        subject,
        message,
        author,
        time,
    }
}

/// Commits reachable from `head` but not from `base`, oldest first.
fn commits_between(
    repo: &gix::Repository,
    head: ObjectId,
    base: Option<ObjectId>,
) -> Result<(Vec<CommitInfo>, bool)> {
    let walk = repo.rev_walk([head]).with_hidden(base).all()?;
    let mut out = Vec::new();
    let mut truncated = false;
    for info in walk {
        let info = info?;
        if out.len() == MAX_COMMITS {
            truncated = true;
            break;
        }
        out.push(commit_info(&info.object()?));
    }
    out.reverse();
    Ok((out, truncated))
}

fn head_label(repo: &gix::Repository, head_id: Option<ObjectId>) -> String {
    match repo.head_name().ok().flatten() {
        Some(name) => name.shorten().to_string(),
        None => match head_id {
            Some(id) => format!("detached {}", short(&id)),
            None => "unborn".to_string(),
        },
    }
}

fn short(id: &ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}

fn explicit_base(repo: &gix::Repository, head_id: Option<ObjectId>, spec: &str) -> Result<Base> {
    let target = repo
        .rev_parse_single(spec)
        .with_context(|| format!("cannot resolve base revision {spec:?}"))?
        .detach();
    let commit = match head_id {
        Some(head) => repo
            .merge_base(head, target)
            .map(|id| id.detach())
            .unwrap_or(target),
        None => target,
    };
    Ok(Base {
        commit: Some(commit),
        name: Some(spec.to_string()),
        reason: format!("{spec} was given with --base"),
    })
}

struct Candidate {
    name: String,
    id: ObjectId,
    reason: &'static str,
    /// Whether `name` is a real ref rather than an expression like `HEAD~1`.
    is_ref: bool,
}

fn detect_base(repo: &gix::Repository, head_id: Option<ObjectId>) -> Result<Base> {
    let Some(head) = head_id else {
        return Ok(Base {
            commit: None,
            name: None,
            reason: "the repository has no commits yet, so every file is new".to_string(),
        });
    };

    let branch = repo.head_name()?.map(|n| n.shorten().to_string());
    let Some(branch) = branch else {
        return Ok(Base {
            commit: Some(head),
            name: None,
            reason: "HEAD is detached, so only uncommitted changes are shown".to_string(),
        });
    };
    let on_default_branch = DEFAULT_BRANCHES.contains(&branch.as_str());

    let mut candidates: Vec<Candidate> = Vec::new();
    let push = |candidates: &mut Vec<Candidate>, name: &str, reason: &'static str| {
        if candidates.iter().any(|c| c.name == name) {
            return;
        }
        let Ok(id) = repo.rev_parse_single(name) else {
            return;
        };
        let id = id.detach();
        // Skip refs that are this very branch (or its own remote copy), unless we
        // are sitting on a default branch and want to compare against its upstream.
        let bare = name.rsplit_once('/').map(|(_, b)| b).unwrap_or(name);
        if bare == branch && !on_default_branch {
            return;
        }
        if name == branch {
            return;
        }
        let is_ref = repo.try_find_reference(name).ok().flatten().is_some();
        candidates.push(Candidate {
            name: name.to_string(),
            id,
            reason,
            is_ref,
        });
    };

    // 1. The reflog records "branch: Created from <ref>".
    if let Some(from) = reflog_origin(repo)
        && from != "HEAD"
    {
        push(&mut candidates, &from, "reflog");
    }

    // 2. The configured upstream.
    if let Ok(Some(head_ref)) = repo.head_ref()
        && let Some(Ok(tracking)) =
            repo.branch_remote_tracking_ref_name(head_ref.name(), gix::remote::Direction::Fetch)
    {
        push(&mut candidates, &tracking.shorten().to_string(), "upstream");
    }

    // 3. Well-known names, local and on origin.
    push(&mut candidates, "origin/HEAD", "default branch");
    for b in DEFAULT_BRANCHES {
        push(&mut candidates, b, "default branch");
    }
    for b in DEFAULT_BRANCHES {
        push(&mut candidates, &format!("origin/{b}"), "default branch");
    }

    // Pick the candidate whose merge-base with HEAD is the most recent commit:
    // that is the closest branch point. On ties a real ref name beats an
    // expression, then the earlier (higher priority) candidate wins.
    let mut best: Option<(i64, ObjectId, &Candidate)> = None;
    for cand in &candidates {
        let Ok(mb) = repo.merge_base(head, cand.id) else {
            continue;
        };
        let mb = mb.detach();
        let time = repo
            .find_commit(mb)
            .ok()
            .and_then(|c| c.time().ok())
            .map(|t| t.seconds)
            .unwrap_or(0);
        let better = match &best {
            None => true,
            Some((t, _, c)) => time > *t || (time == *t && cand.is_ref && !c.is_ref),
        };
        if better {
            best = Some((time, mb, cand));
        }
    }

    match best {
        Some((_, mb, cand)) => {
            let name = if cand.name == "origin/HEAD" {
                resolve_symbolic(repo, "refs/remotes/origin/HEAD")
                    .unwrap_or_else(|| cand.name.clone())
            } else {
                cand.name.clone()
            };
            let how = match cand.reason {
                "reflog" => format!("{branch} was created from {name}, as its reflog records"),
                "upstream" => format!("{name} is the upstream {branch} tracks"),
                _ => format!("{name} is the default branch"),
            };
            Ok(Base {
                commit: Some(mb),
                name: Some(name.clone()),
                reason: format!("{how}; they diverged at {}", short(&mb)),
            })
        }
        None => Ok(Base {
            commit: Some(head),
            name: None,
            reason: "no base branch was found, so only uncommitted changes are shown".to_string(),
        }),
    }
}

fn resolve_symbolic(repo: &gix::Repository, name: &str) -> Option<String> {
    let r = repo.find_reference(name).ok()?;
    match r.target() {
        gix::refs::TargetRef::Symbolic(n) => Some(n.shorten().to_string()),
        _ => None,
    }
}

fn reflog_origin(repo: &gix::Repository) -> Option<String> {
    let head_ref = repo.head_ref().ok()??;
    let mut platform = head_ref.log_iter();
    let mut lines = platform.all().ok()??;
    let first = lines.next()?.ok()?;
    let msg = first.message.to_str_lossy();
    let from = msg.strip_prefix("branch: Created from ")?.trim();
    if from.is_empty() {
        return None;
    }
    Some(from.to_string())
}

/// Paths where the working tree differs from HEAD: staged, unstaged and untracked.
/// Which parts of `git status` to collect paths from.
struct Want {
    /// The tree against the index: staged changes.
    tree_index: bool,
    /// The index against the working tree: unstaged changes.
    index_worktree: bool,
    untracked: bool,
}

impl Want {
    const ALL: Want = Want {
        tree_index: true,
        index_worktree: true,
        untracked: true,
    };
    const STAGED: Want = Want {
        tree_index: true,
        index_worktree: false,
        untracked: false,
    };
    const UNSTAGED: Want = Want {
        tree_index: false,
        index_worktree: true,
        untracked: false,
    };
}

/// Paths `git status` reports, comparing the index with `tree` (HEAD when
/// `None`) and the working tree with the index.
fn status_paths(
    repo: &gix::Repository,
    tree: Option<ObjectId>,
    want: Want,
) -> Result<BTreeSet<BString>> {
    use gix::diff::index::Change as TreeChange;
    use gix::status::Item;
    use gix::status::index_worktree::Item as WtItem;

    let mut paths = BTreeSet::new();
    let mut platform = repo
        .status(gix::progress::Discard)?
        .untracked_files(if want.untracked {
            gix::status::UntrackedFiles::Files
        } else {
            gix::status::UntrackedFiles::None
        })
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled);
    if let Some(tree) = tree {
        platform = platform.head_tree(tree);
    }
    for item in platform.into_iter(Vec::<BString>::new())? {
        match item? {
            Item::TreeIndex(change) if want.tree_index => match change {
                TreeChange::Addition { location, .. }
                | TreeChange::Deletion { location, .. }
                | TreeChange::Modification { location, .. } => {
                    paths.insert(location.into_owned());
                }
                TreeChange::Rewrite {
                    source_location,
                    location,
                    ..
                } => {
                    paths.insert(source_location.into_owned());
                    paths.insert(location.into_owned());
                }
            },
            Item::IndexWorktree(item) if want.index_worktree || want.untracked => match &item {
                WtItem::DirectoryContents { entry, .. }
                    if want.untracked && entry.status == gix::dir::entry::Status::Untracked =>
                {
                    paths.insert(entry.rela_path.clone());
                }
                WtItem::Modification { rela_path, .. } if want.index_worktree => {
                    paths.insert(rela_path.clone());
                }
                WtItem::Rewrite {
                    source,
                    dirwalk_entry,
                    ..
                } if want.index_worktree => {
                    paths.insert(source.rela_path().to_owned());
                    paths.insert(dirwalk_entry.rela_path.clone());
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(paths)
}

/// Add the files that differ between two trees.
fn tree_paths(
    repo: &gix::Repository,
    old: &gix::Tree<'_>,
    new: &gix::Tree<'_>,
    out: &mut BTreeSet<BString>,
) -> Result<()> {
    use gix::object::tree::diff::ChangeDetached as C;
    if old.id == new.id {
        return Ok(());
    }
    for change in repo.diff_tree_to_tree(Some(old), Some(new), None)? {
        match change {
            C::Addition {
                location,
                entry_mode,
                ..
            }
            | C::Deletion {
                location,
                entry_mode,
                ..
            } => {
                if entry_mode.is_blob_or_symlink() {
                    out.insert(location);
                }
            }
            C::Modification {
                location,
                entry_mode,
                previous_entry_mode,
                ..
            } => {
                if entry_mode.is_blob_or_symlink() || previous_entry_mode.is_blob_or_symlink() {
                    out.insert(location);
                }
            }
            C::Rewrite {
                source_location,
                location,
                entry_mode,
                ..
            } => {
                if entry_mode.is_blob_or_symlink() {
                    out.insert(source_location);
                    out.insert(location);
                }
            }
        }
    }
    Ok(())
}

/// Where one side of a comparison reads file contents from.
enum Reader<'a> {
    Tree(&'a gix::Tree<'a>),
    Index(&'a gix::index::State),
    Worktree(&'a Path),
}

impl Reader<'_> {
    /// The file at `path` on this side, with its blob id when known.
    fn read(
        &self,
        repo: &gix::Repository,
        path: &BString,
    ) -> Result<Option<(Option<ObjectId>, Vec<u8>)>> {
        Ok(match self {
            Reader::Tree(tree) => blob_at(repo, tree, path)?.map(|(id, d)| (Some(id), d)),
            Reader::Index(state) => match state.entry_by_path(path.as_bstr()) {
                Some(e) if e.mode != gix::index::entry::Mode::COMMIT => {
                    let id = e.id;
                    Some((Some(id), repo.find_blob(id)?.take_data()))
                }
                _ => None,
            },
            Reader::Worktree(workdir) => {
                read_worktree(&workdir.join(gix::path::from_bstr(path.as_bstr())))
                    .map(|d| (None, d))
            }
        })
    }
}

/// A blob or symlink at `path` in `tree`, with its id.
fn blob_at(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    path: &BString,
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    use gix::objs::tree::EntryKind::*;
    let Some(entry) = tree.lookup_entry_by_path(gix::path::from_bstr(path.as_bstr()).as_ref())?
    else {
        return Ok(None);
    };
    Ok(match entry.mode().kind() {
        Blob | BlobExecutable | Link => {
            let id = entry.id().detach();
            Some((id, repo.find_blob(id)?.take_data()))
        }
        Tree | Commit => None,
    })
}

/// Read both sides of every path and pair exact renames.
fn collect(
    repo: &gix::Repository,
    paths: &BTreeSet<BString>,
    old_side: &Reader<'_>,
    new_side: &Reader<'_>,
) -> Result<Vec<FileChange>> {
    let hash_kind = repo.object_hash();
    struct Side {
        path: String,
        old: Option<(ObjectId, Vec<u8>)>,
        new: Option<Vec<u8>>,
    }
    let mut entries: Vec<Side> = Vec::new();
    for path in paths {
        // The old side's id pairs renames; a working-tree file has none yet.
        let old = old_side.read(repo, path)?.map(|(id, d)| {
            let id = id.unwrap_or_else(|| {
                gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &d)
                    .unwrap_or_else(|_| ObjectId::null(hash_kind))
            });
            (id, d)
        });
        let new = new_side.read(repo, path)?.map(|(_, d)| d);
        let same = match (&old, &new) {
            (None, None) => true,
            (Some((_, a)), Some(b)) => a == b,
            _ => false,
        };
        if same {
            continue;
        }
        entries.push(Side {
            path: path.to_str_lossy().into_owned(),
            old,
            new,
        });
    }

    // Pair exact renames: a deleted blob whose content reappears under a new path.
    let mut deleted_by_id: HashMap<ObjectId, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if let (Some((id, _)), None) = (&e.old, &e.new) {
            deleted_by_id.entry(*id).or_default().push(i);
        }
    }
    let mut rename_target: Vec<Option<usize>> = vec![None; entries.len()];
    let mut consumed = vec![false; entries.len()];
    if !deleted_by_id.is_empty() {
        for i in 0..entries.len() {
            let (None, Some(data)) = (&entries[i].old, &entries[i].new) else {
                continue;
            };
            let Ok(id) = gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, data) else {
                continue;
            };
            if let Some(list) = deleted_by_id.get_mut(&id)
                && let Some(j) = list.pop()
            {
                rename_target[j] = Some(i);
                consumed[i] = true;
            }
        }
    }

    let mut out = Vec::new();
    let mut taken: Vec<Option<Side>> = entries.into_iter().map(Some).collect();
    for i in 0..taken.len() {
        if consumed[i] {
            continue;
        }
        let Some(e) = taken[i].take() else { continue };
        match rename_target[i] {
            Some(j) => {
                let target = taken[j].take().expect("rename target taken once");
                out.push(FileChange {
                    old_path: Some(e.path),
                    new_path: Some(target.path),
                    old: e.old.map(|(_, d)| d),
                    new: target.new,
                });
            }
            None => out.push(FileChange {
                old_path: e.old.as_ref().map(|_| e.path.clone()),
                new_path: e.new.as_ref().map(|_| e.path.clone()),
                old: e.old.map(|(_, d)| d),
                new: e.new,
            }),
        }
    }
    out.sort_by(|a, b| {
        let ka = a.new_path.as_ref().or(a.old_path.as_ref());
        let kb = b.new_path.as_ref().or(b.old_path.as_ref());
        ka.cmp(&kb)
    });
    Ok(out)
}

fn read_worktree(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        return Some(target.to_string_lossy().into_owned().into_bytes());
    }
    if meta.is_file() {
        return std::fs::read(path).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{branch_with_history, fresh_dir, git};

    fn paths(snap: &Snapshot) -> Vec<String> {
        snap.files
            .iter()
            .filter_map(|f| f.new_path.clone().or(f.old_path.clone()))
            .collect()
    }

    #[test]
    fn a_branch_diffs_against_where_it_was_created_including_uncommitted_work() {
        let dir = fresh_dir("base");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        git(&dir, &["checkout", "-q", "-b", "feature", "main"]);
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "feature work"]);
        std::fs::write(dir.join("a.txt"), "a changed\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();

        let snap = snapshot(&dir, None, &Range::Everything).unwrap();
        assert_eq!(snap.head_label, "feature");
        assert_eq!(snap.base.name.as_deref(), Some("main"));
        assert!(snap.base.reason.contains("reflog"), "{}", snap.base.reason);
        assert_eq!(paths(&snap), ["a.txt", "b.txt", "untracked.txt"]);
        assert_eq!(snap.commits.len(), 1);
        assert_eq!(snap.commits[0].subject, "feature work");
        assert!(snap.dirty);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_range_shows_its_own_files() {
        let dir = branch_with_history("ranges");

        let all = snapshot(&dir, None, &Range::Everything).unwrap();
        assert_eq!(
            paths(&all),
            ["a.txt", "b.txt", "gone.txt", "new.txt", "src/deep/one.rs"]
        );
        let subjects: Vec<_> = all.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["one", "two"], "oldest first");

        let dirty = snapshot(&dir, None, &Range::Uncommitted).unwrap();
        assert_eq!(paths(&dirty), ["b.txt", "new.txt"]);

        let one = snapshot(&dir, None, &Range::Commit(all.commits[0].id)).unwrap();
        assert_eq!(
            paths(&one),
            ["src/deep/one.rs"],
            "nested additions are listed"
        );
        assert_eq!(all.commits[0].id, one.commit.as_ref().unwrap().id);
        assert_eq!(
            one.commit.as_ref().unwrap().message,
            "one\n\nAdds the first function."
        );

        let two = snapshot(&dir, None, &Range::Commit(all.commits[1].id)).unwrap();
        assert_eq!(paths(&two), ["a.txt", "gone.txt"]);
        assert_eq!(two.commit.as_ref().unwrap().subject, "two");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn comparisons_read_like_git() {
        let dir = branch_with_history("spec");
        git(&dir, &["add", "b.txt"]);
        let range = |spec: &str| match parse_spec(&dir, spec).unwrap() {
            Spec::Range(r, _) => r,
            Spec::Base(b) => panic!("{spec} became base {b}"),
        };
        let files = |r: Range| paths(&snapshot(&dir, None, &r).unwrap());

        assert_eq!(files(range("staged")), ["b.txt"]);
        assert_eq!(
            files(range("unstaged")),
            Vec::<String>::new(),
            "b.txt is fully staged"
        );
        std::fs::write(dir.join("a.txt"), "a changed again\n").unwrap();
        assert_eq!(files(range("unstaged")), ["a.txt"]);
        assert_eq!(range("worktree"), Range::Uncommitted);
        assert_eq!(
            files(range("main..feature")),
            ["a.txt", "gone.txt", "src/deep/one.rs"]
        );
        assert_eq!(
            files(range("feature...main")),
            Vec::<String>::new(),
            "main has nothing new"
        );
        assert!(
            matches!(range("HEAD~1"), Range::Commit(_)),
            "a revision is one commit"
        );
        assert!(matches!(range("HEAD"), Range::Commit(_)));
        assert_eq!(
            parse_spec(&dir, "main").unwrap(),
            Spec::Base("main".to_string())
        );
        assert!(parse_spec(&dir, "no-such-thing").is_err());
        assert!(ref_names(&dir).contains(&"main".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_branch_is_not_dirty() {
        let dir = branch_with_history("clean");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "three"]);
        let snap = snapshot(&dir, None, &Range::Everything).unwrap();
        assert!(!snap.dirty);
        assert_eq!(snap.commits.len(), 3);
        assert!(
            snapshot(&dir, None, &Range::Uncommitted)
                .unwrap()
                .files
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

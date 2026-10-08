//! Repository access: find the branch point and collect changed files
//! between that point and the working tree.

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

pub struct Snapshot {
    /// What HEAD is: branch name or short hash.
    pub head_label: String,
    /// What we diff against: e.g. "main @ 1a2b3c4".
    pub base_label: String,
    /// Why that base was chosen.
    pub base_reason: String,
    pub files: Vec<FileChange>,
}

impl Snapshot {
    /// Cheap identity of the snapshot's content, to skip no-op refreshes.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.head_label.hash(&mut h);
        self.base_label.hash(&mut h);
        for f in &self.files {
            f.old_path.hash(&mut h);
            f.new_path.hash(&mut h);
            f.old.hash(&mut h);
            f.new.hash(&mut h);
        }
        h.finish()
    }
}

const DEFAULT_BRANCHES: &[&str] = &["main", "master", "develop", "trunk"];

struct Base {
    commit: Option<ObjectId>,
    label: String,
    reason: String,
}

pub fn snapshot(dir: &Path, base_override: Option<&str>) -> Result<Snapshot> {
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

    let files = collect_changes(&repo, &workdir, base.commit)?;

    Ok(Snapshot {
        head_label,
        base_label: base.label,
        base_reason: base.reason,
        files,
    })
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
        label: format!("{spec} @ {}", short(&commit)),
        reason: "given with --base".to_string(),
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
            label: "empty tree".to_string(),
            reason: "repository has no commits yet".to_string(),
        });
    };

    let branch = repo.head_name()?.map(|n| n.shorten().to_string());
    let Some(branch) = branch else {
        return Ok(Base {
            commit: Some(head),
            label: format!("HEAD @ {}", short(&head)),
            reason: "detached HEAD, showing working tree changes".to_string(),
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
            Ok(Base {
                commit: Some(mb),
                label: format!("{name} @ {}", short(&mb)),
                reason: format!("branch point found via {}", cand.reason),
            })
        }
        None => Ok(Base {
            commit: Some(head),
            label: format!("HEAD @ {}", short(&head)),
            reason: "no base branch found, showing working tree changes".to_string(),
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

fn collect_changes(
    repo: &gix::Repository,
    workdir: &Path,
    base: Option<ObjectId>,
) -> Result<Vec<FileChange>> {
    let base_tree = match base {
        Some(id) => repo.find_commit(id)?.tree()?,
        None => repo.empty_tree(),
    };

    let mut paths: BTreeSet<BString> = BTreeSet::new();
    {
        use gix::diff::index::Change as TreeChange;
        use gix::status::Item;
        use gix::status::index_worktree::Item as WtItem;

        let platform = repo
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .head_tree(base_tree.id)
            .index_worktree_rewrites(None)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled);
        for item in platform.into_iter(Vec::<BString>::new())? {
            match item? {
                Item::TreeIndex(change) => match change {
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
                Item::IndexWorktree(item) => match &item {
                    WtItem::DirectoryContents { entry, .. } => {
                        if entry.status == gix::dir::entry::Status::Untracked {
                            paths.insert(entry.rela_path.clone());
                        }
                    }
                    WtItem::Modification { rela_path, .. } => {
                        paths.insert(rela_path.clone());
                    }
                    WtItem::Rewrite {
                        source,
                        dirwalk_entry,
                        ..
                    } => {
                        paths.insert(source.rela_path().to_owned());
                        paths.insert(dirwalk_entry.rela_path.clone());
                    }
                },
            }
        }
    }

    struct Side {
        path: String,
        old: Option<(ObjectId, Vec<u8>)>,
        new: Option<Vec<u8>>,
    }
    let mut entries: Vec<Side> = Vec::new();
    for path in &paths {
        let old =
            match base_tree.lookup_entry_by_path(gix::path::from_bstr(path.as_bstr()).as_ref())? {
                Some(entry) => {
                    use gix::objs::tree::EntryKind::*;
                    match entry.mode().kind() {
                        Blob | BlobExecutable | Link => {
                            let id = entry.id().detach();
                            Some((id, repo.find_blob(id)?.take_data()))
                        }
                        Tree | Commit => None,
                    }
                }
                None => None,
            };
        let new = read_worktree(&workdir.join(gix::path::from_bstr(path.as_bstr())));
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
    let hash_kind = repo.object_hash();
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

    #[test]
    fn a_branch_diffs_against_where_it_was_created_including_uncommitted_work() {
        let dir = std::env::temp_dir().join(format!("codesnob-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "feature work"]);
        std::fs::write(dir.join("a.txt"), "a changed\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();

        let snap = snapshot(&dir, None).unwrap();
        assert_eq!(snap.head_label, "feature");
        assert!(
            snap.base_label.starts_with("main @ "),
            "base was {}",
            snap.base_label
        );
        let paths: Vec<_> = snap
            .files
            .iter()
            .filter_map(|f| f.new_path.clone())
            .collect();
        assert_eq!(paths, ["a.txt", "b.txt", "untracked.txt"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

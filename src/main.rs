mod daemon;
mod diff;
mod git;
mod highlight;
mod link;
mod loader;
mod marks;
mod mcp;
mod pr;
mod proto;
mod recent;
mod registry;
#[cfg(test)]
mod testutil;
mod ui;
mod watch;
mod wm;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Fast terminal code viewer that agents can drive: files, diffs and pull requests.
#[derive(Parser)]
#[command(version, about)]
#[command(
    after_help = "The viewer talks to a per-user daemon that starts on demand; \
`codesnob show PATH` switches every open viewer to PATH."
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Directory inside the repository or worktree to inspect (default: current directory).
    path: Option<PathBuf>,

    /// Compare against the merge-base with this revision instead of auto-detecting.
    #[arg(long)]
    base: Option<String>,

    /// Syntax theme (bat theme names). "base16" follows the terminal palette.
    #[arg(long, default_value = "base16")]
    theme: String,

    /// Use colors tuned for a light terminal background.
    #[arg(long)]
    light: bool,

    /// Lines of context around each change.
    #[arg(short = 'U', long, default_value_t = 3)]
    context: usize,

    /// Print available syntax themes and exit.
    #[arg(long)]
    list_themes: bool,

    /// Print the detected base and a per-file summary instead of opening the viewer.
    #[arg(long)]
    stat: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask the running viewer(s) to display this directory's repository.
    Show {
        path: PathBuf,
        /// What to compare, in git syntax ("main...feature", "HEAD~3..HEAD",
        /// a commit), "worktree", "staged", "unstaged", or a PR.
        #[arg(long)]
        range: Option<String>,
    },
    /// Show the daemon's current target and the connected viewers.
    Status {
        /// Print the raw JSON reply.
        #[arg(long)]
        json: bool,
    },
    /// List the locations open in the viewer, with their marks and unread
    /// comments, and the other viewers running.
    Locations {
        /// Print the raw JSON reply.
        #[arg(long)]
        json: bool,
    },
    /// List the marks on a location in the viewer, the agent's and the
    /// user's; the user's words count as read afterwards.
    Review {
        /// The location: a directory in its checkout; default: the one on screen.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// The location: a pull request, like owner/repo#142.
        #[arg(long)]
        pr: Option<String>,
        /// Print the raw JSON reply.
        #[arg(long)]
        json: bool,
    },
    /// Remove marks from a location in the viewer.
    Clear {
        /// The location: a directory in its checkout; default: the one on screen.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// The location: a pull request, like owner/repo#142.
        #[arg(long)]
        pr: Option<String>,
        /// Which marks: the agent's own (mine), the marks the user made or
        /// wrote under (user), or every mark (all). Default: mine.
        #[arg(long, value_parser = ["mine", "user", "all"])]
        which: Option<String>,
        /// Letters of the marks to remove instead, e.g. C D. A mark goes
        /// with its whole thread.
        letters: Vec<String>,
        /// Allow removing the user's marks, threads and all. Only when the
        /// user explicitly asked for them to be deleted.
        #[arg(long)]
        user_explicitly_asked_to_delete_comments: bool,
    },
    /// Mark lines of a file with words, as the agent, without changing
    /// what the viewer shows.
    Mark {
        /// The file, relative to the current directory.
        path: String,
        /// The lines: 40-42, or 42.
        lines: String,
        /// The words on the mark.
        text: String,
        /// The location: a directory in its checkout; default: the one on screen.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// The location: a pull request, like owner/repo#142.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Answer in the thread under a mark the user made or wrote under.
    Reply {
        /// The mark's letter, e.g. C.
        letter: String,
        /// The reply.
        text: String,
        /// The location: a directory in its checkout; default: the one on screen.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// The location: a pull request, like owner/repo#142.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Move a mark to other lines, keeping its letter, text and thread.
    Move {
        /// The mark's letter, e.g. C.
        letter: String,
        /// Where it goes: 40-42, or 42.
        lines: String,
        /// The file it goes to (default: the one it is on).
        #[arg(long)]
        path: Option<String>,
        /// The location: a directory in its checkout; default: the one on screen.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// The location: a pull request, like owner/repo#142.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Run the daemon in the foreground (it normally starts on demand).
    Daemon,
    /// Serve the Model Context Protocol over stdio for AI agents.
    Mcp,
    /// Remove PR checkouts that no viewer has open.
    Prune,
}

/// The location `--repo` or `--pr` names, if either was given.
fn location_ref(repo: Option<PathBuf>, pr: Option<String>) -> Option<proto::LocationRef> {
    if repo.is_none() && pr.is_none() {
        return None;
    }
    Some(proto::LocationRef {
        repo: repo.map(|r| std::fs::canonicalize(&r).unwrap_or(r)),
        pr,
    })
}

fn canonical(path: &std::path::Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Some(Cmd::Daemon) => return daemon::run(),
        Some(Cmd::Show { path, range }) => {
            let dir = canonical(&path)?;
            return link::show(dir, range).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Status { json }) => return link::status(json).map_err(|e| anyhow::anyhow!(e)),
        Some(Cmd::Locations { json }) => {
            return link::locations(json).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Review { repo, pr, json }) => {
            return link::review(location_ref(repo, pr), json).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Clear {
            repo,
            pr,
            which,
            letters,
            user_explicitly_asked_to_delete_comments,
        }) => {
            let which = which.unwrap_or_else(|| "mine".to_string());
            return link::clear(
                location_ref(repo, pr),
                which,
                letters,
                user_explicitly_asked_to_delete_comments,
            )
            .map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Reply {
            letter,
            text,
            repo,
            pr,
        }) => {
            return link::reply(location_ref(repo, pr), letter, text)
                .map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Mark {
            path,
            lines,
            text,
            repo,
            pr,
        }) => {
            let args = serde_json::json!({
                "marks": [{ "path": path, "lines": lines, "text": text }],
                "repo": repo,
                "pr": pr,
            });
            let cwd = std::env::current_dir()?;
            let query = mcp::mark_args(&args, &cwd).map_err(|e| anyhow::anyhow!(e))?;
            return link::mark(query).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Move {
            letter,
            lines,
            path,
            repo,
            pr,
        }) => {
            let args = serde_json::json!({
                "letter": letter,
                "lines": lines,
                "path": path,
                "repo": repo,
                "pr": pr,
            });
            let cwd = std::env::current_dir()?;
            let query = mcp::move_args(&args, &cwd).map_err(|e| anyhow::anyhow!(e))?;
            return link::move_mark(query).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Mcp) => return mcp::serve(),
        Some(Cmd::Prune) => {
            let Some(cache) = pr::default_cache() else {
                anyhow::bail!("no cache directory: HOME is not set");
            };
            // PRs open in a viewer hold a lock and are kept.
            let n = pr::prune(&cache, std::time::Duration::ZERO);
            let noun = if n == 1 { "checkout" } else { "checkouts" };
            println!("removed {n} PR {noun}");
            return Ok(());
        }
        None => {}
    }
    if args.list_themes {
        for name in highlight::Highlighter::theme_names() {
            println!("{name}");
        }
        return Ok(());
    }

    let dir = canonical(args.path.as_deref().unwrap_or(std::path::Path::new(".")))?;
    let base = args.base.clone();
    let repo = gix::discover(&dir).is_ok();

    if args.stat {
        if !repo {
            anyhow::bail!("{} is not inside a git repository", dir.display());
        }
        let snap = git::snapshot(&dir, base.as_deref(), &git::Range::Everything)?;
        return match print_stat(snap, args.context) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other.map_err(Into::into),
        };
    }

    let hl = highlight::Highlighter::new(&args.theme).map_err(anyhow::Error::msg)?;
    let palette = if args.light {
        ui::Palette::light()
    } else {
        ui::Palette::dark()
    };
    // PR checkouts unused for a week go, quietly, in the background.
    if let Some(cache) = pr::default_cache() {
        std::thread::spawn(move || pr::prune(&cache, std::time::Duration::from_secs(7 * 86_400)));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let link = link::Link::new();
    let mut app = ui::App::new(
        ui::Options {
            context: args.context,
            state_dir: recent::default_dir(),
            cache_dir: pr::default_cache(),
            pr_source: std::sync::Arc::new(pr::Gh),
        },
        palette,
        hl,
        tx.clone(),
        link.clone(),
    );
    app.open(proto::Target {
        dir: dir.clone(),
        base: base.clone(),
        repo,
    });
    link::spawn(tx.clone(), link);
    ui::run(app, rx, tx)
}

fn print_stat(snap: git::Snapshot, context: usize) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    writeln!(
        out,
        "{} vs {} ({})",
        snap.head_label,
        snap.base.name.as_deref().unwrap_or("HEAD"),
        snap.base.reason
    )?;
    let (mut added, mut removed) = (0, 0);
    for change in snap.files {
        let d = diff::build(change, context);
        let tag = match d.status {
            diff::Status::Added => "A",
            diff::Status::Deleted => "D",
            diff::Status::Modified => "M",
            diff::Status::Renamed => "R",
        };
        if d.binary {
            writeln!(out, "{tag} {:<40} binary", d.display_path())?;
        } else {
            writeln!(
                out,
                "{tag} {:<40} +{} -{}  {} hunks",
                d.display_path(),
                d.added,
                d.removed,
                d.hunks.len()
            )?;
        }
        added += d.added;
        removed += d.removed;
    }
    writeln!(out, "total +{added} -{removed}")?;
    out.flush()
}

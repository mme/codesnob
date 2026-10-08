mod daemon;
mod diff;
mod git;
mod highlight;
mod link;
mod mcp;
mod proto;
mod ui;
mod watch;
mod wm;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Fast terminal diff viewer: shows what changed since the branch point.
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
        /// Compare against the merge-base with this revision.
        #[arg(long)]
        base: Option<String>,
    },
    /// Show the daemon's current target and the connected viewers.
    Status {
        /// Print the raw JSON reply.
        #[arg(long)]
        json: bool,
    },
    /// Run the daemon in the foreground (it normally starts on demand).
    Daemon,
    /// Serve the Model Context Protocol over stdio for AI agents.
    Mcp,
}

fn canonical(path: &std::path::Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Some(Cmd::Daemon) => return daemon::run(),
        Some(Cmd::Show { path, base }) => {
            let dir = canonical(&path)?;
            return link::show(dir, base).map_err(|e| anyhow::anyhow!(e));
        }
        Some(Cmd::Status { json }) => return link::status(json).map_err(|e| anyhow::anyhow!(e)),
        Some(Cmd::Mcp) => return mcp::serve(),
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
        let snap = git::snapshot(&dir, base.as_deref())?;
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
    let (tx, rx) = std::sync::mpsc::channel();
    let link = link::Link::new();
    let mut app = ui::App::new(
        ui::Options {
            context: args.context,
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
    watch::spawn_input(tx);
    ui::run(app, rx)
}

fn print_stat(snap: git::Snapshot, context: usize) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    writeln!(
        out,
        "{} vs {} ({})",
        snap.head_label, snap.base_label, snap.base_reason
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

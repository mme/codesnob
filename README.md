<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/codesnob-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/codesnob-transparent.svg">
    <img src="assets/codesnob-transparent.svg" alt="codesnob" width="640">
  </picture>
</h1>

A lean, fast terminal code viewer that agents can drive: files, diffs and
pull requests, with marks on the lines that matter. You comment directly
in the code, the agent reads your comments and leaves its own.

Point it at a repository or worktree and it shows what changed since the
branch point: committed work, staged and unstaged edits, and untracked files,
in one flat view that refreshes as you type. From there, step through the
branch's commits, or open any file, revision, comparison or pull request
beside it.

- [Install](#install)
- [A first session](#a-first-session)
- [Using it with an agent](#using-it-with-an-agent)
- [How the viewer works](#how-the-viewer-works)
- [Keys](#keys)
- [Command line](#command-line)
- [Files it keeps](#files-it-keeps)
- [Building from source](#building-from-source)

## Install

Linux x86_64 and Apple Silicon Macs:

```sh
curl -fsSL https://raw.githubusercontent.com/mme/codesnob/main/install.sh | sh
```

It puts the binary in `~/.local/bin`, no root needed, verifies it against its
published sha256, and running it again upgrades in place. If `~/.local/bin`
is not on your `PATH`, the installer says so. `CODESNOB_VERSION=v0.1.0` pins
a version, `CODESNOB_PREFIX` picks another directory.

codesnob needs `git` on your `PATH`, and for pull requests the GitHub CLI,
[`gh`](https://cli.github.com), logged in with `gh auth login`. On other
platforms, see [Building from source](#building-from-source).

## A first session

You need an agent that speaks MCP, and a repository with some work on a
branch, committed or not.

**1. Connect your agent.** Once, for Claude Code:

```sh
claude mcp add codesnob --scope user -- codesnob mcp
```

Codex, pi and other agents are under
[Using it with an agent](#using-it-with-an-agent).

**2. Open the viewer.** In a terminal next to your agent, in the same
repository:

```sh
codesnob
```

The status bar at the bottom says what you are looking at, for example
`feature · 2 commits and uncommitted work since main`. codesnob found `main`
by itself; [What is compared](#what-is-compared) explains how.

**3. Ask the agent to show you something.** Start the agent in the
repository and ask "show me where the config file is parsed", or "what
changed on this branch?". The viewer switches to that code or diff and
marks the lines the agent means, lettered `[A]`, `[B]`, and so on. `<` and
`>` step through them. Esc takes you back to what you had before.

**4. Answer it.** Put the cursor on one of its marks, press Enter, type,
and press Enter again to save: your answer appears in a thread under the
mark. Enter on any other line comments on that line; for several lines,
press `v` on the first, move to the last and press Enter. Shift-Enter
breaks the line while you type. Your comments are marks like the agent's,
drawn in teal under the lines they are about, and lettered after it.

Then tell the agent "I left comments". It reads them and answers under each
one, in the same thread. Press Enter anywhere in a thread to add to it.
Your comments stay until you delete them; the agent removes them only when
you ask it to. Until it has read them, it can't pull you to another
location: the request waits behind a banner at the top, and `y` goes there
anyway.

Ask it to review something and it comments the same way you do, on the
lines you are looking at. Whenever it adds marks or a reply while you are
not typing, the cursor goes there, to the first of them; while you stay
there, anything further waits behind `>`. Every mark stays until someone
removes it: `x` removes the one under the cursor, `X` all of them, and the
agent clears its own once they have served.

**5. Look around yourself.** `j` and `k` (or the arrow keys) move the cursor
line, `n` and `p` jump between hunks, `N` and `P` between files. `f` lists
the changed files and `z` narrows the view to the one under the cursor.
Change a file in your editor and save: the view updates and stays on the
line you were reading.

**6. Step through commits.** `c` lists the branch's commits. Moving through
the list previews each one, and Enter keeps it. `Alt-↑` and `Alt-↓` step to
the older and newer commit without the list. `u` shows only the uncommitted
changes. Esc goes back to everything since the branch point.

**7. Open something else.** Press `o` and type one of these:

- `src/main.rs`: a file as it is now
- `main:src/main.rs`: the same file as of `main`
- `HEAD~3..HEAD`, `main...feature`, or a commit hash: a comparison
- `#123`: pull request 123 of this repository's GitHub remote

Each one opens as a new location. `Alt-←` takes you back, and `L` lists
everything you have open.

**8. Quit** with `q`. `:` or `Ctrl-K` lists every command with its key.

## Using it with an agent

`codesnob mcp` is an MCP server over stdio. Add it to your agent:

```sh
claude mcp add codesnob --scope user -- codesnob mcp   # Claude Code
codex mcp add codesnob -- codesnob mcp                  # Codex
pi mcp add codesnob -- codesnob mcp                     # pi 1.0 or later
```

Most other agents take a JSON configuration in this form:

```json
{
  "mcpServers": {
    "codesnob": { "command": "codesnob", "args": ["mcp"] }
  }
}
```

The server gives the agent eight tools:

| Tool | Does |
|---|---|
| `show_code` | show whole files or excerpts, as they are or as of a revision, with marks on lines and optional text |
| `show_diff` | show any comparison from [the range table](#what-is-compared), optionally limited to some files, with marks on lines |
| `list_locations` | list every location: each repository or pull request, what it shows, which viewer has it on screen, and how many of your words it has that the agent has not read |
| `get_review` | read every mark on a location, the agent's and yours, with the threads under them, the code under your marks and what is new |
| `mark` | mark lines with words, up to 20 in one call, without changing the view: for comments on code you are already looking at. A ```mermaid fence in the words is drawn as a diagram |
| `reply` | answer in the thread under a mark |
| `move_mark` | put a mark on other lines after the agent changed the code under it; it keeps its letter, text and thread |
| `clear_marks` | remove the agent's own marks. A mark you made or wrote under goes whole, thread and all, and only when the call says you explicitly asked for that (`user_explicitly_asked_to_delete_comments`); otherwise it is refused |

Every reply tells the agent the letters of the marks it placed and whether
you have comments it has not read yet. A reply that leaves marks of the
agent's also lists them all, with a reminder to remove the ones no longer
needed. The review tools work on the
location on screen unless the agent names another, by the repository's
path (`repo`) or as `owner/repo#123` (`pr`). The tool descriptions tell the
agent when to use them, so you don't need to mention codesnob in your
prompts: "show me", "what changed on this branch" and "I left some
comments" are enough.

The viewer must already be running for the agent to show anything; the
agent does not start one. If you have more than one viewer open, the
agent's requests go to the one on the same Hyprland workspace as the
agent's terminal, or the nearest numbered workspace. On other desktops they
go to the viewer that was opened first. Marks are shared: two viewers on
the same repository show the same ones, and reading or writing comments
needs no viewer at all.

## How the viewer works

### What is compared

By default codesnob shows everything on your branch that is not on the
branch it came from: commits, staged changes, unstaged changes and untracked
files, as one diff. It looks for that base branch in this order:

1. the branch your branch was created from, as recorded in its reflog
2. the upstream it tracks
3. `origin/HEAD`, then `main`, `master`, `develop` and `trunk`, locally and
   on `origin`

Of these, it uses the one that shares the most recent commit with your
branch. On `main` itself, that is usually `origin/main`, so you see what
you have not pushed. With a detached HEAD, or when no base is found, only
uncommitted changes are shown. `codesnob --stat` prints the base it picked
and why.

You can always ask for something else, from the Open prompt (`o`), with
`codesnob show --range`, or from an agent. Ranges use git's syntax:

| Range | Shows |
|---|---|
| `main...feature` | what `feature` changed since it diverged from `main` |
| `main..feature` | `main` against `feature` |
| `main` (a branch or tag alone) | everything since the branch point with it |
| `a1b2c3d`, `HEAD~3` (any other revision) | that commit against its parent |
| `worktree` | the working tree against `HEAD` |
| `staged` | the index against `HEAD` |
| `unstaged` | the working tree against the index |
| `#123`, `owner/repo#123`, a PR URL | a GitHub pull request |

### Locations

Everything you open stays open in the viewer as a location: one per
repository, one per pull request. A location holds its own range, cursor
and marks. `Alt-←` and `Alt-→` (or `-`) move between them, and `L` lists
them. With several open, the status bar names the one on screen.

Agents name a location by what it is: the repository's path, or
`owner/repo#123` for a pull request. When an agent shows you something in a
repository you already have open, it replaces what that repository's
location was showing. Esc brings back what you had before.

The marks on a location belong to the location, not to a viewer. The daemon
keeps them: a second viewer opening the same repository shows the same
marks, and they survive closing the viewer until the daemon exits.

### Marks

A mark is a line range with a letter, `[A]`, `[B]`, … in the order marks
were made, whoever made it, with words on it or not. A mark keeps its
letter, so you and the agent can both say "B" and mean the same lines.
Yours are drawn in teal, the agent's in purple. Yours always carry words,
since Enter is how you make one; the agent's carry words when you asked it
to explain, review or comment, and are bare when it is only pointing.

Every mark stays until someone removes it. A show adds marks and removes
none, so the agent is told to clear its own once they have served; `x`
removes the mark under the cursor, whoever made it, and `X` clears the
location after asking.

A mark's text sits under the lines it is about, and its thread under
that. Enter on a marked line, on a mark's row, or on any turn in its
thread adds a turn at the end of the thread; what you type appears where
it will go, and Shift-Enter (or Alt-Enter) breaks the line. To put a
second mark on lines that already carry one, select them with `v`. The
agent answers your marks the same way, and you can answer its.
Turns have no letters of their own: `[C]` names the mark, and the thread
under it reads top to bottom. New marks or a new turn of the agent's take
the cursor to the first of them, unless you are typing or selecting lines,
or have not moved since the last time it did that.

A mark you made, or wrote under, is yours. The agent cannot remove it, nor
anything in its thread, unless you ask it to, and then the mark goes whole.
When it changes the code a mark is on, it can move the mark to the right
lines, but it cannot change what you wrote.

Marks follow their code as you edit. If the lines a mark pointed at are
deleted, the mark stays and is flagged stale.

A ```mermaid fence in a mark's text is drawn as a diagram in place of the
fence, with the words around it as they are: flowcharts, sequence, state,
class and ER diagrams and the other Mermaid kinds. In a terminal that shows
images (Sixel, kitty or iTerm2) it is a picture on the mark's own colour;
elsewhere it is box-drawing text. Pictures are lettered in Inter, which
codesnob brings along, so they look the same on every machine; a diagram
that names its own `fontFamily` in a `%%{init: …}%%` preamble gets that
font if the machine has it. The drawing is made off the main thread and
cached; a diagram that cannot be drawn shows its source and why.

### When the agent shows something while you are busy

If the agent asks to take you to a different location while you are typing
a comment, marking lines, or have comments it has not read yet, the request
waits behind a banner at the top of the view. `y` switches to it, Esc drops
it, and it goes through by itself once the agent reads your comments.
Updates to the location you are on apply at once.

### Pull requests

Opening `#123` asks `gh` for the pull request and checks it out as a
detached worktree under `~/.cache/codesnob`. If the viewer already has a
clone of that repository open, the PR's commits are fetched into it.
Otherwise codesnob keeps a blobless clone in the cache. The diff is taken
from the merge-base with the PR's base branch, so it matches the Files tab
on GitHub.

`d` shows the PR description and `r` fetches the PR again. Checkouts that
have not been used for a week are removed on the next start;
`codesnob prune` removes the ones no viewer has open right away.

### Outside a repository

`codesnob` in a directory that is not inside a git repository opens with a
message saying so. Press `o` to open a repository or a file from there.

### Empty screens

When there is nothing to show (no changes, a pull request still being
fetched, no repository), the viewer says so in one line in the middle of the
screen. In terminals that can show images (Sixel, kitty or iTerm2, such as
foot, kitty, WezTerm or Ghostty), the codesnob sticker sits above that line.

## Keys

Every command is also in the command list, `:` or `Ctrl-K`. The viewer
accepts vi-style, emacs-style and arrow keys for movement. Clicking a line
moves the cursor to it, and the mouse wheel scrolls.

**Moving**

| Key | Does |
|---|---|
| `j` `k`, `↓` `↑`, `Ctrl-N` `Ctrl-P` | move the cursor line |
| `Space` `Backspace`, `PgDn` `PgUp`, `Ctrl-V` `Alt-V` | page down, page up |
| `Ctrl-D` `Ctrl-U` | half a page down, up |
| `g` `G`, `Home` `End`, `Alt-<` `Alt->` | top, bottom |
| `n` `p`, `]` `[` | next, previous hunk |
| `N` `P`, `}` `{`, `Tab` `Shift-Tab` | next, previous file |
| `l` `h`, `→` `←`, `Ctrl-F` `Ctrl-B` | scroll right, left |
| `0`, `Ctrl-A` | scroll back to the first column |

**What is shown**

| Key | Does |
|---|---|
| `o` | open a directory, file, revision, range or pull request; Tab completes directory and branch names |
| `c` | list the branch's commits, previewing each |
| `Alt-↑` `Alt-↓` | older, newer commit |
| `u` | only the uncommitted changes, and back |
| `f` | list the changed files |
| `z` | narrow to the file under the cursor, and back |
| `d` | show or hide the commit message or PR description |
| `r`, `F5` | reload; on a pull request, fetch it again |
| Esc | back out one step: return from what the agent showed, leave a file, widen a narrowed view, return to the full range. Esc never quits |

**Locations**

| Key | Does |
|---|---|
| `Alt-←`, `-` | previous location |
| `Alt-→` | next location |
| `L` | list the open locations |

**Marks**

| Key | Does |
|---|---|
| Enter | comment on the cursor's line; on a marked line, a mark or in its thread, add to the thread |
| Shift-Enter, Alt-Enter | while typing, break the line |
| `v`, `Ctrl-Space` | start marking lines; move to extend, then Enter to write the comment, Esc to cancel |
| `e` | edit your comment or your turn under the cursor |
| `x` | delete the mark under the cursor (with its thread), whoever made it, or your turn |
| `X` | delete all marks in this location, after asking |
| `<` `>` | previous, next mark (or hunk, when there are no marks) |
| `y` | switch to what the agent is waiting to show |

**Other**

| Key | Does |
|---|---|
| `:`, `Ctrl-K` | command list |
| `q`, `Ctrl-C` | quit |

## Command line

```
codesnob [OPTIONS] [PATH]
```

Opens the viewer on the repository containing `PATH` (default: the current
directory).

| Option | Does |
|---|---|
| `--base REV` | compare against the merge-base with `REV` instead of detecting the base branch |
| `-U N`, `--context N` | lines of context around each change (default 3) |
| `--theme NAME` | syntax theme, by bat theme name. The default, `base16`, uses your terminal's colors |
| `--list-themes` | print the available themes |
| `--light` | colors for a light terminal background |
| `--stat` | print the detected base and a per-file summary instead of opening the viewer |

The subcommands talk to viewers that are already running:

| Command | Does |
|---|---|
| `codesnob show PATH [--range R]` | show the repository at `PATH` in the viewer, optionally a specific range |
| `codesnob locations [--json]` | list every location, with its marks and unread comments and the viewers it is open in |
| `codesnob review [--repo DIR \| --pr REF] [--json]` | print the marks on a location (default: the one on screen); the user's words count as read afterwards |
| `codesnob mark PATH LINES TEXT [--repo DIR \| --pr REF]` | mark lines of a file with words, as the agent, e.g. `codesnob mark src/x.rs 40-42 "unused"` |
| `codesnob reply LETTER TEXT [--repo DIR \| --pr REF]` | answer in the thread under a mark |
| `codesnob move LETTER LINES [--path P] [--repo DIR \| --pr REF]` | put a mark on other lines, e.g. `codesnob move C 40-42` |
| `codesnob clear [LETTERS…] [--which mine\|user\|all] [--repo DIR \| --pr REF]` | remove marks by letter, or by owner: `mine` is the agent's own (the default), `user` is every mark the user made or wrote under. Removing the user's marks needs `--user-explicitly-asked-to-delete-comments` |
| `codesnob status [--json]` | list the connected viewers, with their workspace and directory, and every location |
| `codesnob mcp` | run the MCP server on stdin and stdout |
| `codesnob prune` | remove PR checkouts that no viewer has open |
| `codesnob daemon` | run the daemon in the foreground |

Viewers and agents talk through a small per-user daemon over a Unix socket.
It starts when it is first needed and you don't have to manage it. The
daemon keeps every location's marks, so the review commands work without a
viewer, and a location that is not on screen is named with `--repo` or
`--pr`.

## Files it keeps

| Path | Holds |
|---|---|
| `~/.cache/codesnob` | pull request checkouts and clones (`$XDG_CACHE_HOME` if set) |
| `~/.local/state/codesnob` | recently opened locations, offered by `o` (`$XDG_STATE_HOME` if set) |
| `$XDG_RUNTIME_DIR/codesnob` | the daemon's socket (`codesnob-<uid>` in the temp directory if that variable is unset, as on macOS) |

To uninstall, delete `~/.local/bin/codesnob`, `~/.local/share/codesnob` and
the directories above.

## Building from source

With a recent stable Rust toolchain:

```sh
cargo install --git https://github.com/mme/codesnob
```

Or from a checkout, with [just](https://github.com/casey/just):

```
just install    # release build into ~/.local/bin
just test
just lint
```

### Releasing

`just release minor` (or `patch`, `major`) opens a release PR that bumps the
version and drafts the notes in `CHANGELOG.md`. Edit the notes on the PR if
needed and merge it: CI tags the merge, builds and tests on Linux and Apple
Silicon, and publishes both tarballs with checksums as a GitHub release.
Closing the PR instead cancels the release.

## License

Apache-2.0. See [LICENSE](LICENSE).

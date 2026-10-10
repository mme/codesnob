<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/codesnob-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/codesnob-transparent.svg">
    <img src="assets/codesnob-transparent.svg" alt="codesnob" width="640">
  </picture>
</h1>

A lean, fast terminal code viewer that agents can drive: files, diffs and
pull requests, with highlights and notes. You comment directly in the code,
and the agent reads your comments.

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
highlights the lines the agent means, lettered `[A]`, `[B]`, and so on. `<`
and `>` step through them. Esc takes you back to what you had before.

**4. Answer it.** Put the cursor on one of its highlights, press Enter, type,
and press Enter again to save. Enter on any other line comments on that
line; for several lines, press `v` on the first, move to the last and press
Enter. Your comments are marks too, drawn in amber and lettered after the
agent's.

Then tell the agent "I left comments". It reads them and answers under each
one: its first answer to `[C]` is `[C1]`, the next `[C2]`. Press Enter on
its answer to reply in the same thread. Your comments stay until you delete
them; the agent removes them only when you ask it to. Until it has read
them, it can't pull you to another location: the request waits behind a
banner at the top, and `y` goes there anyway.

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

The server gives the agent seven tools:

| Tool | Does |
|---|---|
| `show_code` | show whole files or excerpts, as they are or as of a revision, with highlighted lines and optional notes |
| `show_diff` | show any comparison from [the range table](#what-is-compared), optionally limited to some files, with highlighted lines |
| `list_locations` | list the locations open in the viewer, which one is on screen, and how many comments each has that the agent has not read; and other viewers running |
| `get_review` | read every mark on a location: the agent's highlights, your comments and the replies under them, with the code they are on and what is new |
| `reply_to_comment` | answer one of your comments under it, as `[C1]`, `[C2]`, … |
| `move_comment` | put one of your comments on other lines after the agent changed the code under it; it keeps its letter, text and replies |
| `clear_marks` | remove the agent's own highlights and replies. Your comments only when the call says you explicitly asked for that (`user_explicitly_asked_to_delete_comments`); otherwise it is refused |

Every reply tells the agent the location id and the letters of the marks it
placed, and whether you have comments it has not read yet. The tool
descriptions tell the agent when to use them, so you don't need to mention
codesnob in your prompts: "show me", "what changed on this branch" and "I
left some comments" are enough.

The viewer must already be running; the agent does not start one. If you
have more than one viewer open, the agent's requests go to the one on the
same Hyprland workspace as the agent's terminal, or the nearest numbered
workspace. On other desktops they go to the viewer that was opened first.

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

Everything you open stays open in the viewer as a location, with an id like
`L3` shown in the status bar. A location holds its own range, cursor and
marks. `Alt-←` and `Alt-→` (or `-`) move between them, and `L` lists them.

When an agent shows you something in a repository you already have open, it
replaces what that repository's location was showing. Esc brings back what
you had before.

### Marks and comments

Marks are highlighted line ranges, lettered `[A]`, `[B]`, … in the order
they were made. A mark keeps its letter, so you and the agent can both say
"B" and mean the same lines. The agent's highlights can carry a short note;
your comments are drawn in amber.

Your comments are yours. The agent answers one with a reply under it,
numbered after its letter (`[C1]`, `[C2]`), and you can reply to that in
turn, in the same thread. When it changes the code a comment is on, it can
move the comment to the right lines, but it cannot change what you wrote,
and it deletes your comments only when you ask it to.

Marks follow their code as you edit. If the lines a mark pointed at are
deleted, the mark stays and is flagged stale.

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

### In a window of its own

`codesnob --gui` opens the same viewer as a desktop app: every key,
command, location, mark and agent request works as described here, drawn
with native views instead of a terminal's grid.

- **The view**: file headers and hunks are bars, line numbers sit in columns
  and marks in coloured badges beside them. Highlights and comments are
  cards whose text wraps, with their replies under them; hovering a card
  offers Reply, or Edit and Delete for yours. The view scrolls by the pixel
  and has scroll bars that show while scrolling, the right one with where
  the marks are.
- **Touchpads**: a swipe flings on when the fingers lift, slowing as in
  Chrome, and stops when they touch the pad again. That takes Wayland's
  hold gesture; on X11, which has none, a fling is a short glide instead.
- **Comments**: Enter (or clicking left of a line's number) opens an editor
  under the lines, where the comment wraps as you type; Shift-Enter starts a
  new line, Enter saves, Esc cancels. Prompts and lists float above the
  status bar and take clicks.
- **Fonts**: code in your terminal's font at its size, read from the default
  terminal's configuration (foot, Ghostty, kitty or Alacritty), and the rest
  in the desktop's sans-serif. `--font`, `--font-size` and `--ui-font` pick
  others.
- **Colors**: on [Omarchy](https://omarchy.org) the window takes the current
  theme's colors, light or dark, and follows when you switch themes; the
  default `base16` syntax theme is drawn in them, as in a themed terminal.
  Elsewhere it uses Tomorrow Night, or Tomorrow with `--light`.
- **Selecting and copying**: drag to select, double-click for a word,
  triple-click for a line. Copying lines of code leaves their numbers
  behind. What you select is the primary selection, which middle-click
  pastes. `Ctrl-C` (or `Ctrl-Shift-C`, `Ctrl-Insert`) copies and `Ctrl-V`
  (or `Ctrl-Shift-V`, `Shift-Insert`) pastes into a prompt, so Omarchy's
  `Super-C` and `Super-V` work too. In the window these keys no longer quit or
  page down; `q` quits.

The window needs a codesnob built with it, see
[Building from source](#building-from-source). Its window class is
`codesnob`, for window rules.

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

**Marks and comments**

| Key | Does |
|---|---|
| Enter | comment on the cursor's line; on your comment or reply, edit it; on an agent's highlight, answer it; on an agent's reply, reply to it in the same thread |
| `v`, `Ctrl-Space` | start marking lines; move to extend, then Enter to write the comment, Esc to cancel |
| `e` | edit the comment or reply under the cursor |
| `x` | delete the comment (with its replies) or your reply under the cursor |
| `X` | delete all your comments in this location, after asking |
| `<` `>` | previous, next mark (or hunk, when there are no marks) |
| `y` | switch to what the agent is waiting to show |

**Other**

| Key | Does |
|---|---|
| `:`, `Ctrl-K` | command list |
| `q`, `Ctrl-C` | quit (`q` alone in the window, where `Ctrl-C` copies) |

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
| `--gui` | open the viewer [in a window of its own](#in-a-window-of-its-own) |
| `--font NAME`, `--font-size PX` | the window's font for code (default: your terminal's) |
| `--ui-font NAME` | the window's font around the code (default: the desktop's sans-serif) |

The subcommands talk to viewers that are already running:

| Command | Does |
|---|---|
| `codesnob show PATH [--range R]` | show the repository at `PATH` in the viewer, optionally a specific range |
| `codesnob locations [--json]` | list the locations open in the viewer, with their marks and unread comments, and the other viewers running |
| `codesnob review [--location L3] [--json]` | print the marks on a location (default: the one on screen); the comments count as read afterwards |
| `codesnob reply LETTER TEXT [--location L3]` | answer one of the user's comments under it |
| `codesnob move LETTER LINES [--path P] [--location L3]` | put one of the user's comments on other lines, e.g. `codesnob move C 40-42` |
| `codesnob clear [LETTERS…] [--which mine\|user\|all] [--location L3]` | remove marks by letter, or by owner: `mine` is the agent's highlights and replies (the default), `user` is the user's comments. Removing the user's comments needs `--user-explicitly-asked-to-delete-comments` |
| `codesnob status [--json]` | list the connected viewers, with their workspace and directory |
| `codesnob mcp` | run the MCP server on stdin and stdout |
| `codesnob prune` | remove PR checkouts that no viewer has open |
| `codesnob daemon` | run the daemon in the foreground |

Viewers and agents talk through a small per-user daemon over a Unix socket.
It starts when it is first needed and you don't have to manage it.

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

The window (`--gui`) is a build feature for Linux, off by default because it
links the desktop's libraries (xkbcommon, fontconfig, Vulkan, Wayland and
X11), which headless machines and the release binaries do without:

```sh
cargo install --git https://github.com/mme/codesnob --features gui
```

Or from a checkout, with [just](https://github.com/casey/just):

```
just install    # release build into ~/.local/bin, with the window on Linux
just test
just lint
```

`just gui= install` leaves the window out, for a machine without a desktop.

### Releasing

`just release minor` (or `patch`, `major`) opens a release PR that bumps the
version and drafts the notes in `CHANGELOG.md`. Edit the notes on the PR if
needed and merge it: CI tags the merge, builds and tests on Linux and Apple
Silicon, and publishes both tarballs with checksums as a GitHub release.
Closing the PR instead cancels the release.

## License

Apache-2.0. See [LICENSE](LICENSE).

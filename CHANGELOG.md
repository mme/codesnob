# Changelog

Every release, newest first. A version's section is drafted when its release
PR is prepared (`release / create-pr`), can be edited on that PR like any
other file, and becomes the GitHub Release's body verbatim when the PR
merges. The marker below is where the next section is inserted; it is part of
the machinery, not decoration.

<!-- next -->

## Unreleased

- **Locations are named by what they are.** The `L3` ids are gone. A
  location is a repository or a pull request, and agents name it by the
  repository's path (`repo`) or as `owner/repo#123` (`pr`); the review
  tools default to the location on screen. With several locations open,
  the status bar shows the name of the one on screen.
- **Marks are shared.** The daemon keeps every location's marks. Two
  viewers on the same repository show the same marks, reading and
  answering comments needs no viewer, and comments survive closing the
  viewer until the daemon exits.
- **Threads instead of numbered replies.** Replies no longer have labels
  like `[C1]`; every mark has a thread under it. Enter on a mark or
  anywhere in its thread adds to the thread, and what you type appears
  where it will go. Answering an agent's mark starts the thread under it.
  `e` edits your own words.
- **Your threads are yours.** A mark you made or wrote under cannot be
  touched by the agent, not even its own turns, unless you explicitly ask
  for it to be deleted, and then it goes whole.
- **One kind of mark.** Yours and the agent's are the same thing: a
  lettered line range with an author, words on it or not, and a thread.
  Every mark stays until someone removes it; a show adds marks and removes
  none (the `append` option is gone with the rule it softened). The agent
  is told to clear its own once they have served, and every reply that
  leaves marks of its own lists them with a reminder to do so. `x` removes
  the mark under the cursor whoever made it, `X` clears the location after
  asking. Your marks are drawn in teal.
- **Text under the lines, over several rows.** A mark's text and its
  thread now sit below the lines they are about, as in a review on the
  web, and a jump puts the first marked line at the top. Shift-Enter (or
  Alt-Enter) breaks the line while you type, and text with several lines
  is drawn on several rows. Enter on a line that already carries a mark
  adds to that mark's thread instead of starting another; `v` still marks
  the same lines again on purpose. Saving a comment leaves the cursor on
  it. A mark's tint now covers its lines even where the diff coloured
  them, and the bar beside the lines is gone: the letter marks the first
  line, the tint says how far it runs.
- **The agent marks without showing.** A new `mark` tool (and `codesnob
  mark`) puts marks with words on lines, up to 20 in one call, without
  changing the view, for comments on code you are already looking at. Its
  marks are lettered top to bottom, as a show's are. New marks or a reply
  of the agent's take the cursor to the topmost of them, unless you are
  typing or have not moved since the last jump. The show tools
  take `marks` with `text` instead of `highlights` with `note`; `reply`
  (was `reply_to_comment`) answers under any mark, `move_mark` (was
  `move_comment`) moves any mark, and `get_review` says who made each.
- The protocol version is now 3. Viewers, agents and the daemon must be
  on the same build.

## v0.1.0

The first release.

- **What changed since the branch point**: point codesnob at any directory
  inside a repository or worktree and it diffs the working tree against the
  commit the branch was created from, found through the reflog, the upstream
  or the default branch. Committed, staged, unstaged and untracked changes
  show together.
- **A flat, readable view**: sticky file headers, old and new line numbers,
  hunk headers with the enclosing function, syntax highlighting and
  intra-line highlighting of what actually changed. A cursor line moves with
  `j`, `k`, the arrows or a click, and the view scrolls once it reaches an
  edge. Arrow, vi and emacs keys.
- **Live**: the view refreshes as files change and stays on the line you were
  reading. Ignored directories are never watched.
- **Command palette**: `:` or Ctrl-K lists every command with its key.
- **Commits**: `c` lists the branch's commits and previews each one as you
  move through the list. Alt-↑ and Alt-↓ step to the older and newer commit
  directly, and `u` shows only the uncommitted changes. `d` shows or hides
  the commit message.
- **Files**: `f` lists the changed files, and `z` narrows the view to one.
- **Open anything**: `o` opens a directory, a file, a file as of a revision
  (`main:src/diff.rs`), a pull request (`#123`, `owner/repo#123`, a URL) or
  a comparison in git's syntax (`main...feature`, `HEAD~3`, `staged`).
- **Locations**: everything you open stays open in the viewer, under an id
  like `L3` that the status bar shows. Alt-← and Alt-→ walk through them,
  `-` goes back, and `L` lists them. A location that only ever showed an
  error goes away once a working one is open.
- **Pull requests**: checked out with `gh` into `~/.cache/codesnob`, next to
  your own clone when one is open. `d` shows the description and `r`
  fetches again. Checkouts unused for a week are removed, and
  `codesnob prune` removes the ones no viewer has open right away.
- **Agents show you code**: `codesnob mcp` is an MCP server over stdio, so
  Claude Code, Codex, pi or any agent that speaks MCP can use it.
  `show_code` shows whole files or excerpts, as they are or as of a
  revision, and can add files to the view. `show_diff` compares anything
  with anything: git ranges, staged or unstaged work, or a pull request.
  Both pick the viewer on your own Hyprland workspace, else the nearest one,
  and replace what that repository's location showed until you press Esc.
  `codesnob show PATH` does the same from a shell, with `--range` to pick
  the comparison.
- **Marks you share with the agent**: agents highlight line ranges in files
  and diffs, with short notes when you ask them to explain the code. Marks
  are lettered `[A]`, `[B]`… in the order they were made and keep their
  letters, so you and the agent mean the same thing by "B". They follow
  their code as you edit, and a mark whose lines are gone stays, flagged
  stale. `<` and `>` go to the previous and next mark from the cursor.
- **Comments**: Enter comments on the cursor's line, or answers the agent's
  highlight under it. Ctrl-Space or `v` marks several lines first. `e` and
  `x` edit or delete a comment, and `X` clears all of yours after asking.
  Comments are marks too, drawn in amber.
- **Agents read your review**: `get_review` gives the agent every mark on a
  location with the commented code, and says which comments are new.
  `clear_marks` removes marks by letter, or all of the agent's, yours or
  everyone's. Every reply names the location and the letters of the marks
  it placed, and says when you have comments the agent has not read.
  `codesnob review` and `codesnob clear` do the same from a shell.
- **Your review comes first**: while you write a comment, mark lines or have
  comments the agent has not read, its request to show something else waits
  behind a banner. `y` switches, Esc drops it, and it goes through on its
  own once the agent reads your comments. Updates to the location you are
  on apply at once.
- **A status bar in plain words**: it says what is shown, such as "feature ·
  2 commits and uncommitted work since main", where you are when stepping
  through commits, and how many of your comments the agent has not read.
- **Linux and Apple Silicon Macs**, installed with `install.sh`.

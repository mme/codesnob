# Changelog

Every release, newest first. Changes land under `## Unreleased` as they
merge; when a release PR is prepared (`release / create-pr`) that section
becomes the version's, or one is drafted from the commits when there is
none. It can be edited on that PR like any other file and becomes the GitHub
Release's body verbatim when the PR merges. The marker below is where the
next section goes; it is part of the machinery, not decoration.

<!-- next -->

## v0.2.1

- **Words wrap.** A mark's text, the turns under it and what you type
  break at the width, at a space where there is one, instead of running
  off the screen; a resize wraps them again. A block that fits is still
  one stop for the cursor, and jumps land on the marks as before.
- **Letters run down the screen.** The marks one `show` adds are lettered
  as they stand in the viewer, file by file and top down, whatever order
  the agent gave them in.
- **A mark with something to say.** The agent is asked to put a sentence,
  or a diagram, on a mark where the lines alone do not tell the story,
  and to leave one bare where they do.

## v0.2.0

- **Locations are named by what they are.** The `L3` ids are gone. To an
  agent, a location is a checkout and a view of it in one name
  (`/home/u/repo`, `/home/u/repo#main`, `/home/u/repo#main...HEAD`,
  `/home/u/repo#branch`, `#worktree`, `#staged`, `#unstaged`) or a pull
  request (`owner/repo#123`), never relative to the agent's directory,
  and every call names one. The marks belong to that exact view. With
  several locations open, the status bar shows the name of the one on
  screen.
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
- **Three tools.** `show` puts a location on screen, whole files or
  regions of them, and sets its marks in one call: entries add new marks,
  lettered in the order given, move one by letter, or delete one;
  `replace` removes the agent's own marks first. `review` (was
  `get_review`) reads the marks, or only some by letter, and says who
  made each; `reply` answers under any mark. `show_code`, `show_diff`,
  `mark`, `move_mark`, `clear_marks` and `list_locations` are gone, as
  are the `line` and `excerpts` options: a file shows whole or by `lines`,
  and marks without files show the marked lines with a few around them.
  Every reply closes with the same lines: the location, the marks there
  with the agent's own by file and your unread words, and a note when
  something is held or stale. New marks or a reply of the agent's take
  the cursor to the topmost of them, unless you are typing or have not
  moved since the last jump. The CLI follows: `codesnob show LOCATION
  [FILES]`, `codesnob review LOCATION [LETTERS]`, `codesnob reply
  LOCATION LETTER TEXT`.
- **Diagrams in marks.** A ```mermaid fence in a mark's text, the
  agent's or yours, is drawn as a diagram where the fence was: a picture
  on the mark's colour in a terminal that shows images, box-drawing text
  anywhere else. All Mermaid kinds; drawn off the main thread, cached,
  and given up on after a few seconds with the source shown instead.
  Pictures are lettered in Inter, which codesnob carries with it, so a
  diagram looks the same on every machine; a fence's own `fontFamily`
  still wins.
- The protocol version is now 4. Viewers, agents and the daemon must be
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

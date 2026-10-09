# Changelog

Every release, newest first. A version's section is drafted when its release
PR is prepared (`release / create-pr`), can be edited on that PR like any
other file, and becomes the GitHub Release's body verbatim when the PR
merges. The marker below is where the next section is inserted; it is part of
the machinery, not decoration.

<!-- next -->

## v0.1.0

The first release.

- **What changed since the branch point**: point codesnob at any directory
  inside a repository or worktree and it diffs the working tree against the
  commit the branch was created from, found through the reflog, the upstream
  or the default branch. Committed, staged, unstaged and untracked changes
  show together.
- **A flat, readable view**: sticky file headers, old and new line numbers,
  hunk headers with the enclosing function, syntax highlighting and
  intra-line highlighting of what actually changed. Arrow, vi and emacs keys.
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
- **Locations**: everything you open stays open in the viewer. Alt-← and
  Alt-→ walk through them, `-` goes back, and `L` lists them. A location
  that only ever showed an error goes away once a working one is open.
- **Pull requests**: checked out with `gh` into `~/.cache/codesnob`, next to
  your own clone when one is open. `d` shows the description and `r`
  fetches again. `codesnob prune` removes checkouts no viewer has open.
- **Agents show you code**: `codesnob mcp` gives Claude Code two tools.
  `show_code` shows whole files or excerpts, as they are or as of a
  revision. `show_diff` compares anything with anything: git ranges, staged
  or unstaged work, or a pull request. Both pick the viewer on your own
  Hyprland workspace, else the nearest one, and replace what that
  repository's location showed until you press Esc. `codesnob show PATH`
  does the same from a shell, with `--range` to pick the comparison.
- **Highlights**: agents can mark line ranges with short notes, in files and
  in diffs. They are numbered top to bottom, ① to ⑳, `<` and `>` step
  through them, and they follow their code as you edit.
- **A status bar in plain words**: it says what is shown, such as "feature ·
  2 commits and uncommitted work since main", and where you are when
  stepping through commits.
- **Linux and Apple Silicon Macs**, installed with `install.sh`.

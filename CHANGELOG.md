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
- **Driven by agents**: `codesnob mcp` gives Claude Code a `show_diff` tool,
  and `codesnob show PATH` does the same from a shell. Both pick the viewer
  on your own Hyprland workspace, else the nearest one.
- **Linux and Apple Silicon Macs**, installed with `install.sh`.

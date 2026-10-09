<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/codesnob-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/codesnob-transparent.svg">
    <img src="assets/codesnob.svg" alt="codesnob" width="640">
  </picture>
</h1>

A lean, fast terminal code viewer that agents can drive: files, diffs and pull
requests, with highlights and notes.

Point it at a repository or worktree and it shows what changed since the
branch point: committed work, staged and unstaged edits, and untracked files,
in one flat view that refreshes as you type.

## Installing

Linux x86_64 and Apple Silicon Macs. The repository is private, so the
installer downloads through `gh` (run `gh auth login` once):

```
gh api repos/mme/codesnob/contents/install.sh -H 'Accept: application/vnd.github.raw' | sh
```

From a checkout, `sh install.sh` does the same. It puts the binary in
`~/.local/bin`, verifies it against its published sha256, and running it again
upgrades in place. `CODESNOB_VERSION=v0.1.0` pins a version.

## Using

```
codesnob [PATH]        open the viewer (default: current directory)
codesnob show PATH     show PATH in the viewer on your workspace
  --range R             what to compare: main...feature, HEAD~3..HEAD, a commit,
                        worktree, staged, unstaged, or a PR
codesnob status        list connected viewers and their workspaces
codesnob mcp           MCP server over stdio, for agents
codesnob prune         remove PR checkouts no viewer has open
```

Give Claude Code the `show_code` and `show_diff` tools, so "show me the
code" or "what changed" lands in your viewer:

```
claude mcp add codesnob --scope user -- codesnob mcp
```

In the viewer, `:` or Ctrl-K lists every command with its key. The ones you
need to start:

| Key | Does |
|---|---|
| `o` | open a directory or a file, a file as of a revision (`main:src/diff.rs`), a PR (`#123`, `owner/repo#123`, a URL), or a comparison (`main...feature`, `HEAD~3`, `staged`) |
| `c` | the branch's commits; moving through the list previews each one |
| Alt-↑ Alt-↓ | older and newer commit, directly |
| `<` `>` | previous and next highlight an agent placed, or hunk when there are none |
| `u` | only the uncommitted changes, and back |
| `f` | the changed files; `z` narrows the view to one |
| `d` | show or hide the commit message or PR description |
| Alt-← Alt-→ | previous and next location; `-` also goes back |
| `L` | all locations open in this viewer |
| Esc | back out one level |

PRs are checked out with `gh` into `~/.cache/codesnob`, next to your own
clone when one is open. Checkouts unused for a week are removed, and `codesnob prune` clears
the ones no viewer has open right away.

## Building

```
just install            release build into ~/.local/bin
just test
just lint
```

## Releasing

1. Run `just release minor` (or `patch`, `major`), or start
   **release / create-pr** from the Actions tab. It bumps the version, drafts
   the notes into CHANGELOG.md and opens a PR.
2. Edit the notes on that PR if needed.
3. Merge it. **release / publish** tags the merge, builds and tests on Linux
   and Apple Silicon, and attaches both tarballs with checksums to a GitHub
   Release.

Closing the PR instead cancels the release; nothing is published until the
merge.

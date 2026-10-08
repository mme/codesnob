# codesnob

A lean, fast terminal diff viewer. Point it at a repository or worktree and it
shows what changed since the branch point: committed work, staged and
unstaged edits, and untracked files, in one flat view that refreshes as you
type.

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
codesnob status        list connected viewers and their workspaces
codesnob mcp           MCP server over stdio, for agents
```

Give Claude Code the `show_diff` tool:

```
claude mcp add codesnob --scope user -- codesnob mcp
```

Press `?` in the viewer for keys.

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

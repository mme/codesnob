default:
    @just --list

build:
    cargo build

# Build a release codesnob into ~/.local/bin (override with CODESNOB_PREFIX)
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    prefix="${CODESNOB_PREFIX:-$HOME/.local/bin}"
    mkdir -p "$prefix"
    # to a temp name first, then rename: installing over a running viewer or
    # daemon would otherwise fail with ETXTBSY
    install -m 755 target/release/codesnob "$prefix/.codesnob.new"
    mv -f "$prefix/.codesnob.new" "$prefix/codesnob"
    echo "installed $prefix/codesnob"
    case ":${PATH}:" in
      *":$prefix:"*) ;;
      *) echo "warning: $prefix is not on PATH" ;;
    esac

# Remove the binary and its license file
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    prefix="${CODESNOB_PREFIX:-$HOME/.local/bin}"
    rm -f "$prefix/codesnob"
    rm -rf "$HOME/.local/share/codesnob"
    echo "removed $prefix/codesnob"

test:
    cargo test

lint:
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

# Open a release PR (patch, minor or major); merging it publishes
release bump="patch":
    gh workflow run release-pr.yml -f bump={{bump}}
    @echo "the PR appears in a minute: gh pr list --head release/"

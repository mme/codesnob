# The window (`--gui`), built on Linux; `just gui= install` leaves it out,
# for machines without a desktop.
gui := if os() == "linux" { "--features gui" } else { "" }

default:
    @just --list

build:
    cargo build

# Build a release codesnob, with the window on Linux, into ~/.local/bin (override with CODESNOB_PREFIX)
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release {{gui}}
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
    if [ -n "{{gui}}" ]; then cargo test {{gui}}; fi

lint:
    cargo clippy --all-targets -- -D warnings
    if [ -n "{{gui}}" ]; then cargo clippy --all-targets {{gui}} -- -D warnings; fi
    cargo fmt --all --check

fmt:
    cargo fmt --all

# Open a release PR (patch, minor or major); merging it publishes
release bump="patch":
    gh workflow run release-pr.yml -f bump={{bump}}
    @echo "the PR appears in a minute: gh pr list --head release/"

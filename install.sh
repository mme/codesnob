#!/bin/sh
# Installs codesnob: the latest release binary into ~/.local/bin, no root.
#
#   sh install.sh
#   gh api repos/mme/codesnob/contents/install.sh -H 'Accept: application/vnd.github.raw' | sh
#
# Running it again is also how codesnob upgrades: the binary is replaced in
# place, and a daemon left over from the old version is taken over.
#
# Downloads use curl against the public release URLs. The repository is
# private for now, where those answer 404, so every download falls back to
# `gh`, which carries the user's credentials.
#
# Override the directory with CODESNOB_PREFIX, pin a version with
# CODESNOB_VERSION=vX.Y.Z. CODESNOB_BASE_URL exists for tests/install_sh.rs
# and is not part of the interface.
set -eu

REPO="mme/codesnob"
REPO_URL="${CODESNOB_BASE_URL:-https://github.com/$REPO}"
BIN_DIR="${CODESNOB_PREFIX:-$HOME/.local/bin}"
SHARE_DIR="$HOME/.local/share/codesnob"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS/$ARCH" in
  Linux/x86_64) SUFFIX="x86_64-linux" ;;
  Darwin/arm64) SUFFIX="aarch64-darwin" ;;
  Darwin/*)
    echo "prebuilt codesnob for Mac is Apple Silicon only (this is $ARCH)." >&2
    echo "build from source instead: https://github.com/$REPO#building" >&2
    exit 1
    ;;
  *)
    echo "prebuilt codesnob supports x86_64 Linux and Apple Silicon Macs (this is $OS/$ARCH)." >&2
    echo "build from source instead: https://github.com/$REPO#building" >&2
    exit 1
    ;;
esac

have_gh() {
  command -v gh >/dev/null 2>&1
}

TAG="${CODESNOB_VERSION:-}"
if [ -z "$TAG" ]; then
  # The latest tag, read from where the stable /releases/latest URL lands.
  # No API call: nothing to rate-limit, nothing to parse.
  LATEST="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$REPO_URL/releases/latest" 2>/dev/null)" || LATEST=""
  TAG="${LATEST##*/}"
  case "$TAG" in
    v[0-9]*) ;;
    *)
      TAG=""
      if have_gh; then
        TAG="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null)" || TAG=""
      fi
      ;;
  esac
fi
case "$TAG" in
  v[0-9]*) ;;
  *)
    echo "could not find a codesnob release (got '$TAG')." >&2
    echo "the repository is private: install gh and run 'gh auth login' first." >&2
    exit 1
    ;;
esac

# Whether this is an upgrade, decided by what is on disk before the swap. The
# old version is captured now, while the binary that can answer still exists.
UPGRADING=0
PREVIOUS=""
if [ -x "$BIN_DIR/codesnob" ]; then
  UPGRADING=1
  PREVIOUS="$("$BIN_DIR/codesnob" --version 2>/dev/null | cut -d' ' -f2)" || true
fi

TARBALL="codesnob-$TAG-$SUFFIX.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() {
  curl -fsSL "$REPO_URL/releases/download/$TAG/$1" -o "$TMP/$1" 2>/dev/null && return 0
  have_gh || return 1
  gh release download "$TAG" --repo "$REPO" --pattern "$1" --dir "$TMP" --clobber >/dev/null 2>&1
}

echo "downloading codesnob $TAG"
for asset in "$TARBALL" "$TARBALL.sha256"; do
  fetch "$asset" || {
    echo "could not download $asset" >&2
    exit 1
  }
done
# sha256sum on Linux; macOS ships shasum instead, reading the same format.
if command -v sha256sum >/dev/null 2>&1; then
  CHECK="sha256sum -c --quiet"
else
  CHECK="shasum -a 256 -c --quiet"
fi
(cd "$TMP" && $CHECK "$TARBALL.sha256" >/dev/null 2>&1) || {
  echo "checksum mismatch: refusing to install" >&2
  exit 1
}
tar -xzf "$TMP/$TARBALL" -C "$TMP"

# To a temporary name first, then rename: installing over a running viewer or
# daemon would otherwise fail with ETXTBSY.
mkdir -p "$BIN_DIR" "$SHARE_DIR"
install -m 755 "$TMP/codesnob" "$BIN_DIR/.codesnob.new"
mv -f "$BIN_DIR/.codesnob.new" "$BIN_DIR/codesnob"
install -m 644 "$TMP/LICENSE" "$SHARE_DIR/"

if [ "$UPGRADING" = 1 ]; then
  case "$PREVIOUS" in
    "${TAG#v}") echo "reinstalled codesnob ${TAG#v}" ;;
    "") echo "upgraded codesnob to ${TAG#v}" ;;
    *) echo "upgraded codesnob $PREVIOUS -> ${TAG#v}" ;;
  esac
  if [ "$PREVIOUS" != "${TAG#v}" ]; then
    # A daemon from the old version keeps running until something replaces
    # it. A newer daemon takes the socket over on start; viewers reconnect.
    ("$BIN_DIR/codesnob" daemon </dev/null >/dev/null 2>&1 &)
  fi
else
  echo "installed $BIN_DIR/codesnob"
fi
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo "note: $BIN_DIR is not on PATH in this shell"
    if [ "$OS" = "Darwin" ]; then
      echo "  add to ~/.zshrc:  export PATH=\"\$HOME/.local/bin:\$PATH\""
    fi
    ;;
esac
if [ "$UPGRADING" = 0 ]; then
  echo "next: run codesnob inside a repository; for agents:"
  echo "  claude mcp add codesnob --scope user -- codesnob mcp"
fi

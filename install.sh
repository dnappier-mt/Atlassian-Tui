#!/usr/bin/env bash
# Build jui in release mode and install binaries to ~/.local/bin.
#
# Usage:
#   ./install.sh              # build release, copy binaries
#   ./install.sh --link       # symlink to target/release instead of copying
#   ./install.sh --debug      # use debug profile (target/debug)
#   ./install.sh --uninstall  # remove installed binaries
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${JUI_INSTALL_DIR:-$HOME/.local/bin}"
BINARIES=(jui jui-daemon)

PROFILE="release"
PROFILE_DIR="release"
MODE="copy"
ACTION="install"

for arg in "$@"; do
  case "$arg" in
    --link) MODE="link" ;;
    --debug) PROFILE="dev"; PROFILE_DIR="debug" ;;
    --uninstall) ACTION="uninstall" ;;
    -h|--help)
      sed -n '2,8p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

mkdir -p "$BIN_DIR"

if [[ "$ACTION" == "uninstall" ]]; then
  for bin in "${BINARIES[@]}"; do
    target="$BIN_DIR/$bin"
    if [[ -e "$target" || -L "$target" ]]; then
      rm -f "$target"
      echo "removed $target"
    fi
  done
  exit 0
fi

echo "building jui ($PROFILE) ..."
if [[ "$PROFILE" == "release" ]]; then
  (cd "$REPO_DIR" && cargo build --release --bins)
else
  (cd "$REPO_DIR" && cargo build --bins)
fi

SRC_DIR="$REPO_DIR/target/$PROFILE_DIR"

for bin in "${BINARIES[@]}"; do
  src="$SRC_DIR/$bin"
  dst="$BIN_DIR/$bin"
  if [[ ! -x "$src" ]]; then
    echo "missing binary: $src" >&2
    exit 1
  fi
  rm -f "$dst"
  if [[ "$MODE" == "link" ]]; then
    ln -s "$src" "$dst"
    echo "linked $dst -> $src"
  else
    install -m 0755 "$src" "$dst"
    echo "installed $dst"
  fi
done

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "warning: $BIN_DIR not in PATH" >&2 ;;
esac

echo "done. run: jui"

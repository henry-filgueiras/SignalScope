#!/usr/bin/env bash
# Internal helper: locate the most recently-built `signalscope` binary
# under the repo. Builds debug if neither release nor debug exists.
#
# Usage:  _locate-binary.sh REPO_ROOT
#
# Prints the binary path on stdout. The caller honors SIGNALSCOPE_BIN
# if set (lets the user pin a specific binary).

set -euo pipefail

repo_root="${1:?repo root required}"

if [[ -n "${SIGNALSCOPE_BIN:-}" ]]; then
  if [[ ! -x "$SIGNALSCOPE_BIN" ]]; then
    echo "SIGNALSCOPE_BIN is set but not executable: $SIGNALSCOPE_BIN" >&2
    exit 2
  fi
  echo "$SIGNALSCOPE_BIN"
  exit 0
fi

release="$repo_root/target/release/signalscope"
debug="$repo_root/target/debug/signalscope"

mtime() {
  # macOS uses `stat -f %m`, Linux uses `stat -c %Y`. Try both.
  stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

# Prefer whichever binary was built most recently. Avoids the pitfall
# of an old `cargo build --release` shadowing a fresh `cargo build`
# during development.
pick=""
if [[ -x "$release" && -x "$debug" ]]; then
  if (( $(mtime "$release") >= $(mtime "$debug") )); then
    pick="$release"
  else
    pick="$debug"
  fi
elif [[ -x "$release" ]]; then
  pick="$release"
elif [[ -x "$debug" ]]; then
  pick="$debug"
fi

if [[ -z "$pick" ]]; then
  echo "no signalscope binary found; building debug..." >&2
  ( cd "$repo_root" && cargo build -p signalscope-tui -q )
  pick="$debug"
fi

echo "$pick"

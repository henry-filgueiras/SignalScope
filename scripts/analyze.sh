#!/usr/bin/env bash
# Open a recorded SignalScope session in the analyze TUI.
#
# Usage:
#   scripts/analyze.sh DIR_OR_FILE
#
# DIR_OR_FILE may be either a directory previously written by record.sh
# (in which case session.signalscope-session inside it is used) or a
# direct path to a .signalscope-session file.

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: analyze.sh DIR_OR_FILE

  Opens the session in the analyze TUI. Snapshot at end of recording;
  seek with [/] (one event), {/} (ten events), g/G (start/end).

  Accepts either a directory written by record.sh (looks for
  session.signalscope-session inside) or a direct path to a session
  file.
EOF
}

if [[ $# -ne 1 || "$1" == "-h" || "$1" == "--help" ]]; then
  usage
  exit 0
fi

arg="$1"

if [[ -d "$arg" ]]; then
  candidate="$arg/session.signalscope-session"
  if [[ ! -f "$candidate" ]]; then
    echo "analyze.sh: no session.signalscope-session in directory $arg" >&2
    exit 2
  fi
  session_file="$candidate"
elif [[ -f "$arg" ]]; then
  session_file="$arg"
else
  echo "analyze.sh: not a directory or file: $arg" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

bin="$("$SCRIPT_DIR/_locate-binary.sh" "$REPO_ROOT")"

exec "$bin" analyze "$session_file"

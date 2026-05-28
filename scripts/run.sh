#!/usr/bin/env bash
# Build and run signalscope. Any args after `run.sh` are forwarded to the
# binary (currently the binary doesn't take CLI args, but this lets us add
# them without changing the script).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PROFILE="${SIGNALSCOPE_PROFILE:-release}"

case "$PROFILE" in
  release) PROFILE_FLAG=(--release) ;;
  debug)   PROFILE_FLAG=() ;;
  *)
    echo "SIGNALSCOPE_PROFILE must be 'release' or 'debug' (got '$PROFILE')" >&2
    exit 2
    ;;
esac

cd "$REPO_ROOT"
exec cargo run "${PROFILE_FLAG[@]}" -p signalscope-tui -- "$@"

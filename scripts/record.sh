#!/usr/bin/env bash
# Record a SignalScope session sized by the operator's intent.
#
# Usage:
#   scripts/record.sh DURATION -o DIR [--label TEXT] [--max DURATION]
#
# DURATION is the *data* window: the script asks the binary to exit
# only once every spawned sensor has produced observations that span
# at least DURATION. So "30s" means "30 seconds of useful data from
# every channel", not "30 wall-clock seconds." The recording's
# wall-clock length is whatever it has to be to honor that.
#
# A sensor that reports degraded health (e.g. Wi-Fi off, no default
# route) is treated as satisfied — capture won't hang waiting for a
# source that physically can't contribute.
#
# DURATION accepts a bare number (seconds) or a `<n><unit>` suffix
# where unit is `s`, `m`, or `h` — e.g. `30s`, `5m`, `1h`.
#
# DIR is the output directory. The session file is always written as
# `DIR/session.signalscope-session`. mkdir -p applies.
#
# --max is a wall-clock safety cap, passed through to the binary.
# Defaults to max(60s, 3 × DURATION) so pathological waits don't
# hang forever but reasonable captures aren't surprised by it.

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: record.sh DURATION -o DIR [--label TEXT] [--max DURATION]

  DURATION    the data window per sensor. Accepts 30s / 5m / 1h, or
              a bare number of seconds. Capture exits when every
              spawned sensor has data spanning at least DURATION
              (or has gone degraded, whichever).
  -o DIR      output directory. session.signalscope-session is
              written inside it.
  --label     optional free-form recording label.
  --max       wall-clock safety cap. Defaults to max(60s, 3*DURATION).

Examples:
  scripts/record.sh 30s -o "$(mktemp -d)"
  scripts/record.sh 5m -o ./recordings/hotel-wifi
  scripts/record.sh 30s --max 2m -o ./recordings/run-12
EOF
}

if [[ $# -eq 0 || "$1" == "-h" || "$1" == "--help" ]]; then
  usage
  exit 0
fi

duration_raw="$1"
shift

out_dir=""
label=""
max_raw=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -o|--output)
      [[ $# -ge 2 ]] || { echo "record.sh: -o requires a directory" >&2; exit 2; }
      out_dir="$2"
      shift 2
      ;;
    --label)
      [[ $# -ge 2 ]] || { echo "record.sh: --label requires a value" >&2; exit 2; }
      label="$2"
      shift 2
      ;;
    --max)
      [[ $# -ge 2 ]] || { echo "record.sh: --max requires a duration" >&2; exit 2; }
      max_raw="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "record.sh: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$out_dir" ]]; then
  echo "record.sh: -o DIR is required" >&2
  usage
  exit 2
fi

parse_duration() {
  local raw="$1"
  case "$raw" in
    *s) echo "${raw%s}" ;;
    *m) echo "$(( ${raw%m} * 60 ))" ;;
    *h) echo "$(( ${raw%h} * 3600 ))" ;;
    *)  echo "$raw" ;;
  esac
}

if ! window_seconds="$(parse_duration "$duration_raw")"; then
  echo "record.sh: could not parse duration '$duration_raw'" >&2
  exit 2
fi
if ! [[ "$window_seconds" =~ ^[0-9]+$ ]] || (( window_seconds < 1 )); then
  echo "record.sh: duration must resolve to a positive integer of seconds (got '$duration_raw')" >&2
  exit 2
fi

# Wall-clock safety cap. Default keeps short captures unbothered
# while still bounding long ones at 3× the requested window.
if [[ -n "$max_raw" ]]; then
  if ! max_seconds="$(parse_duration "$max_raw")"; then
    echo "record.sh: could not parse --max '$max_raw'" >&2
    exit 2
  fi
else
  triple=$(( window_seconds * 3 ))
  if (( triple > 60 )); then
    max_seconds=$triple
  else
    max_seconds=60
  fi
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

bin="$("$SCRIPT_DIR/_locate-binary.sh" "$REPO_ROOT")"

mkdir -p "$out_dir"
session_file="$out_dir/session.signalscope-session"

if [[ -z "$label" ]]; then
  label="record-$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

echo "record.sh: capturing until every sensor spans ${window_seconds}s (hard cap ${max_seconds}s) → ${session_file}"
"$bin" capture \
  --output "$session_file" \
  --label "$label" \
  --window "${window_seconds}s" \
  --max "${max_seconds}s" &
ss_pid=$!

trap 'kill -INT "$ss_pid" 2>/dev/null || true' INT TERM

# The binary exits on its own once the window is satisfied or --max
# fires — no need for the script to manage timing. Just wait.
wait "$ss_pid" || true

echo
"$bin" inspect "$session_file"

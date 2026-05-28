#!/usr/bin/env bash
# Record a SignalScope session for a bounded duration.
#
# Usage:
#   scripts/record.sh DURATION -o DIR [--label TEXT] [--warmup DURATION]
#
# DURATION accepts a bare number (seconds) or a `<n><unit>` suffix where
# unit is `s`, `m`, or `h` — e.g. `30s`, `5m`, `1h`.
#
# DIR is the output directory. The session file is always written as
# `DIR/session.signalscope-session`. The directory must already exist
# OR be creatable; the script will mkdir -p.
#
# --warmup adds a pre-roll period to the capture. The macOS Wi-Fi sensor
# pays a one-shot ~10–15 s `system_profiler` cold-start cost; in a short
# capture that means the first Wi-Fi data lands deep into the recording.
# Set `--warmup 15s` (or similar) when you want the requested DURATION
# to be observable-data time rather than wall-clock time.
#
# On completion, `signalscope inspect` runs against the file so the
# operator gets an immediate summary.

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: record.sh DURATION -o DIR [--label TEXT] [--warmup DURATION]

  DURATION    how long to record. Accepts 30s / 5m / 1h, or a bare
              number of seconds.
  -o DIR      output directory. session.signalscope-session is
              written inside it.
  --label     optional free-form recording label.
  --warmup    optional pre-roll added to the capture, in the same
              format as DURATION. Defaults to 0. Useful on macOS
              where Wi-Fi acquisition has a ~10–15 s cold start;
              with `--warmup 15s` the requested duration becomes
              steady-state observation time.

Examples:
  scripts/record.sh 20s -o "$(mktemp -d)"
  scripts/record.sh 5m -o ./recordings/hotel-wifi
  scripts/record.sh 30s --warmup 15s -o ./recordings/run-12
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
warmup_raw="0"

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
    --warmup)
      [[ $# -ge 2 ]] || { echo "record.sh: --warmup requires a duration" >&2; exit 2; }
      warmup_raw="$2"
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

# Parse the duration suffix into seconds.
parse_duration() {
  local raw="$1"
  case "$raw" in
    *s) echo "${raw%s}" ;;
    *m) echo "$(( ${raw%m} * 60 ))" ;;
    *h) echo "$(( ${raw%h} * 3600 ))" ;;
    *)  echo "$raw" ;;
  esac
}

if ! seconds="$(parse_duration "$duration_raw")"; then
  echo "record.sh: could not parse duration '$duration_raw'" >&2
  exit 2
fi
if ! [[ "$seconds" =~ ^[0-9]+$ ]] || (( seconds < 1 )); then
  echo "record.sh: duration must resolve to a positive integer of seconds (got '$duration_raw')" >&2
  exit 2
fi

if ! warmup_seconds="$(parse_duration "$warmup_raw")"; then
  echo "record.sh: could not parse warmup '$warmup_raw'" >&2
  exit 2
fi
if ! [[ "$warmup_seconds" =~ ^[0-9]+$ ]]; then
  echo "record.sh: warmup must resolve to a non-negative integer of seconds (got '$warmup_raw')" >&2
  exit 2
fi

total_seconds=$(( warmup_seconds + seconds ))

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

bin="$("$SCRIPT_DIR/_locate-binary.sh" "$REPO_ROOT")"

mkdir -p "$out_dir"
session_file="$out_dir/session.signalscope-session"

# Default label includes the recording timestamp so untagged sessions
# are still self-describing.
if [[ -z "$label" ]]; then
  label="record-$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fi

if (( warmup_seconds > 0 )); then
  echo "record.sh: capturing ${warmup_seconds}s warmup + ${seconds}s observation → ${session_file}"
else
  echo "record.sh: capturing for ${seconds}s → ${session_file}"
fi
"$bin" capture --output "$session_file" --label "$label" &
ss_pid=$!

# Forward SIGINT to the capture process so Ctrl-C cleanly closes the
# session file rather than leaving the shell wrapper holding the bag.
trap 'kill -INT "$ss_pid" 2>/dev/null || true' INT TERM

# Sleep, then stop the capture cleanly. `wait` returns after the
# capture process flushes and exits.
sleep "$total_seconds"
kill -INT "$ss_pid" 2>/dev/null || true
wait "$ss_pid" || true

echo
"$bin" inspect "$session_file"

# SignalScope

> Explain why the network environment feels bad.

SignalScope is a terminal observability tool for diagnosing Wi-Fi and local-
network quality issues. It correlates RF conditions, gateway latency, DNS
behavior, and interface throughput into short, human-readable operational
narratives — and lets you record those narratives to a file so you can replay
the moment something went wrong without re-running the network.

It is **not** a packet sniffer, an enterprise monitoring product, or a
generic sysadmin dashboard. The goal is to make invisible local-network
trouble *legible*.

## Three things you can do with it

1. **`signalscope observe`** — open the live TUI. Watch the network as it is
   right now: connected-link RSSI, RF environment, gateway/DNS latency,
   interface throughput, with sparklines and persistence phrases ("stable
   2m12s", "spiking 8s", "bursting 6s") so the dashboard reads as *what's
   changing*, not *what is*.
2. **`signalscope capture`** — record a session to a portable JSONL file.
   Run it on a flaky laptop, ship the file somewhere, replay it offline.
3. **`signalscope analyze`** — open a recorded session in the same TUI,
   anchored on a *playhead* you move through the recording. Includes a
   one-row timeline strip that shows the shape of the whole recording at
   a glance, plus a derived list of *landmarks* — the moments worth
   investigating — and `n`/`p` keys to hop between them.

The shell wrappers in `scripts/` give you the record → analyze loop in two
commands.

## Quick start

```sh
# Build (recent stable Rust). The scripts will do this for you if needed.
cargo build --release

# Record 20 seconds into a fresh directory…
./scripts/record.sh 20s -o "$(mktemp -d)"

# …and open the same directory in the analyze TUI.
./scripts/analyze.sh /tmp/tmp.XXXXXX
```

`record.sh` prints an `inspect` summary on completion so you can confirm
what landed in the file before opening it. `analyze.sh` accepts either the
directory `record.sh` wrote, or a direct path to a
`.signalscope-session` file.

You can skip the scripts and call the binary directly:

```sh
./target/release/signalscope observe                                    # live TUI
./target/release/signalscope observe --record session.signalscope-session
./target/release/signalscope capture --output session.signalscope-session --label hotel-wifi
./target/release/signalscope inspect session.signalscope-session         # one-screen summary
./target/release/signalscope analyze session.signalscope-session         # offline replay TUI
```

`scripts/record.sh DURATION -o DIR` parses `30s`, `5m`, `1h`, or a bare number
of seconds.

## Replay at a glance

Once you're in `analyze`, the screen is a time instrument:

```
SignalScope · analyze · hotel-wifi · playhead +00:38:12 of 02:00:00 · 84/127 · 2026-05-28T19:23:21.456Z
─────·──·──•─────────●●●─────────·─────────────┃─────────●●─────·──·──────·──   ← timeline strip
┌─ Connected link ──────────────┐  ┌─ RF environment ─────┐
│  …                            │  │  …                    │
│  Held 12m34s · Δ RSSI -3 dB   │  │                       │
│  RX/TX 28.4 Mbps / 3.1 Mbps   │  │                       │
└───────────────────────────────┘  └───────────────────────┘
…
┌─ Landmarks · 12/23 ──────────────────────────────────────┐
│ ▸ +00:38:12  FIND  Active · gateway flapping             │
│   +00:38:35  GW    Gateway spiking · 192.168.1.1 47 ms   │
│   +00:38:48  TPUT  Throughput idle → bursting · 28 Mbps  │
│   +00:39:02  DNS   DNS failing · cloudflare.com (timeout)│
└──────────────────────────────────────────────────────────┘
```

- **Top strip** projects the whole recording onto the terminal width. Glyph
  weight (`·` `•` `●`) tracks how many landmarks fall in each column;
  color tracks worst severity (red `Alarm`, green `Recovery`, blue
  `Notable`); the `┃` is your playhead. The shape of the recording reads
  in one row before you read any words.
- **Landmarks pane** is a derived index of the recording's lifecycle
  transitions, sensor-health changes, and stance flips (gateway → spiking,
  DNS → failing, throughput → bursting). It's a *pure function* of the
  recording — the same file always produces the same landmarks.
- **Everything else** — the connected-link card, gateway/DNS sparklines,
  RF occupancy histogram, throughput row — renders the dashboard *as it
  would have looked* at the playhead's exact moment. `now` is virtualized
  to the playhead's timestamp, so "Held 12m34s" and "stable 2m12s" read
  truthfully.

## What works today

- live **connected-link** card (SSID, BSSID, RSSI, noise, SNR, channel,
  PHY mode) driven by `system_profiler -xml SPAirPortDataType` on macOS,
  with a legacy `airport` fallback. Includes a longitudinal "Held" duration,
  Δ RSSI / 60s callout, recent-RSSI sparkline, and a duplex RX/TX
  sparkline pair scaled log10 so a Kbps trickle and a Gbps burst share
  a row without one hiding the other
- **RF environment** panel anchored on the connected channel: header reads
  `connected ch44 · pressure: moderate · density stable`, body is a flat,
  relevance-ranked channel-occupancy histogram (connected first → same-band
  by proximity → other bands by AP count → background), each row band-
  annotated. The identity-oriented AP table is one `d` keypress away
- **Interface throughput plane** — per-interface counters from `sysinfo`
  (safe wrapper around `getifaddrs`+`if_data`), with a 15 s rolling
  throughput window and per-step rate for sparkline bars
- gateway latency probe + rolling sparkline + `stable / spiking / unreachable`
  stance phrasing
- DNS latency probe + rolling sparkline + `answering / failing` stance
- sensor-health surface — when Wi-Fi is off, redacted, a backend is missing,
  or interface counters are unavailable, the card shows the actual state
  instead of going silent
- per-observation confidence tags (`Direct` / `Inferred` / `Estimated` /
  `Stale`) so the UI can distinguish "we measured this" from "we inferred
  this" from "we used to know this"
- lightweight correlation rules with confidence + evidence, including
  longitudinal trend findings (`SignalTrend`, `RfDensityTrend`,
  `RfCongestion`)
- lifecycle-aware findings (`Active` / `Escalating` / `Recovering` /
  `Resolved`) — the dashboard reacts to *transitions*, not every poll,
  so the feed stays calm even when a condition holds for minutes
- canonical session recording (v2): append-only newline-delimited JSON,
  RFC 3339 timestamps, versioned header. `tail -f`, `jq`, `wc -l` all
  just work
- offline replay (`analyze`) with virtual "now" anchored on the playhead,
  full state rebuild on every seek (no drift, no stale half-state), and
  the timeline strip + landmarks navigation described above
- graceful resize, clean shutdown, structured file logging

## What's intentionally **not** here yet

- raw 802.11 frames, monitor mode, packet capture
- offensive tooling
- web dashboard / distributed agents
- packet injection
- speed tests / bandwidth benchmarks
- Linux sensor implementations (the abstraction is in place; the adapters
  aren't written)
- replay playback at original cadence (we do event-stepped seek instead;
  see [`DIRECTORS_NOTES.md`](DIRECTORS_NOTES.md) for the rationale)

These are tracked in [`DIRECTORS_NOTES.md`](DIRECTORS_NOTES.md) and
[`docs/architecture.md`](docs/architecture.md).

## Keyboard

Common (both modes):

| Key            | Action                                          |
| -------------- | ----------------------------------------------- |
| `q` / `Esc`    | quit                                            |
| `Ctrl-C`       | quit                                            |
| `Tab` / `f`    | cycle focus                                     |
| `d`            | toggle RF view (occupancy histogram ↔ AP table) |
| `?`            | toggle help                                     |

Replay only (`signalscope analyze`):

| Key             | Action                          |
| --------------- | ------------------------------- |
| `[` / `]`       | seek ±1 event                   |
| `{` / `}`       | seek ±10 events                 |
| `←` / `→`       | seek ±1 event (Shift = ±10)     |
| `n` / `p`       | next / previous landmark        |
| `g` / `G`       | jump to recording start / end   |
| `Home` / `End`  | jump to recording start / end   |

The playhead always lands on a real event — no "no-op" steps even when
adjacent events are spaced hours apart.

## Session file format

A `.signalscope-session` file is **append-only newline-delimited JSON**.
The first line is a versioned header; every subsequent line is one bus
envelope, verbatim:

```jsonc
{"row":"header","kind":"signalscope-session","format_version":2,
 "created_at":"2026-05-28T19:23:13.381101Z","tool_version":"0.1.0",
 "label":"hotel-wifi"}
{"row":"envelope","id":1,"at":"2026-05-28T19:23:13.412587Z",
 "source":"wifi","event":{"type":"Wifi", … }}
```

Timestamps are RFC 3339 strings, so `jq -r '.at'` returns a date.
`signalscope inspect PATH` prints a one-screen summary of any file.

The format is intentionally inspectable, not optimized — no SQLite,
no binary framing, no compression. Suitable for archiving, diffing,
or piping into your own tooling.

## Building

```sh
# Stable Rust 1.75+
cargo build --release
cargo test --workspace
```

Environment variables:

| Variable               | Meaning                                  | Default  |
| ---------------------- | ---------------------------------------- | -------- |
| `SIGNALSCOPE_LOG`      | `tracing` filter directive (e.g. `debug`)| `info`   |
| `SIGNALSCOPE_LOG_DIR`  | Directory for daily-rotated log files    | `./logs` |
| `SIGNALSCOPE_BIN`      | Override the binary the scripts launch   | (auto)   |

## Workspace layout

```
signalscope-events/    normalized, platform-agnostic event/observation types
signalscope-core/      event bus, clock, session recorder/reader,
                       TemporalSeries, EventSource — the runtime backbone
signalscope-sensors/   platform adapters that emit normalized events
                       (wifi · gateway · dns · iface)
signalscope-analysis/  correlation rules + rolling windows over the stream
signalscope-tui/       ratatui dashboard, replay/landmarks/strip
                       (binary: `signalscope`)
```

The dependency arrow always points *down* this list: TUI knows about
everything, sensors know about events + core, events knows about nothing.
Analysis is platform-agnostic by construction — it consumes normalized
event types only, never CoreWLAN / nl80211 / NetworkManager vocabulary.

## Caveats

- **macOS Wi-Fi redaction.** Without Location Services permission for the
  invoking terminal, macOS redacts SSIDs as `<redacted>` and omits BSSIDs.
  SignalScope still surfaces channel + signal density (useful for RF
  analysis) and tags the observation `Inferred` so the UI shows it as a
  partial reading. Grant Location Services to your terminal of choice to
  get full identifiers.
- **Legacy `airport` backend** is retained for pre-Sonoma hosts only. On
  macOS 14.4+ it doesn't exist; the sensor picks `system_profiler`
  automatically.
- **Gateway probes use `ping(8)`.** Replacing this with a `socket2` ICMP
  path (no subprocess overhead) is intentional future work.
- **Live and replay states are independent.** `observe --record PATH`
  writes a recording from a live run; `analyze PATH` plays it back. There
  is no live-replay split-screen.

## Project notes

[`DIRECTORS_NOTES.md`](DIRECTORS_NOTES.md) is the living design log —
current canon at the top, append-only pivot/discovery archive below.
Read it for the *why*. The README is the *what*.

## License

MIT. See `LICENSE`.

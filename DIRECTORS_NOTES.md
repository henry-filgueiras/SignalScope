# Director's Notes

A living design doc. **Current Canon** is the present-state truth and may be
edited in place — but when a fact stops being true, move the verbatim
sentence into the **Resolved Dragons and Pivots** archive below. Never edit
past entries in the archive.

---

## Current Canon

### Mission

SignalScope answers one question: *why does the network environment feel
bad right now?* It is a terminal observability tool, not a sniffer, not an
enterprise dashboard, not an offensive tool.

### Crate layout

A Cargo workspace with five crates:

- `signalscope-events` — normalized, platform-agnostic event/observation
  types. The lingua franca of the system.
- `signalscope-core` — append-only in-memory event bus (broadcast +
  bounded backlog), clock abstraction, tracing setup, session
  recorder/reader, `EventSource` abstraction, `TemporalSeries`
  rolling-history primitive.
- `signalscope-sensors` — `Sensor` trait + per-source adapters. Currently:
  Wi-Fi (macOS, primary backend `system_profiler -xml SPAirPortDataType`,
  legacy `airport` retained as a fallback for pre-Sonoma hosts), gateway
  (`ping`), DNS (`hickory-resolver`), interface counters (`sysinfo`
  wrapper around `getifaddrs`+`if_data` / `/proc/net/dev`).
- `signalscope-analysis` — rolling windows over the event stream + a
  handful of confidence-scored correlation rules.
- `signalscope-tui` — ratatui dashboard, binary `signalscope`.

Dependency direction is strictly downward. `events` knows about nothing;
`tui` knows about all of them.

### Event bus invariants

- All envelopes carry a monotonic `EventId`, a wall-clock `OffsetDateTime`,
  and a `SensorId`.
- The bus is append-only: once published, an envelope is never mutated or
  revoked.
- The bus broadcasts on a `tokio::sync::broadcast` and retains a bounded
  ring of recent envelopes (default 4096) so new subscribers can seed
  their state on attach.
- Sensors publish; analysis subscribes *and* publishes (its `Finding`s go
  back onto the same bus); the TUI only subscribes.

### Normalization rule

Sensor adapters convert platform-native readings into types defined in
`signalscope-events`. The analysis and TUI layers must not import any OS-
specific term (CoreWLAN, nl80211, NetworkManager, etc.) directly or
transitively. New sensors that need a new event variant must add it in
`signalscope-events` first.

### Correlation philosophy

Findings carry `kind`, stable `fingerprint`, `headline`, `Confidence`,
`peak_confidence`, `evidence`, `lifecycle`, and `first_seen`/`last_seen`
timestamps. Rules are intentionally cautious hand-tuned heuristics —
never "the system says X". The TUI shows confidence and the active
duration so the operator can judge.

The analysis crate is split into two halves: **rules** (stateless,
produce `CandidateFinding`s every cycle) and **lifecycle** (stateful,
emits onto the bus only on `Active` / `Escalating` / `Recovering` /
`Resolved` transitions). Quiescent re-evaluations are suppressed by a
`material_delta` of 0.15, a `min_cooldown` of 15 s between consecutive
emissions of the same fingerprint, and a `resolved_after` of 20 s.

The engine evaluates both on incoming sensor events and on a 2 s
periodic tick, so resolutions still fire when sensors are quiet.

### Observation epistemics

Every observation carries an `ObservationConfidence` tag — `Direct`,
`Inferred`, `Estimated`, or `Stale`. The point is honesty about *how*
something was learned. Examples:

- macOS without Location Services redacts SSIDs and omits BSSIDs; we
  surface those observations as `Inferred`, not `Direct`.
- A neighbor without RSSI is still useful for density counting; it ships
  as `Inferred`.

Renderers can downgrade colors / labels when confidence is anything but
`Direct`. Analysis is free to weight by confidence in the future; today
it only filters out neighbors lacking required fields.

### Degraded-state semantics

Sensors emit `SensorHealth` events on state transitions:
`Operational`, `BackendUnavailable`, `HardwareDisabled`,
`PermissionDenied`, `ParseFailed`, `Stale`. The dashboard reads the
latest health per sensor and reflects it (e.g. Wi-Fi card title shows
the backend, banner shows the state when not `Operational`). Health
events are deliberately *not* synthesized observations — a missing
sensor stays silent on the data plane and loud on the health plane.

### Connected link vs RF environment

SignalScope distinguishes two conceptual layers and the UI mirrors them:

- **Connected link** — the currently associated network as a
  longitudinal entity (RSSI, SNR, channel, PHY mode, "held for X"
  duration, recent-RSSI sparkline, Δ-over-60s callout). The TUI tracks
  `connected_identity = (Option<Ssid>, Option<Bssid>)`,
  `connected_since`, and a rolling `signal_history` ring; identity
  changes reset the duration counter.
- **RF environment** — ambient AP activity treated as sparse and
  probabilistic. The panel is *anchored on the connected channel*: the
  header reads `connected ch44 · pressure: moderate · density
  stable`. The body is a *flat, relevance-ranked* occupancy histogram
  — no band grouping — ordered: connected channel → same-band channels
  (by proximity to connected) → other-band channels (by AP count) →
  background (≤2 APs). Each row carries its band annotation. This
  keeps the connected channel onscreen even when 2.4 GHz is dense, and
  remains coherent on small terminals. The individual-AP table is
  demoted to a `d`-toggled detail view — modern macOS redacts SSIDs
  and BSSIDs anyway, so identity rows are low value. Pressure is a
  four-tier ladder (low / moderate / elevated / severe) computed from
  the AP count on the connected channel.

In `signalscope-analysis`, two rolling windows back the trend rules:

- `WifiSignalWindow` records RSSI tied to an `(Option<Ssid>,
  Option<Bssid>)` identity and resets on identity change. Exposes
  `associated_duration(now)` and `rssi_delta(lookback, now)`.
- `RfEnvironmentWindow` records `(timestamp, ap_count)` per scan,
  exposes `density_delta(lookback, now)`.

Two new rules — `signal_trend` (RSSI Δ over 90 s, threshold ±5 dB)
and `rf_density_trend` (AP-count Δ over 120 s, threshold ±3 APs) —
ride the existing lifecycle pipeline. Direction lives in the
fingerprint suffix (`:degrading` vs `:recovering`; `:rising` vs
`:falling`), so a trend that reverses doesn't quietly mutate the same
entry — the old direction resolves and the new one goes Active.
"Stabilising" is naturally expressed as the Resolved edge of a
previously-active trend.

### Wi-Fi backend layering (macOS)

Inside `signalscope-sensors/src/wifi/macos/` the sensor picks one
acquisition backend at startup and sticks with it:

1. `system_profiler -xml SPAirPortDataType` — primary. Works on every
   modern macOS, no root, but modern privacy surfaces SSIDs as
   `<redacted>` and omits BSSIDs unless Location Services is granted to
   the invoking process.
2. `airport -I` / `airport -s` — legacy. Only picked if the binary
   exists, which it doesn't on macOS 14.4+.

Backend selection is an implementation detail; analysis and the TUI
only see normalized `WifiObservation` / `NeighborAp` / `SensorHealth`.
The TUI's Wi-Fi card title surfaces the active backend so the operator
can tell, but no code path outside `wifi/macos/` branches on it.

### Session recording (canonical format v2)

A single run is preserved as a `.signalscope-session` file: append-
only newline-delimited JSON, first line a versioned `SessionHeader`,
every subsequent line one `SessionRow::Envelope` carrying a published
bus envelope verbatim. The recorder is one async task that subscribes
to the bus (backlog included, in order) and writes through a
`BufWriter<File>` with a per-row `flush()` — an abrupt kill loses at
most the most recent observation, never the tail.

Header (v2 sample):

```json
{"row":"header","kind":"signalscope-session","format_version":2,
 "created_at":"2026-05-28T19:23:13.381101Z","tool_version":"0.1.0",
 "label":"canonical-test"}
```

Envelope:

```json
{"row":"envelope","id":4,"at":"2026-05-28T19:23:13.412587Z",
 "source":"dns","event":{"type":"DnsLatency", … }}
```

Format guarantees:

- **Append-only** — rows are never rewritten.
- **Inspectable** — `tail -f`, `jq`, `wc -l` all just work. Timestamps
  are RFC 3339 strings, so `jq -r '.at'` returns a date. Not a
  database, not binary, not compressed.
- **Versioned** — both `SESSION_FORMAT_VERSION` (current = 2) and
  `SESSION_MIN_READABLE_VERSION` (= 2) are checked on read.
  Future-newer files surface `UnsupportedNewerVersion`; legacy v1
  files (tuple timestamps, never shipped) surface
  `UnsupportedOlderVersion`. Header parsing is two-phase: kind +
  version are validated against the raw JSON value first, so
  legacy files report a useful error instead of a deep serde
  shape failure. Unknown header fields are tolerated so writers
  can grow non-breakingly.
- **Semantically faithful** — what the bus carries is what gets
  recorded: observations, scans, gateway/DNS probes, interface
  counters, interface state changes, roams, correlation findings
  with their full lifecycle edges, sensor-health events. Monotonic
  `EventId`s and wall-clock `at` survive round-trip exactly.

`SessionRow` is a tagged enum so future row kinds (replay markers,
operator notes) can land without breaking the existing shape.

`SessionStats` + `signalscope-core::summarize_session(path)` walk a
file once and return a lightweight per-category tally + time span,
without holding the envelope stream in memory. This is the data
shape the `inspect` subcommand reads off.

### `signalscope inspect`

`signalscope inspect PATH` is the canonical "did the recording
survive the handoff?" verifier. It prints, in one screen, the file
metadata (kind, format version, tool version, label, created_at),
the wall-clock span of the recording, and a per-category event
tally (wifi / scan / gateway / dns / iface_counter / iface_state /
roam / findings / sensor_health). No TUI, no replay, no analysis —
just the smallest tool that confirms a `.signalscope-session` file
is what the recipient thinks it is.

### Event source abstraction

`core::source::EventSource` is a tiny pull trait — `async fn
next_envelope() -> Option<Arc<Envelope>>`. Two implementors today:
the live bus `Subscription` and `FileEventSource` (replay from a
session file). The trait exists to keep replay foundations in place
without committing to a replay UI: an analyzer or a test harness can
be written against `EventSource` and run against either source. No
seeking, no pacing controls — those remain explicit future work.

### Two-mode binary

The `signalscope` binary now dispatches on subcommand:

- `signalscope observe` — the live TUI dashboard. Optional
  `--record PATH` mirrors every envelope to a session file at the
  same time, so the operator can promote a live observation into a
  permanent artifact without restarting. The bare invocation (no
  subcommand) still means `observe`, so existing muscle memory holds.
- `signalscope inspect PATH` — one-shot verifier (see above).
- `signalscope capture --output PATH` — headless recording. Same
  sensor + analysis pipeline, no ratatui. Emits a one-line stderr
  status every 5 s (`wifi=… scan=… gw=… dns=… find=… health=…`) so
  the operator can tell the run is healthy without a dashboard.
  Ctrl-C stops cleanly, prints the elapsed duration and final path.

Arg parsing is hand-rolled; no `clap` dependency. The intent is
non-technical collection and portable diagnostics, not a daemon
platform.

### Rolling-history primitive

`signalscope-core::TemporalSeries<T>` is a deliberately small bounded
wall-clock-timestamped sample window. Capacity is by count
(predictable memory; matches sparkline pixel width); timestamps are
wall-clock `OffsetDateTime` (replay-friendly — a series rebuilt from
a recorded envelope stream renders identically to one built live).
The primitive backs gateway / DNS / RSSI / RX / TX visual history
inside the TUI; the same shape will work for any future rolling-
visualization need without growing a metrics framework. Helpers
worth knowing: `span()`, `elapsed_since_last(now)`, `mean_over(d,
now)` on `f64` series, `max_value()` on ordered series.

This is the only state-shape primitive in `core`. Analysis still owns
its own typed rolling windows because they encapsulate domain math
(trend-half deltas, monotonic-counter resets) — `TemporalSeries` is
the *visual* history layer, not a replacement for those.

### Temporal dashboard stance

The dashboard is increasingly worded in terms of *what is changing*
rather than *what is*:

- **Connected link** — three single-row sparklines stacked under the
  text (RSSI, RX rate, TX rate). RX/TX use a log10 scale so a Kbps
  trickle and a Gbps burst share a row without one flattening the
  other into invisibility. The throughput row carries a one-phrase
  regime callout ("idle 1m12s" / "trickling 24s" / "sustained 8s" /
  "bursting 6s") computed by walking the rate history backwards while
  the regime classifier (`Idle/Trickle/Sustained/Bursting`) stays
  steady on each pair.
- **Gateway latency** — `gateway_stance` walks history back while
  reachability and the "median+50%+5 ms" stance hold, emitting
  `stable 2m12s` / `spiking 8s` / `unreachable 45s` as a tail span on
  the summary row. Below ~3 s the phrase is suppressed; persistence
  is the signal.
- **DNS latency** — `dns_stance` does the same for answered/failing
  runs.
- **RF environment** — already trend-aware via the
  `RfDensityTrend` finding; left as-is.

Across the dashboard the operator should be able to identify bursts,
sustained transfers, idle periods, saturation, and recovery by
*shape* before reading any number.

### Per-step vs averaged throughput

`InterfaceThroughputWindow` exposes two views. `throughput_bps()` is
the rolling average over the entire retained window — the right
shape for the headline number. `step_throughput()` returns the rate
between only the last two samples — the right shape for sparkline
bars, because a sustained transfer reads as a plateau, a burst as a
single tall bar, idle as zero. The TUI uses `throughput_bps()` for
the headline RX/TX numbers and `step_throughput()` for the per-step
samples pushed into the RX/TX rolling series.

### Interface counters & throughput

A fourth sensor (`iface`) follows the default-route interface and
publishes [`InterfaceCountersObservation`] every 2 s — cumulative
`rx/tx_bytes_total`, `rx/tx_packets_total`, `rx/tx_errors_total`. The
backend is the `sysinfo` crate (default features stripped, only the
`network` plane enabled), which wraps `getifaddrs(3)` + `if_data`
on macOS and `/proc/net/dev` on Linux without forcing us to drop
`#![forbid(unsafe_code)]`. No `ifconfig` / `netstat` shelling out.

`rx/tx_dropped_total` and `retry_count` are `Option<u64>` in the
event model and left `None` from this backend by design. Modern
userspace counter surfaces don't expose them; richer integrations
(Linux nl80211 station stats, monitor-mode capture, future platform
work) can populate them later without an event-model migration.

Throughput is derived in `signalscope-analysis::InterfaceThroughputWindow`
as `(latest_counters − earliest_counters_in_window) / dt` over a
15 s rolling span. Deliberately no extra smoothing — over-smoothing
would smear the bursty failures the observatory is built to make
visible. The window resets on interface name change *and* on any
non-monotonic counter (interface reset, driver reload, sysinfo
rebaseline), so derivations never compare apples to oranges. The
analysis engine also forgets throughput state when the iface sensor
reports a `Stale` / `BackendUnavailable` / `HardwareDisabled` health
edge.

The TUI shares the same `InterfaceThroughputWindow` for its visual
derivation, so the dashboard and any future throughput rules
necessarily agree on what "now" means.

### Findings hook for path throughput

No throughput-related `FindingKind` variants exist yet — per
deliberate scope. `InterfaceThroughputWindow` is the *data hook*:
the rolling state needed to author rules like "throughput collapse",
"sustained inactivity under known link load", "throughput recovery",
"bufferbloat-suspicious latency under load" lives in the analysis
crate and ticks against every counter envelope. Adding a rule means
writing a stateless `rules::*` function over the existing window —
no further plumbing.

### Phase 1 scope

In: Wi-Fi link + scan, gateway probe, DNS probe, interface counters
+ throughput derivation, lightweight correlation, TUI dashboard,
append-only JSONL session recording with a minimal replay-read path.
Out: packet capture, monitor mode, offensive tooling, replay UI,
timeline scrubbers, web UI, plugin system, speed tests, bandwidth
benchmarks.

### Platform stance

Initial target is macOS. Linux is an architectural stretch goal — the
abstraction is in place but no Linux sensor is implemented. Adapter swaps
should not require touching `events`, `analysis`, or `tui`.

### Logging

The TUI owns the terminal, so logs go to a rotating file under
`$SIGNALSCOPE_LOG_DIR` (default `./logs`). Filter via `SIGNALSCOPE_LOG`
(default `info`).

### Open questions

- Replace `ping(8)` subprocess with a `socket2`-based ICMP/UDP probe to
  shed the per-tick fork+exec cost.
- Whether SQLite ever earns its keep over JSONL. The current bet is
  that JSONL stays the primary artifact and any future indexed store
  is a derived view computed from it — keeping the recording shape
  inspectable and forward-compatible.
- Decide whether to ship a Location-Services-aware setup path on macOS
  so the operator can see real SSIDs/BSSIDs when desired. Today they
  enable LS for Terminal manually; SignalScope's only job is to be
  honest about the redaction when it happens.
- First Linux backend choice: `iw dev <iface> link` shell-out vs. raw
  `nl80211` netlink. Lean toward `nl80211` for the same reason we
  preferred `system_profiler` over `airport` on macOS — first-party,
  no privilege escalation, no shell parsing.

---

## Resolved Dragons and Pivots

### 2026-05-28 — Claude Opus 4.7 (canonical recording format v2, inspect)

**Demoted from Canon, verbatim:**

> ### Session recording
>
> A single run can be preserved as a `.signalscope-session` file: append-
> only newline-delimited JSON, first line a versioned `SessionHeader`,
> every subsequent line one `SessionRow::Envelope` carrying a published
> bus envelope verbatim. The recorder is one async task that subscribes
> to the bus (backlog included, in order) and writes through a
> `BufWriter<File>` with a per-row `flush()` — an abrupt kill loses at
> most the most recent observation, never the tail. The format is
> deliberately:
>
> - **Append-only** — rows are never rewritten.
> - **Inspectable** — `tail -f`, `jq`, `wc -l` all work; not a database,
>   not binary, not compressed (yet).
> - **Versioned** — `SESSION_FORMAT_VERSION` is checked on read; future-
>   newer files are rejected rather than silently misinterpreted, and
>   the reader tolerates unknown header fields so writers can grow.
> - **Semantically faithful** — what the bus carries is what gets
>   recorded, including lifecycle transitions, observation confidence,
>   sensor-health distinctions, and monotonic `EventId`s.
>
> `SessionRow` is a tagged enum so future row kinds (replay markers,
> operator notes) can land without breaking the existing shape.

**Goal.** Promote the existing `.signalscope-session` shape into the
*canonical* observability recording format: portable enough to hand
to another person, inspectable enough to verify by eye, versioned
strictly enough that older readers refuse newer files (and vice
versa) instead of misinterpreting them.

**Canonical inspectability — RFC 3339 timestamps.** The dominant
ergonomic problem in v1 was the default `time` crate tuple form for
`OffsetDateTime`: `created_at: [2026, 148, 17, 16, 50, ...]`.
`jq -r '.at'` returned a nine-element array. Anybody not already
inside the codebase had no way to know what position the year was in.

Switched `Envelope::at` and `SessionHeader::created_at` to
`#[serde(with = "time::serde::rfc3339")]`, enabled the
`serde-well-known` + `parsing` features on the workspace `time`
dep. Both fields now emit ISO-8601 strings:
`"2026-05-28T19:23:13.412587Z"`. `jq -r '.at'` returns a date.

This is a backwards-incompatible on-disk change. Bumped
`SESSION_FORMAT_VERSION` from 1 to 2 and added a
`SESSION_MIN_READABLE_VERSION = 2` so legacy v1 files (only ever
created during initial development) are rejected with a clear
`UnsupportedOlderVersion` error rather than blowing up deep in
serde with "invalid type: sequence, expected an RFC 3339-formatted
OffsetDateTime."

**Two-phase header parse.** The version check has to run *before*
strict deserialization, or the timestamp-shape failure wins. The
reader now parses the first line as `serde_json::Value`, validates
`kind` and `format_version` against the raw map, and only then
deserializes to the strict `SessionHeader` shape. Same JSON parse
cost in the happy path, dramatically better error messages on the
sad paths.

**SessionStats + summarize_session.** New tiny type in `core::session`
that aggregates a per-category envelope tally and the
first/last timestamps over a session. The walker is one pass, never
buffers the envelope stream. Exposed as `signalscope_core::
summarize_session(path) -> (SessionHeader, SessionStats)`.

**`signalscope inspect PATH` subcommand.** New module
`signalscope-tui/src/inspect.rs`. Reads the session, prints a
one-screen summary (header metadata, span, first/last event
timestamps, per-category counts). No TUI, no replay, no analysis —
the smallest tool that confirms a handed-off `.signalscope-session`
file is what the recipient thinks it is. CLI usage updated, help
text mentions the RFC 3339 timestamp inspectability.

**Test coverage — explicitly broadened for canonical confidence.**
Five new tests in `core::session::tests` on top of the previous
five:

- `timestamps_serialize_as_rfc3339_strings` — guard against a future
  serde-attribute regression on either timestamp field. Asserts both
  `created_at` and `at` deserialize as strings and look RFC-3339-ish.
- `rejects_older_than_minimum_format_version` — legacy v1 files
  surface `UnsupportedOlderVersion`, not a serde shape error.
- `round_trip_handles_a_mixed_event_stream` — Scan + GatewayLatency
  + DnsLatency (failed) + InterfaceCounters + Finding (Active
  lifecycle with evidence + peak_confidence) + SensorHealth all
  round-trip in publication order, with deep field equality on the
  Finding and SensorHealth payloads.
- `malformed_line_surfaces_a_bad_json_error_with_line_number` —
  appends a garbage line between two valid envelopes; reader emits
  `BadJson { line: 3, … }` for the bad row and continues on the
  next.
- `header_tolerates_unknown_fields_for_forward_compat` — header
  with extra `hostname` + `operator` fields parses fine, so a
  slightly-newer writer doesn't strand current readers as long as
  the format_version is unchanged.

Existing tests refactored: the previous `UnsupportedVersion` variant
split into `UnsupportedNewerVersion` and `UnsupportedOlderVersion`
so the two failure modes are nameable.

**Smoke.** `signalscope capture --output /tmp/canonical.session
--label canonical-test` for 11 s on this host produced a file with
RFC-3339 timestamps end-to-end (`jq -r '.at'` printed dates), then
`signalscope inspect` reported 23 envelopes / 11 s span / per-
category tally (gateway 12 / dns 4 / iface_counter 6 / sensor_health
1). The success-criteria flow (record → hand off → recipient can
verify shape and content) works.

**Untouched.** Bus shape, event-bus invariants, lifecycle pipeline,
sensors, observation confidence, macOS backend layering, trend
rules, RF occupancy panel, finding fingerprints, throughput
plane, temporal series, dashboard stance phrases. All edits are
contained to `events::Envelope` (serde attr on `at`),
`core::session` (format bump + RFC 3339 + reader two-phase + stats),
the binary's CLI dispatcher, and one new `inspect` module.

`cargo test --workspace`: 63/63 green (up from 58).

### 2026-05-28 — Claude Opus 4.7 (temporal observatory: TemporalSeries, RX/TX sparklines, stance phrases)

**Demoted from Canon, verbatim:**

> - `signalscope-core` — append-only in-memory event bus (broadcast +
>   bounded backlog), clock abstraction, tracing setup, session
>   recorder/reader, `EventSource` abstraction.

**Goal.** Make the dashboard show *how the environment is changing*
rather than *what the latest reading is*. The project had been
collecting richer data than it was visualizing: throughput existed
as numbers, gateway/DNS history existed as sparklines without
persistence-aware framing, and the connected-link card answered
"what is RSSI" but not "what has the path been doing."

**Pivot.** A small shared rolling-history primitive in `core`, a
duplex throughput sparkline, and a vocabulary of "stance" phrases
that name the current regime and how long it's held.

**Primitive — `signalscope-core::TemporalSeries<T>`.** Bounded by
sample count; each sample carries a wall-clock `OffsetDateTime` so
the same series can be reconstructed from a recorded session and
rendered identically (replay-friendly without a replay UI). Has
`push`, `iter`, `iter_values`, `values`, `latest`, `earliest`,
`span`, `elapsed_since_last`, `clear`. `mean_over(d, now)` for
`f64`; `max_value()` for `T: PartialOrd`. Eight unit tests pin
capacity eviction, zero-capacity degradation, span semantics,
mean-over windowing, max-value, and chronological snapshot order.

The TUI's previously ad-hoc `VecDeque` history collections
(`gateway_history`, `dns_history`, `signal_history`) all moved
onto this. Each sample now carries `env.at` rather than dropping
its timestamp. The bespoke `SignalSample` struct is gone —
`TemporalSeries<i32>` covers it.

**Per-step throughput.** `InterfaceThroughputWindow::step_throughput`
returns the rate between only the last two samples — the bursty
view sparklines should feed off. The headline RX/TX number keeps
using `throughput_bps()` (rolling average) so the *summary* stays
calm. New test `step_throughput_uses_only_last_pair` pins the
shape: in a quiet window followed by one full burst sample, step
≫ avg.

**RX/TX sparklines.** The Connected-link card now stacks three
single-row sparklines under its text: RSSI (existing), RX rate,
TX rate. RX/TX use a log10 scale (`log10(bps) × 10`, clamped
0–100) because throughput legitimately spans many orders of
magnitude on the same row — log scaling preserves the *shape* of
the activity instead of letting one spike flatten everything else
into invisibility. The card grew from 12 to 13 rows so a banner
+ all three sparklines + 7 text lines fit cleanly.

**Stance phrases.** Three new lightweight regime classifiers:

- `throughput_stance` — `Idle / Trickling / Sustained / Bursting`
  based on the per-step peak rate (50 Kbps / 500 Kbps / 25 Mbps
  cutoffs). Walks the RX/TX series back in lockstep while the
  regime holds, emits `idle 1m12s` / `bursting 6s` / ….
- `gateway_stance` — `Lost / Elevated / Stable` based on
  reachability and "median + 50% + 5 ms". Emits `stable 2m12s` /
  `spiking 8s` / `unreachable 45s`.
- `dns_stance` — `Failing / Answering` runs. Emits `failing 12s` /
  `answering 4m08s`.

All three suppress the phrase under 3 s of held state — persistence
is the signal; flickers are noise. Phrases hang off existing
summary rows, never a separate line.

**What this buys.**

- The duplex RX/TX sparkline lets the operator see bursts,
  sustained transfers, idle, and saturation by *shape* before
  reading any number.
- "stable 2m12s" on a flat gateway row makes the absence of drama
  itself a signal. The opposite — "spiking 8s" — names the
  beginning of a regime change before any rule fires.
- The dashboard answers *what is changing* more often than *what
  exists*.

**Untouched.** Bus shape, lifecycle pipeline, sensors, observation
confidence, macOS backend layering, trend rules, RF occupancy
panel, finding fingerprints, session-recording format. Purely
additive across every layer.

`cargo test --workspace`: 58/58 green (up from 49).

### 2026-05-28 — Claude Opus 4.7 (interface counters + throughput)

**Demoted from Canon, verbatim:**

> - `signalscope-sensors` — `Sensor` trait + per-source adapters. Currently:
>   Wi-Fi (macOS, primary backend `system_profiler -xml SPAirPortDataType`,
>   legacy `airport` retained as a fallback for pre-Sonoma hosts), gateway
>   (`ping`), DNS (`hickory-resolver`).

> ### Phase 1 scope
>
> In: Wi-Fi link + scan, gateway probe, DNS probe, lightweight
> correlation, TUI dashboard, append-only JSONL session recording with
> a minimal replay-read path.
> Out: packet capture, monitor mode, offensive tooling, replay UI,
> timeline scrubbers, web UI, plugin system.

**Goal.** Add a fourth observational plane — path throughput and
interface health — without dragging in packet capture or platform
archaeology. Strengthens "what is the network actually doing right
now?" while staying inside the observatory aesthetic.

**Event model.** New `InterfaceCountersObservation` with cumulative
`rx/tx_bytes_total`, `rx/tx_packets_total`, `rx/tx_errors_total` as
required fields. `rx/tx_dropped_total` and `retry_count` are
`Option<u64>` so richer backends (Linux nl80211, monitor mode) can
populate them later without an event-model migration. `Event::
InterfaceCounters(...)` joins the union; `EventCategory::Interface`
now covers state changes *and* counter snapshots.

**Sensor.** New `iface` sensor using the `sysinfo` crate (default
features off, only `network`). That gives us `getifaddrs`/`if_data`
on macOS and `/proc/net/dev` on Linux as safe APIs — keeps the
`#![forbid(unsafe_code)]` invariant intact across the workspace. No
shelling out to `ifconfig` or `netstat`. The sensor follows the
default-route interface (reuses the gateway sensor's `route` lookup),
re-discovers every 30 s so DHCP renewals / Wi-Fi↔Ethernet handoffs
don't strand it, and emits a `Stale` health edge when no default
route exists. Cadence is 2 s — chosen to be fine-grained enough that
the 15-s throughput window has plenty of samples, coarse enough that
the recorder file doesn't bloat under capture.

**Analysis.** New `InterfaceThroughputWindow` in `windows.rs` stores
successive counter snapshots and exposes `throughput_bps()` returning
a `Throughput { rx_bps, tx_bps, sample_span }`. Two reset paths:

- Interface name change wipes the buffer — `en0` counters and `en7`
  counters live in unrelated number spaces.
- Any non-monotonic byte total wipes the buffer — interface resets,
  driver reloads, sysinfo rebaselines all manifest as cumulative
  going backwards, and a derived rate over that boundary would be
  nonsense.

The engine ingests `InterfaceCounters` into the window and also
calls `forget()` when the iface sensor publishes a `Stale` /
`BackendUnavailable` / `HardwareDisabled` health edge.

No throughput-related `FindingKind` variants yet — per scope. The
window *is* the future-findings hook: throughput collapse, sustained
inactivity under known link load, throughput recovery, and bufferbloat
indicators can be authored as `rules::*` functions over it whenever
the project wants them.

**TUI.** The Connected-link card gained one new row:
`RX/TX 28.4 Mbps / 3.1 Mbps    errs 0/0`. Rate uses `fmt_rate` —
fixed-precision per magnitude (Kbps / Mbps / Gbps) so the column
doesn't jitter on sample-to-sample fluctuation, with `idle` for
literal zero. Color-graded by peak rate: dim under 50 Kbps,
informational under 5 Mbps, OK above. Errors light up in `WARN_FG`
the moment they become nonzero — this is the first knob that should
correlate with `gateway loss` / `dns failure` clusters once those
findings can read it. Connected-link card grew from 11 to 12 rows
to make space.

The event feed gained an `iface en0 rx=… tx=… err=…/…` row using a
binary-suffix humaniser so cumulative GB totals don't blow up the
column width.

Both `observe` and `capture` modes register the new sensor; the
capture status line grew an `iface=N` bucket.

**Tests.** New `signalscope-analysis` tests:

- `throughput_window_returns_none_until_one_second_spans` —
  derivation refuses to fire with a single sample.
- `throughput_window_derives_bps_from_byte_delta` — math sanity:
  100 KB/s = 800 Kbps, 10 KB/s = 80 Kbps.
- `throughput_window_resets_on_interface_swap` — name change wipes
  the buffer.
- `throughput_window_resets_on_counter_decrease` — non-monotonic
  counters wipe the buffer.
- `throughput_window_evicts_old_samples_but_keeps_minimum_two` —
  span eviction still leaves enough history to derive a rate, and
  the rate stays stable across eviction.

New `signalscope-sensors` tests pin the route-output parsers:
`macos_route_extracts_interface_field`, `linux_route_extracts_dev_token`,
`linux_route_handles_missing_default`.

`cargo test --workspace`: 49/49 green (up from 41).

**Smoke.** `signalscope capture --output /tmp/iface-smoke.session`
for 7 s on this host emitted 3 counter envelopes for `en0`,
operational health edge, real cumulative byte totals (rx=3.6 GB,
tx=3.2 GB since boot), and the inline `iface=N` status counter
ticked.

**What this buys.** The observatory now sees the *traffic* on the
path, not just the path's RTT. The next operational story it can
help tell — "DNS failures are clustered around throughput collapse
on en0" or "gateway p95 stayed flat across a 30 Mbps→idle drop" —
becomes authorable as a small set of new rules over windows that
already exist.

**Untouched.** Bus shape, lifecycle pipeline, gateway/DNS/Wi-Fi
sensors, observation confidence, macOS backend layering, trend
rules, RF occupancy panel, finding fingerprints, session-recording
format. The change is purely additive at every layer.

### 2026-05-28 — Claude Opus 4.7 (session recording + capture mode)

**Demoted from Canon, verbatim:**

> - `signalscope-core` — append-only in-memory event bus (broadcast +
>   bounded backlog), clock abstraction, tracing setup.

> ### Phase 1 scope
>
> In: Wi-Fi link + scan, gateway probe, DNS probe, lightweight correlation,
> TUI dashboard.
> Out: packet capture, monitor mode, offensive tooling, persistence, replay,
> web UI, plugin system.

> - Decide when persistence is justified (JSONL first, SQLite later).

**Goal.** Introduce durable observability session recording and the
minimal foundations for future replay — without becoming a database
project. Sessions should be portable forensic artifacts: a flaky
laptop captures a run, ships the file somewhere, replays offline.

**Format.** A session is newline-delimited JSON. Line 1 is a
`SessionHeader` (`kind: "signalscope-session"`, `format_version`,
`created_at`, `tool_version`, optional operator `label`). Every
subsequent line is a `SessionRow::Envelope` carrying the bus envelope
verbatim — same `EventId`, same wall-clock `at`, same `source`, same
typed event payload. Lifecycle transitions, observation confidence,
sensor-health distinctions all survive the round-trip. The tagged-
enum row framing means we can add `SessionRow::Marker` /
`SessionRow::Note` later without breaking readers.

The format is deliberately not a database. No SQLite, no binary
framing, no compression. `tail -f`, `jq`, `wc -l` all work. When
those constraints start hurting we will revisit, but inspectability
and forward-compatibility win today.

**Code.** Two new modules in `signalscope-core`:

- `session` — `SessionWriter` (cloneable handle backed by
  `BufWriter<File>` behind a `Mutex`; per-row flush so abrupt
  termination loses at most the most recent observation, never the
  tail). `SessionReader` (streaming iterator that reads + validates
  the header on construction and yields envelopes in file order).
  `spawn_recorder(bus, writer)` — one async task that seeds the
  backlog and then drains the bus subscription into the file.
  `SessionReadError` distinguishes missing header, wrong kind,
  future-newer `format_version`, malformed JSON line, and duplicate
  header.
- `source` — `EventSource` trait with `async fn
  next_envelope() -> Option<Arc<Envelope>>`. Implementations: the
  bus `Subscription` (live) and `FileEventSource` (replay-from-
  file). This is the smallest interface that lets future code be
  written generically over "live or replayed" without committing to
  any replay UI.

**Binary.** The `signalscope` binary grew subcommands:

- `signalscope observe [--record PATH] [--label TEXT]` — the live
  TUI. `--record` mirrors every envelope into a session file in
  parallel with the dashboard, so a live observation can be
  promoted to a forensic artifact without restarting. Bare
  invocation (no subcommand) defaults to `observe`.
- `signalscope capture --output PATH [--label TEXT]` — headless
  recording. Same sensor + analysis stack, no ratatui. A 5-second
  stderr status line shows running counts per category so the
  operator can confirm the run is healthy. Ctrl-C stops cleanly
  and prints the elapsed duration + final path.

Arg parsing is hand-rolled — no `clap` dependency. The intent is
non-technical collection, not a daemon platform.

**Tests.** Five new tests in `signalscope-core`:

- `round_trip_preserves_envelopes_in_order` — write 5 envelopes,
  read them back, confirm `id` / `at` / `source` align.
- `round_trip_carries_dns_event_intact` — confirm the DNS payload
  shape survives (resolver, query, RTT, answered).
- `rejects_file_with_no_header` — `MissingHeader`.
- `rejects_future_format_version` — `UnsupportedVersion`.
- `rejects_wrong_kind` — `WrongKind`.

`cargo test --workspace`: 41/41 green (up from 36).

**What this buys.**

- "Capture now, analyze later" is now a real workflow.
- Forensic sessions are inspectable with stock UNIX tools.
- Replay foundations exist (a `FileEventSource` is interchangeable
  with a bus `Subscription` in spirit) without dragging a replay UI
  into Phase 1.
- The recording shape is the bus shape — no derived-only summaries
  flattened in. Future analyzers improving means re-running them
  over preserved sessions still works.

**Untouched.** Bus shape and invariants, lifecycle pipeline,
gateway/DNS sensors, observation confidence, macOS backend layering,
trend rules, RF occupancy panel, finding fingerprints. The change
is purely additive — a new module in core, a new module in the bin
crate, a new `--record` flag.

### 2026-05-28 — Claude Opus 4.7 (flat relevance-ranked occupancy)

**Demoted from Canon, verbatim:**

> - **RF environment** — ambient AP activity treated as sparse and
>   probabilistic. The panel is *anchored on the connected channel*: the
>   header line reads `connected ch44 · pressure: moderate · density
>   stable`, and the primary body is a per-band channel-occupancy
>   histogram with the connected channel marked. The individual-AP table
>   is demoted to a `d`-toggled detail view — modern macOS redacts SSIDs
>   and BSSIDs anyway, so identity rows are low value. Pressure is a
>   four-tier ladder (low / moderate / elevated / severe) computed from
>   the AP count on the connected channel.

(The same paragraph in `docs/architecture.md` was likewise replaced
with the new flat-ordering framing.)

**Problem.** The band-grouped histogram had a structural flaw: the
2.4 GHz section (often dense — 1, 6, 11 all in use) consumed three
high-up rows even when the operator was on 5 GHz or 6 GHz, pushing
the *actually relevant* connected channel below the fold on smaller
terminals. Operationally backwards.

**Pivot.** Flatten the histogram into a single relevance-ranked list,
keeping the connected channel as a hard anchor at the top regardless
of which band is loudest.

The ordering function (`ui::relevance_order`) ranks in four tiers:

1. Connected channel — always row 1.
2. Same-band-as-connected channels with AP count > 2 — sorted by
   numeric distance to the connected channel (close overlap matters
   more than far co-existence within band), tied by count desc.
3. Other-band channels with AP count > 2 — sorted by count desc,
   tied by channel number asc.
4. Background channels (≤2 APs) — sorted by count desc.

Each row carries its band as an annotation column (`   5 GHz`), so
band context survives the flattening — the operator still sees that
`ch11` is 2.4 GHz and `ch44` is 5 GHz without the header rows. The
connected row gets a `▸` glyph, bold styling, and a `· connected`
suffix.

The `BandSort` helper and band-section headers are gone. When the
panel overflows, the last visible row is a "X more · press 'd' for
full AP list" hint instead of silent truncation.

**Tests.** Six unit tests on `relevance_order` in the bin crate pin
the priority: connected-always-first, same-band-beats-busier-other-
band, same-band-orders-by-proximity, other-band-orders-by-count,
background-pushed-to-bottom, unconnected-falls-back-to-busiest.

**What this buys.**

- Connected-channel context never gets buried by a noisy 2.4 GHz block.
- The panel reads coherently on small terminals — 5–6 rows is enough
  to surface the connected channel + 2–3 same-band siblings + 1–2
  other-band highlights.
- The visual feel shifts from "spectrum enumeration" toward "what
  matters to *this* client right now."

**Untouched.** Bus shape, lifecycle pipeline, gateway/DNS sensors,
observation confidence, trend rules, macOS backend layering,
pressure tier ladder, density trend, AP detail mode.

`cargo test --workspace`: 36/36 green.

### 2026-05-28 — Claude Opus 4.7 (RF environment → occupancy instrument)

**Demoted from Canon, verbatim:**

> - **RF environment** — ambient AP activity around the host. Sparse,
>   probabilistic, frequently redacted on modern macOS. This is *the
>   weather*. The panel summarises density and busiest channel and shows
>   a calm trend indicator (`density rising` / `falling` / `stable`)
>   driven by the current `RfDensityTrend` finding state.

(The same paragraph in `docs/architecture.md` was likewise replaced
with the new occupancy-instrument framing.)

**Problem.** Modern macOS routinely redacts SSIDs and omits BSSIDs in
`SPAirPortDataType`, leaving the AP-row table reading as columns of
`<redacted>  —  ch6  -67 dBm`. Identity was the wrong organising
principle. Worse, the panel summary surfaced the *global* busiest
channel — but the operational question is "how hostile is the airspace
around *my* connection?", not "which channel is busiest globally?".

**Pivot.**

Analysis:

- `WifiState` gained `current_channel: Option<Channel>` so rules can
  ask the obvious question: "what channel am I sitting on?"
- `rf_congestion` rule reshaped to be *local*: counts neighbors only
  on the connected channel and fires from `Elevated` upward on the
  new `PressureTier` ladder. Fingerprint becomes
  `rf_congestion:ch<connected>`, so a roam to a quieter channel
  resolves the old finding and a roam into a busier one starts a new
  one — the lifecycle pipeline handles it for free.
- New `PressureTier { Low, Moderate, Elevated, Severe }` and
  `pressure_tier(count) -> tier` are re-exported from the analysis
  crate so the TUI can render the *same* ladder in the panel header
  without inventing parallel thresholds. Cutoffs (0-2, 3-5, 6-8, 9+)
  are deliberately coarse — finer gradations would over-claim
  certainty from sparse scan snapshots.
- Five new rule tests pin: tier ladder, silence-when-unassociated,
  silence-when-connected-channel-quiet-but-others-busy, fires-only-
  from-elevated, fingerprint-follows-connected-channel.

TUI:

- The RF environment panel now leads with a one-line "weather report"
  anchored on the connected channel
  (`connected ch44 · pressure: moderate · density stable`). The
  pressure phrase uses `tier_color()` so the operator gets the
  severity at a glance.
- The primary body is a per-band channel-occupancy histogram. Each
  channel is one row: `  ch44  ██████        6  ← connected`. Bars
  scale to the busiest channel currently in the scan; the connected
  channel is highlighted bold and gets the trailing arrow. Bands
  ordered 2.4 / 5 / 6 GHz; channels ordered ascending within band.
- The previous AP-row table is preserved but demoted behind a `d`
  toggle. `AppState::show_neighbor_detail` defaults `false`, the
  footer surfaces the current mode (`RF: occupancy` /
  `RF: AP table`), and the help overlay documents the toggle. This
  keeps identity-oriented inspection available without crowding the
  primary view.
- When the histogram overflows the panel, the last visible row is
  replaced with a `… press 'd' for full AP list` hint instead of
  silently truncating.

**What this buys.** The panel stays readable — and operationally
meaningful — under macOS redaction, because occupancy doesn't need
identities. It also reframes the question from "what exists" to
"what pressure am *I* under", which is what the operator actually
wanted to know.

**Untouched.** Bus shape, lifecycle pipeline, gateway/DNS sensors,
observation confidence, macOS backend layering, trend rules. The
pivot is contained to one rule + one panel + a couple of small
event-model exposures.

`cargo test --workspace`: 30/30 green.

### 2026-05-28 — Claude Opus 4.7 (connected link / RF environment split)

**Goal.** Strengthen the semantic distinction between the *current
lifeline* (the associated network as a longitudinal entity) and the
*ambient weather* (RF activity around the host). Begin introducing
trend awareness so the dashboard shifts from "what exists right now?"
toward "what is changing over time?"

**Event model.** `FindingKind` gained two trend kinds: `SignalTrend`
(connected-link RSSI drift) and `RfDensityTrend` (ambient AP-count
shift). Direction lives in the finding's fingerprint suffix.

**Analysis.** Two new rolling windows in `analysis/windows.rs`:

- `WifiSignalWindow` keeps an `(Option<Ssid>, Option<Bssid>)` identity
  and a rolling RSSI buffer; identity changes wipe the buffer. Exposes
  `associated_duration(now)` and `rssi_delta(lookback, now)` (recent
  half mean minus prior half mean, `None` until each half has ≥2
  samples — we refuse to claim a trend from a single reading).
- `RfEnvironmentWindow` records `(timestamp, ap_count)` per scan and
  exposes `density_delta(lookback, now)` with the same recent/prior
  halving.

Two new stateless rules over those windows:

- `signal_trend` — RSSI Δ over 90 s, threshold ±5 dB.
- `rf_density_trend` — AP-count Δ over 120 s, threshold ±3 APs.

Both ride the existing lifecycle pipeline, so "stabilising" is the
Resolved edge of a previously-active trend.

`AnalysisEngine::ingest` was rewritten to thread the envelope's
wall-clock `at` through to the windows. Seeding from the bus backlog
now preserves real time positions instead of compressing the whole
backlog to "now."

The engine also forgets the connected-link signal window when a
Wi-Fi `SensorHealth` event reports `HardwareDisabled`,
`BackendUnavailable`, or `PermissionDenied`, so the "Held" counter
doesn't keep accumulating against a connection we've lost visibility
into.

**TUI.**

- "Wi-Fi link" → "Connected link" (card title), "Nearby APs" → "RF
  environment". The semantic split is now legible at a glance.
- Connected-link card gained a longitudinal line ("Held 12m34s · Δ RSSI
  -3 dB / 60s") and a one-row recent-RSSI sparkline.
- RF environment card gained a one-row summary: "busiest ch6 (4 APs)
  · density stable", where the trend phrase reads off the currently
  active `RfDensityTrend` finding fingerprint (`rf_density_trend:
  rising` / `:falling` / absent → `stable`).
- `AppState` grew `connected_identity`, `connected_since`, and a
  `signal_history` ring (≤90 samples, ~15 min at 10 s cadence). All
  reset when the Wi-Fi sensor reports it can no longer see the
  hardware.

**Tests.** Five new window tests pin the new behaviours:
`signal_window_resets_on_association_change`,
`signal_delta_returns_none_until_enough_samples`,
`signal_delta_detects_drift`, `env_density_delta_detects_rise`,
`env_density_delta_returns_none_with_too_few_samples`. Workspace
total 25/25 green.

**What didn't change.** The bus shape, the lifecycle pipeline, the
gateway/DNS sensors, the sensor health surface, observation confidence,
the macOS backend layering. The pivot is contained to the analysis
windows + rules + a TUI rename and one new card row.

### 2026-05-28 — Claude Opus 4.7 (findings → transitions)

**Demoted from Canon, verbatim:**

> Findings carry `kind`, `headline`, `Confidence` in `0.0..=1.0`, and
> `evidence`. Rules are intentionally cautious hand-tuned heuristics — never
> "the system says X". The TUI shows confidence so the operator can judge.

**Problem.** The previous engine ran `rules::evaluate()` on every
gateway / DNS / scan event and republished every firing finding. With
the gateway sensor at 1 Hz, a single persistent condition (e.g. DNS
pathology) produced dozens of identical "find ..." lines per minute in
the event feed. Visually loud, operationally meaningless, and it
trained the operator to ignore findings entirely — the opposite of what
this surface is for.

**Pivot.** Findings are now state transitions, not heartbeats.

- `CorrelationFinding` gained `fingerprint: String`,
  `peak_confidence: Confidence`, `lifecycle: FindingLifecycle`,
  `first_seen: Timestamp`, `last_seen: Timestamp`. Active duration is
  a computed accessor.
- New `FindingLifecycle` enum: `Active`, `Escalating`, `Recovering`,
  `Resolved`.
- `rules.rs` is now stateless: each rule returns a `CandidateFinding`
  with a stable fingerprint (e.g. `"rf_congestion:ch11"`,
  `"gateway_instability:192.168.1.1"`, `"sticky_client:HomeAP"`,
  `"dns_pathology"`). Headlines are framing-neutral — the lifecycle
  layer decorates them.
- New `lifecycle.rs` owns a tiny per-fingerprint state table and
  decides what to publish:
    * new fingerprint → `Active` with `first_seen = now`,
    * existing fingerprint with confidence delta ≥ `material_delta`
      (0.15) and `min_cooldown` (15 s) elapsed → `Escalating` /
      `Recovering`,
    * fingerprint missing from candidates for `resolved_after` (20 s)
      → `Resolved`, then dropped.
  All other cycles emit nothing.
- The engine grew a 2 s safety-net tick so resolutions fire even with
  no incoming events, and `LifecycleConfig` is now configurable via
  `AnalysisEngine::with_lifecycle_config`.
- TUI: findings are keyed by fingerprint (so RF congestion on ch11
  and ch36 coexist instead of overwriting), dropped on `Resolved`, and
  the panel shows a lifecycle glyph (`●` / `↑` / `↓` / `○`) plus the
  active duration. The feed feed-line decorates the lifecycle on
  transition events, so the wall of red goes away.

**Tests.** Ten new lifecycle tests pin the transition semantics:
first-emit-is-Active, repeated-no-emit, sub-threshold-suppressed,
material-rise-escalates, material-drop-recovers, absent-resolves,
cooldown-suppresses-oscillation, brief-flicker-does-not-resolve,
different-fingerprints-are-independent, resolved-headline-suffix. All
green; total workspace tests now 20/20.

**What didn't change.** The Wi-Fi card, neighbor list, gateway and
DNS sparklines, event feed shape, sensor health panel, observation
confidence — all unchanged. The pivot is contained to analysis and a
~50 LOC TUI edit.

### 2026-05-28 — Claude Opus 4.7 (Wi-Fi backend pivot)

**Demoted from Canon, verbatim:**

> `signalscope-sensors` — `Sensor` trait + per-source adapters. Currently:
> Wi-Fi (macOS `airport`), gateway (`ping`), DNS (`hickory-resolver`).

> - Replace `airport`-based macOS Wi-Fi adapter (binary removed in macOS
>   14.4+). `system_profiler -xml SPAirPortDataType` is the likely path.

**What changed.** Replaced the single-binary `airport`-only Wi-Fi
adapter with a layered backend system under `wifi/macos/`:

- `WifiBackend` enum chosen at sensor startup: `SystemProfiler` first,
  `Airport` only if its binary still exists. Selection is logged once,
  never re-evaluated, never branched on outside this module.
- `system_profiler -xml SPAirPortDataType` parsed via the `plist` crate.
  Handles modern macOS realities the airport parser couldn't: SSID
  `<redacted>`, missing BSSID, missing neighbor RSSI, the actual Apple
  typo `pairport_security_mode_wpa3_transition` (no leading `s`), and
  the 6 GHz / Wi-Fi 6E channel format `"37 (6GHz, 160MHz)"`.

**Event model changes.** Three new things in `signalscope-events`:

- `ObservationConfidence { Direct, Inferred, Estimated, Stale }`,
  attached to `WifiObservation` and `NeighborAp`. Lets us be honest
  about redaction without inventing values.
- `NeighborAp::bssid` and `NeighborAp::rssi_dbm` are now `Option`. The
  field changes ripple to one place (sticky-client rule); analysis
  silently drops entries that lack the fields it needs.
- New `Event::SensorHealth` variant carrying
  `SensorState { Operational, BackendUnavailable, HardwareDisabled,
  PermissionDenied, ParseFailed, Stale }`, a backend name, and a free
  detail string. Sensors only publish on transition — no per-tick
  health spam.

**TUI changes.** Wi-Fi card title shows the active backend; an
inline warning banner shows the state when not `Operational`; the SSID
line displays `<redacted>` in a dim warning style and surfaces the
`Inferred` confidence tag. Neighbor list handles `None` RSSI by
sorting to the bottom with a dash; `None` BSSID renders as `—`.

**Fixture-driven parser.** Four fixtures under
`examples/fixtures/wifi/`: `associated` (Location Services granted),
`redacted` (modern default), `no_association`, `wifi_off`. All
synthetic SSIDs/BSSIDs. Six parser tests pin behavior so Apple output
drift doesn't silently degrade the dashboard.

**Cadence.** Single conservative interval of 10 s for the Wi-Fi
snapshot — `system_profiler` takes 1–2 s and we don't want it dominating
the runtime. Gateway and DNS cadences are unchanged.

`cargo test --workspace` runs 10 tests green; the bootstrap caveat from
the earlier entry that Wi-Fi data wouldn't populate on modern macOS is
now resolved at the architecture level (subject to Location Services
for non-redacted identifiers).

### 2026-05-28 — Claude Opus 4.7 (run script)

Added `scripts/run.sh` — a thin wrapper around `cargo run -p
signalscope-tui` that forwards any trailing CLI args to the binary and
honors `SIGNALSCOPE_PROFILE` (`release` default, `debug` opt-in). Keeps
the day-to-day invocation short and means the CI / docs only ever need
to reference `scripts/run.sh`.

### 2026-05-28 — Claude Opus 4.7 (cargo check follow-up)

**Bootstrap compiles clean.** A Rust toolchain became available
(Homebrew, cargo/rustc 1.95.0) and `cargo check --workspace` revealed six
small issues, all fixed in this exchange:

- `signalscope-analysis/src/rules.rs`: mixed `f32`/`f64` arithmetic in
  the gateway-instability and DNS-pathology confidence calculations.
  Resolved by promoting to `f64` and casting back to `f32` at the
  `Confidence::new` boundary.
- `signalscope-{wifi,gateway,dns}` sensors: `Sensor::spawn(self, ...)`
  partially moved `self.cfg` before calling `self.id()`. Trivial
  reorder.
- `signalscope-events::FindingKind`: missing `Hash` derive — needed
  because the TUI keys its findings panel by `FindingKind` in a
  `HashMap`. Added `Hash` to the derive list.
- `signalscope-tui::ui::card_block`: lifetime elision tied the returned
  `Block` to the temporary `format!(...)` String passed in. Changed the
  return type to `Block<'static>` (the body produces an owned title via
  `format!`, so the lifetime independence is honest).
- Two `dead_code` warnings on unused `samples()` iterators on the
  window structs removed — these were speculative and YAGNI applied.

`cargo test --workspace` now runs 4 tests (Wi-Fi parser, channel spec
parser, ping-RTT parser, ping-no-reply parser) green.

The earlier "no cargo check was possible" caveat from the bootstrap
entry is therefore resolved; the highest-priority follow-up is now to
actually run `signalscope` on a macOS host and iterate on the TUI feel.

### 2026-05-28 — Claude Opus 4.7

**Bootstrap of the repository.** Created the five-crate workspace, the
event model, the event bus, three sensors (Wi-Fi/macOS, gateway, DNS),
a small correlation engine with four rules (RF congestion, gateway
instability, DNS pathology, sticky client), and a ratatui dashboard with a
Wi-Fi card, neighbor list, gateway sparkline, DNS sparkline, findings
panel, and rolling event feed.

Choices worth recording:

- **`Sensor` trait kept minimal** (`id` + `spawn`). Rejected richer
  abstractions (async trait with associated streams, descriptor structs,
  registry/DI) on the grounds that we have three sensors and YAGNI
  applies. If the trait needs to grow later we'll know exactly what for.
- **Sparkline + summary line** chosen over a full chart widget for both
  gateway and DNS cards. Keeps vertical real-estate compact and matches
  the "flight-panel" feel called for in `CLAUDE.md`.
- **`airport` binary picked** for the macOS Wi-Fi adapter despite being
  removed in 14.4+. Reasoning: it's the simplest path that produces
  every field of `WifiObservation` on supported macOS versions, and the
  failure mode (warning + no Wi-Fi events) is graceful. Replacement is
  tracked in Open Questions above.
- **`ping(8)` shell-out for gateway** rather than raw ICMP. Avoids the
  capability/sudo dance for a bootstrap; the `Sensor` abstraction lets us
  swap the probe later without disturbing analysis or the TUI.
- **System resolver preferred for DNS** with Cloudflare fallback when
  system config can't be read. The sensor times the lookup itself
  (`Instant::now()`-bracketed) rather than trusting `hickory`'s internal
  timings, because pathological resolvers can return early.
- **Confidence scores deliberately cautious.** No rule fires above 0.9
  except under genuinely pathological inputs; the tone we want is "this
  *might* be why," not "the system has diagnosed your network."
- **No `cargo check` was possible** in this bootstrap environment — there
  is no Rust toolchain on the workstation that ran the bootstrap. The
  code was written carefully and the dependency versions pinned to
  widely-used recent releases, but the next human (or agent) to touch
  the repo should run `cargo check --workspace` and resolve any
  cosmetic compile errors before declaring the bootstrap green. This is
  the highest-priority follow-up.

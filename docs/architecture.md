# Architecture

This document captures the architectural reasoning that the code is meant to
embody. When you change the code, change this document, too.

## North star

SignalScope exists to answer one question:

> Why does the network environment feel bad right now?

Everything in the architecture should serve that question. We prefer
narratives over numbers, confidence over certainty, and timelines over
snapshots.

## Four-layer architecture

```
┌───────────────────────────────────────────────────────────────┐
│ presentation       ─── signalscope-tui                        │
│   • renders state; never reads sensors or rules directly      │
├───────────────────────────────────────────────────────────────┤
│ correlation        ─── signalscope-analysis                   │
│   • subscribes to the event bus                               │
│   • emits CorrelationFinding events                           │
├───────────────────────────────────────────────────────────────┤
│ event model        ─── signalscope-events / signalscope-core  │
│   • normalized, append-only, platform-agnostic                │
│   • in-memory bus with bounded backlog                        │
├───────────────────────────────────────────────────────────────┤
│ acquisition        ─── signalscope-sensors                    │
│   • thin platform adapters                                    │
│   • emit observations, never conclusions                      │
└───────────────────────────────────────────────────────────────┘
```

The dependency arrows point *up* — each layer knows only about layers below
it, never about layers above. Sensors don't know the TUI exists. Analysis
doesn't know about CoreWLAN. The TUI doesn't know about `airport(8)`.

## The event bus is the backbone

All observations become normalized timestamped events. Once published an
envelope is immutable. The bus performs two roles:

1. **Broadcast** to live subscribers (analysis, TUI).
2. **Bounded backlog** so newly-attached consumers (e.g. the TUI on startup,
   or analysis when seeding its rolling windows) can replay recent history.

The bus assigns monotonic `EventId`s so that future replay / persistence
features have a stable ordering primitive.

## Why semantic types, not platform mirrors

A tempting design is to define traits like `CoreWlanProvider` and let the
analysis layer call them. We refuse to do that. The analysis layer should
not know what CoreWLAN is — and importantly, neither should the *event
model*. Instead we normalize observations:

```rust
struct WifiObservation {
    interface: String,
    ssid: Option<Ssid>,
    bssid: Option<Bssid>,
    rssi_dbm: Option<i32>,
    noise_dbm: Option<i32>,
    channel: Option<Channel>,
    // ...
}
```

These fields describe *what was observed*, not *how it was observed*. A
Linux nl80211 adapter and a macOS `airport` adapter both fill in the same
struct; the analysis and TUI code does not change.

See [`sensor-model.md`](sensor-model.md) for adapter contracts.

## Correlation philosophy

Findings preserve ambiguity. Each finding carries:

- a `kind` (e.g. `RfCongestion`, `DnsPathology`),
- a stable `fingerprint` string,
- a one-line `headline`,
- a `Confidence` and a `peak_confidence` in `0.0..=1.0`,
- a short list of `evidence` strings,
- a `lifecycle` state (`Active` / `Escalating` / `Recovering` /
  `Resolved`),
- `first_seen` and `last_seen` timestamps.

We do not pretend to know ground truth. We do not run statistical models.
We do not invoke an LLM. The rules are hand-tuned heuristics that explain
themselves. Better rules will replace them.

## Connected link vs RF environment

SignalScope reasons about Wi-Fi at two distinct conceptual layers, and
the UI is shaped to mirror them:

- **Connected link** — the currently associated network as a
  longitudinal entity. RSSI, SNR, channel, PHY mode, and a "held for X"
  duration that resets when the (SSID, BSSID) identity changes. This is
  *my current lifeline*. Anything that disturbs it directly affects the
  operator. The card includes a small recent-RSSI sparkline and a
  Δ-over-60s callout for at-a-glance trend awareness.
- **RF environment** — ambient AP activity treated as sparse and
  probabilistic. This is *the weather*. The panel is *anchored on the
  connected channel* rather than the global busiest, because the
  operational question is "how hostile is the airspace around my
  connection?" The header reads `connected ch44 · pressure: moderate
  · density stable`. The body is a *flat, relevance-ranked* channel
  occupancy histogram — no band grouping — with rows ordered:
  connected channel → same-band channels (by proximity to connected)
  → other-band channels (by AP count) → background (≤2 APs). Each row
  carries its band annotation so context survives the flattening, and
  the connected row is marked with a `▸` glyph + bold + `· connected`
  suffix. This ordering keeps the connected-channel context onscreen
  even when 2.4 GHz is dense. Identity (SSID/BSSID) rows are demoted
  behind a `d` toggle — modern macOS frequently redacts them anyway,
  so occupancy is the more reliable layer. Pressure is a coarse
  four-tier ladder (`Low` / `Moderate` / `Elevated` / `Severe`) shared
  between analysis (which fires `RfCongestion` from `Elevated` upward)
  and the panel header (which surfaces every tier).

That conceptual split also lives in `signalscope-analysis`. Two new
windows in `analysis/windows.rs`:

- `WifiSignalWindow` records RSSI tied to an association identity. The
  window resets when the identity changes — a different connection has
  its own clock. Methods:
    * `associated_duration(now)` — wall-clock time since first sample,
    * `rssi_delta(lookback, now)` — recent-half minus prior-half mean
      RSSI; `None` until each half has ≥2 samples.
- `RfEnvironmentWindow` records `(timestamp, ap_count)` per scan and
  exposes `density_delta(lookback, now)` with the same recent/prior
  halving.

Two new rules consume those windows:

- `signal_trend` — RSSI Δ over 90 s, threshold ±5 dB. Fingerprint
  encodes direction (`signal_trend:<key>:degrading` vs `:recovering`)
  so a degradation that flips to a recovery resolves cleanly under the
  lifecycle pipeline instead of mutating the same entry.
- `rf_density_trend` — AP-count Δ over 120 s, threshold ±3 APs. Same
  direction-as-fingerprint pattern (`rf_density_trend:rising` vs
  `:falling`).

Both inherit the lifecycle suppression and the periodic 2 s safety-net
tick, so "stabilising" is automatically expressed as the Resolved edge
of a previously-active trend.

## Findings are transitions, not heartbeats

A printf-loop dashboard re-emits "RF congestion!" every poll for as long
as the condition holds. SignalScope refuses to do this — it's noise, not
observability. Inside the analysis crate the work is split:

- `rules.rs` is **stateless**. Each rule looks at the current rolling
  state and returns a `CandidateFinding` (kind, fingerprint, headline,
  confidence, evidence) when its conditions are met.
- `lifecycle.rs` is **stateful**. It keeps a small per-fingerprint table
  with `first_seen`, `last_seen`, last-emitted confidence, and peak
  confidence. On each step it compares the new candidate set against
  the active set and emits `CorrelationFinding`s onto the bus only at
  *transitions*:
    * a fingerprint not previously active → `Active`,
    * an active fingerprint whose confidence moved by more than
      `material_delta` → `Escalating` or `Recovering`,
    * an active fingerprint absent from candidates for
      `resolved_after` → `Resolved`.

Quiet cycles emit nothing. A `min_cooldown` floor prevents oscillating
values from ping-ponging emissions even when the delta is material. The
defaults are `material_delta = 0.15`, `min_cooldown = 15s`,
`resolved_after = 20s`.

Because the engine runs both on incoming events *and* on a periodic
~2 s tick, resolutions still fire when sensors go quiet.

The TUI reads the lifecycle directly: findings are keyed by fingerprint,
dropped on `Resolved`, and the panel shows the lifecycle glyph and
active duration. The event feed sees one line per transition, not one
line per poll.

## Observation epistemics

Findings carry confidence; *observations* also carry confidence, via the
`ObservationConfidence` tag (`Direct`, `Inferred`, `Estimated`, `Stale`).
This is a deliberate epistemic-honesty primitive — different platforms
report different fidelities, and we don't want analysis or rendering to
treat an inferred value as if it were measured.

Concrete cases:

- macOS without Location Services redacts SSIDs to `<redacted>` and
  omits BSSIDs. The sensor still reports those observations, marks them
  `Inferred`, and the TUI shows a dim "(redacted source)" badge so the
  operator immediately understands they're not looking at a full-fidelity
  reading.
- A neighbor whose RSSI was not reported still ships in the scan because
  it contributes to channel density signal — also `Inferred`.

The rule of thumb: prefer **honest partial data** over **silent omission**
or **synthetic filler**.

## Degraded-state semantics

When acquisition fails, sensors publish a `SensorHealth` event rather
than synthesizing a fake observation or going silent. The state machine
is intentionally small:

| State                | Meaning                                                    |
| -------------------- | ---------------------------------------------------------- |
| `Operational`        | acquisition succeeded; observations are fresh              |
| `BackendUnavailable` | no acquisition path exists on this host                    |
| `HardwareDisabled`   | hardware reports off (e.g. Wi-Fi turned off)               |
| `PermissionDenied`   | telemetry source requires a permission we don't have       |
| `ParseFailed`        | the backend's output couldn't be interpreted               |
| `Stale`              | a transient failure; whatever we had is now out of date    |

Sensors only emit on transitions, not on every successful cycle — the
health stream is meant to be sparse and readable, not a heartbeat.
The TUI keeps the latest `SensorHealth` per `SensorId` and surfaces it
in the relevant card (today: the Wi-Fi card title and an inline banner).

## Replayability is a design constraint, not a feature

We use wall-clock `OffsetDateTime` (not `Instant`) on every envelope so the
stream is meaningful after a restart and can someday be:

- written as JSONL,
- replayed deterministically into the bus,
- compared across sessions.

We have not built persistence yet. We have left the door open.

## What we deliberately defer

| Capability                              | Why deferred                                 |
| --------------------------------------- | -------------------------------------------- |
| Linux nl80211/netlink sensor            | macOS-first bootstrap; design preserves it   |
| Packet capture / pcap                   | Out of Phase 1 scope; adds privilege concerns|
| Monitor mode                            | Niche, intrusive, platform-fragmented        |
| Persistent JSONL/SQLite store           | Premature until rules need history           |
| Replay engine                           | Depends on persistence                       |
| Roam-timeline visualization             | Depends on richer Wi-Fi sensor               |
| Topology inference                      | Depends on multiple sensors over time        |
| Plugin system                           | Not yet justified — sensors are a fixed set  |

These are tracked as future work, not as TODOs sprinkled through the code.

## Why ratatui / crossterm / tokio

- **ratatui** because the value proposition is *temporal information density*
  in a terminal — sparklines, compact cards, event feeds — which is what
  ratatui is good at.
- **crossterm** for portable terminal control.
- **tokio** because sensors are inherently I/O-bound (subprocesses, DNS,
  sockets) and need cancellation on shutdown. The single shared runtime
  avoids thread-per-sensor overhead.

We do not introduce async abstractions beyond `tokio::spawn` and
`broadcast`. There are no custom executors, no extension traits, no plugin
loaders. The codebase should stay readable by someone joining tomorrow.

## On platform portability

Initial target is macOS. Linux is a stretch goal. The split is intentional:

- `signalscope-events`, `signalscope-core`, `signalscope-analysis`,
  `signalscope-tui` are platform-agnostic.
- `signalscope-sensors` contains platform-specific adapters behind `#[cfg]`
  gates.

Adding Linux means adding adapters to `signalscope-sensors`. Nothing else
should need to change. If something else *does* need to change, we got the
abstraction wrong and should fix it.

We do not over-build for portability. If a macOS-specific implementation
yields substantially better observability, we prefer it and normalize the
output. Architectural purity must not block operational value.

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
- a one-line `headline`,
- a `Confidence` in `0.0..=1.0`,
- a short list of `evidence` strings.

We do not pretend to know ground truth. We do not run statistical models.
We do not invoke an LLM. The rules are hand-tuned heuristics that explain
themselves. Better rules will replace them.

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

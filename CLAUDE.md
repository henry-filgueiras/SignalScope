# SignalScope — CLAUDE.md

## Working guidelines for this repo

### Per-exchange checklist
An "exchange" is one user request, start to finish. Before declaring it done:

1. Update `DIRECTORS_NOTES.md` (see below).
2. Commit the work + notes update together. Smaller intermediate commits are fine if each has a meaningful message.
3. Do **not** `git push`.

### DIRECTORS_NOTES.md
Living design doc at repo root, two sections:

**Current Canon** — architecture, invariants, present-state truth. Edit in place to keep it reconciled with reality. When a fact stops being true, move the old text **verbatim** into the archive below instead of editing or deleting it in place.

**Resolved Dragons and Pivots** — append-only devlog of discoveries, fixes, pivots, and entries demoted from Canon. Never edit past entries.

#### Entry format
Prefix each new entry with ISO date and AI friendly name, e.g. `2026-04-17 — Claude Opus 4.7`. If no friendly-name mapping is available, use the model ID (e.g. `claude-opus-4-7`).

---

## Project Vision

SignalScope is a terminal observability tool for diagnosing Wi-Fi and local network quality issues.

The project goal is NOT to become:

* a generic packet sniffer,
* enterprise monitoring software,
* or a traditional sysadmin dashboard.

The goal is:

> Explain why the network environment feels bad.

SignalScope should correlate:

* RF conditions,
* roaming behavior,
* latency spikes,
* packet loss,
* DNS instability,
* interface telemetry,
* and environmental changes

into coherent operational narratives.

The project should feel:

* alive,
* temporal,
* reactive,
* interpretable,
* and trustworthy.

Avoid “wall of counters” UX.

Prioritize:

* timelines,
* sparklines,
* compact summaries,
* state transitions,
* event streams,
* and human-readable interpretations.

---

# Core Architectural Principles

## 1. Append-only event model

All observations should become normalized timestamped events.

Avoid tightly coupling:

* sensors,
* analysis,
* persistence,
* and rendering.

The event stream is the backbone of the system.

---

## 2. Replayability matters

The system should eventually support:

* replay,
* offline analysis,
* deterministic test scenarios,
* and historical comparisons.

Prefer designs that preserve temporal semantics.

---

## 3. Correlation is the product

Raw telemetry alone is insufficient.

SignalScope should gradually evolve lightweight correlation rules that identify likely operational issues.

Examples:

* AP overload suspicion
* WAN congestion suspicion
* roaming instability
* DNS pathology
* RF congestion
* sticky-client behavior
* intermittent gateway instability

Avoid fake certainty.

Confidence scoring is preferred over absolute claims.

---

## 4. Sensor modularity

Sensor collection should remain isolated from:

* TUI concerns,
* interpretation logic,
* and persistence.

Sensors should emit observations/events, not conclusions.

---

## 5. TUI quality matters

The terminal experience should feel polished and information-dense without becoming visually noisy.

Prefer:

* temporal views,
* sparklines,
* compact cards,
* responsive layouts,
* and event-centric UX.

Avoid:

* giant static tables,
* excessive color usage,
* or “hacker movie” aesthetics.

---

# Cross-Platform Architectural Direction

SignalScope is initially macOS-focused for rapid iteration and access to local development hardware.

However, the architecture should preserve future Linux portability.

The core value of SignalScope is NOT:

* CoreWLAN integration,
* Apple-specific telemetry,
* or platform-native APIs.

The core value is:

* temporal analysis,
* signal correlation,
* environmental interpretation,
* and operational observability.

The architecture should therefore preserve a clean separation between:

1. platform-specific signal acquisition
2. normalized observations/events
3. correlation and interpretation
4. presentation/rendering

---

# Important Design Principle

Normalize semantic observations, not platform APIs.

Avoid designing traits or models around:

* CoreWLAN terminology,
* Linux nl80211 structures,
* or OS-specific APIs.

Instead, define stable domain-level observations.

Good example:

```rust
struct WifiObservation {
    timestamp: Instant,
    bssid: String,
    ssid: Option<String>,
    rssi_dbm: i32,
    noise_dbm: Option<i32>,
    channel: Channel,
    channel_width: Option<ChannelWidth>,
}
```

Bad example:

```rust
trait CoreWlanProvider {
    fn scan(...)
}
```

The analysis layer should not know:

* CoreWLAN
* airport
* nl80211
* iw
* NetworkManager
* Windows Native Wi-Fi APIs

It should consume normalized observations/events.

---

# Sensor Layer Philosophy

Platform integrations should behave as thin adapters.

Examples:

* macOS CoreWLAN adapter
* Linux nl80211 adapter
* Linux iw/netlink adapter
* pcap adapter
* future Windows WLAN adapter

These adapters should emit normalized observations/events into the shared event model.

The analysis engine should remain platform-agnostic whenever practical.

---

# Important Constraint

Do not prematurely optimize for universal portability.

SignalScope should remain practical and shippable.

If a platform-specific implementation yields substantially better observability or UX, prefer correctness and usefulness first, then normalize carefully afterward.

Architecture purity should not block operational value.

---

# Technology Direction

Primary language:

* Rust

Primary TUI stack:

* ratatui
* crossterm

Async/runtime:

* tokio

Potential networking libraries:

* pnet
* pcap (later)
* netlink integrations later for Linux

macOS integration:

* CoreWLAN bindings
* system utilities if necessary during bootstrap

Persistence:

* JSONL initially
* SQLite later if justified

---

# Phase 1 Scope

Phase 1 intentionally excludes:

* monitor mode
* packet injection
* offensive tooling
* enterprise controller integration
* web dashboards
* distributed agents
* raw 802.11 frame analysis

The initial goal is:

* live Wi-Fi scan timelines,
* local telemetry,
* gateway/WAN quality monitoring,
* DNS timing visibility,
* and coherent event visualization.

---

# Development Guidance

Prefer:

* small vertical slices,
* visible UX progress,
* deterministic artifacts,
* and operational realism.

Do not prematurely optimize.

Do not over-engineer plugin systems.

Do not introduce unnecessary async complexity before clear need exists.

Focus on:

1. stable telemetry collection,
2. event normalization,
3. temporal visualization,
4. useful interpretation.

---

# Event Model Guidance

Events should represent:

* observations,
* transitions,
* anomalies,
* and environmental changes.

Prefer:

* append-only immutable events
* timestamped structures
* semantic naming
* optional fields for platform variance

Example categories:

* WifiObservation
* GatewayLatencyObservation
* DnsLatencyObservation
* RoamDetected
* PacketLossSpike
* InterfaceStateChanged
* CorrelationFinding

---

# Correlation Philosophy

SignalScope should avoid pretending to know ground truth.

Correlation findings should:

* expose supporting evidence,
* preserve ambiguity,
* and include confidence estimates.

Example:

```text
Likely RF congestion detected
Confidence: 0.72

Evidence:
- AP density increased on channel 149
- Retry counters increased 3.8x
- Gateway latency stable
- WAN latency stable
```

The system should behave like:

* an observability assistant,
* not an omniscient oracle.

---

# Desired Emotional Tone

SignalScope should feel like:

* a flight instrument panel,
* a network MRI,
* a systems observatory,
* or environmental telemetry for invisible infrastructure.

Not:

* a pentesting framework,
* generic enterprise sludge,
* or a toy dashboard.

---

# Future Possibilities (Not Yet)

Possible future directions:

* packet capture integration
* monitor mode support
* radiotap analysis
* AP roaming timelines
* topology inference
* replay sessions
* comparative historical analysis
* anomaly clustering
* remote telemetry nodes
* distributed environment snapshots

These are future possibilities, not current goals.

Maintain focus on:

* operational usefulness,
* observability clarity,
* and temporal awareness.

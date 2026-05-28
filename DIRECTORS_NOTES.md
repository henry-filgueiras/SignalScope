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
  bounded backlog), clock abstraction, tracing setup.
- `signalscope-sensors` — `Sensor` trait + per-source adapters. Currently:
  Wi-Fi (macOS, primary backend `system_profiler -xml SPAirPortDataType`,
  legacy `airport` retained as a fallback for pre-Sonoma hosts), gateway
  (`ping`), DNS (`hickory-resolver`).
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
- **RF environment** — ambient AP density treated as sparse and
  probabilistic. The TUI panel summarises busiest channel and shows a
  density trend indicator (`density rising` / `falling` / `stable`)
  derived from the current `RfDensityTrend` finding.

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

### Phase 1 scope

In: Wi-Fi link + scan, gateway probe, DNS probe, lightweight correlation,
TUI dashboard.
Out: packet capture, monitor mode, offensive tooling, persistence, replay,
web UI, plugin system.

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
- Decide when persistence is justified (JSONL first, SQLite later).
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

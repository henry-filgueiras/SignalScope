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

Findings carry `kind`, `headline`, `Confidence` in `0.0..=1.0`, and
`evidence`. Rules are intentionally cautious hand-tuned heuristics — never
"the system says X". The TUI shows confidence so the operator can judge.

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

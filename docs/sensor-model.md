# Sensor model

## What a sensor is

A sensor is a long-lived background task that:

1. acquires data from one source on a fixed cadence,
2. translates it into [`signalscope_events`] types,
3. publishes those events to the bus.

That's it. A sensor must not:

- read other sensors' output,
- run correlation rules,
- touch the UI,
- persist anything.

## The trait

```rust
pub trait Sensor: Send + 'static {
    fn id(&self) -> SensorId;
    fn spawn(self, bus: Arc<EventBus>) -> tokio::task::JoinHandle<()>;
}
```

That's the whole abstraction. Each sensor owns its own cadence (it picks
the tick interval), its own retry policy, and its own back-off behavior.
The scheduler is one tiny helper that calls `spawn` and remembers the
`JoinHandle` so we can abort cleanly on shutdown.

We deliberately avoided:

- a giant `Sensor` enum with a method per source,
- an async trait with associated stream types,
- a plugin / registry / DI system,
- a "sensor descriptor" type that wraps the trait.

If a sensor needs more structure later — health checks, capability probes,
configuration reloads — we will add the smallest thing that meets the need.

## Adapter contract

An adapter is the platform-specific guts of a sensor. The adapter's job is
to return *normalized* values:

```rust
// sensors/src/wifi/macos.rs
pub async fn current_link(interface: &str) -> Result<Option<WifiObservation>>;
pub async fn scan(interface: &str) -> Result<ScanResult>;
```

Notice the return types: `WifiObservation` and `ScanResult` are defined in
`signalscope-events`. Neither type has any CoreWLAN-shaped fields. Both
exist before any adapter is written, so they cannot drift toward the
shape of a particular OS.

When a Linux adapter lands, it must satisfy the same contract:

```rust
// sensors/src/wifi/linux.rs           (future work)
pub async fn current_link(interface: &str) -> Result<Option<WifiObservation>>;
pub async fn scan(interface: &str) -> Result<ScanResult>;
```

The `wifi/mod.rs` body is shared and picks the right adapter via `#[cfg]`.

## Sensors currently implemented

| Sensor      | Cadence    | What it emits                          | Implementation notes                              |
| ----------- | ---------- | -------------------------------------- | ------------------------------------------------- |
| `wifi`      | 2 s + 15 s | `WifiObservation`, `ScanResult`        | macOS: shells out to legacy `airport`             |
| `gateway`   | 1 s        | `GatewayLatencyObservation`            | shells out to `ping(8)`; discovers default GW     |
| `dns`       | 3 s        | `DnsLatencyObservation`                | uses `hickory-resolver`                           |

## Sensors we plan to add

| Sensor                  | Purpose                              | Status |
| ----------------------- | ------------------------------------ | ------ |
| `interface`             | link state transitions               | TODO   |
| `wifi.linux`            | nl80211 via netlink                  | TODO   |
| `pcap`                  | retry / RTS-CTS / TCP retransmits    | future |
| `monitor`               | 802.11 frames, beacon-rate analysis  | future |

## How a new sensor gets added

1. Decide what *semantic* event the new sensor emits. If `signalscope-events`
   does not already have a type for it, add one — and keep it
   platform-agnostic.
2. Add a module under `signalscope-sensors/src/` with:
   - a `Config` struct,
   - a public `Sensor`-implementing struct,
   - an internal `run` function that does the work.
3. Register it in `signalscope-tui/src/main.rs` with the scheduler.
4. If the analysis layer should treat the new event specially, extend the
   relevant rule in `signalscope-analysis`. The bus is the only contract;
   analysis sees the new event whether the TUI cares about it or not.

## Failure modes a sensor must handle

A sensor that crashes its task should not crash the program. It should:

- log at `warn` and try again on the next tick,
- never panic on parse errors,
- treat platform-tool absence (e.g. `airport` removed in macOS 14.4+) as a
  loud-but-survivable condition.

In particular, sensors must **not** publish synthetic "all-bad" events on
failure — that would poison correlation. Silence is better than fabrication.

## On privilege

Phase 1 sensors run as the invoking user. We do not require root. Anything
that needs raw sockets or kernel netlink is deferred until we have a story
for least-privilege capability acquisition (e.g. `cap_net_raw` on Linux, or
a small SUID helper on macOS).

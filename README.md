# SignalScope

> Explain why the network environment feels bad.

SignalScope is a terminal observability tool for diagnosing Wi-Fi and local
network quality issues. It correlates RF conditions, gateway latency, DNS
behavior, and (later) roaming, packet loss, and interface telemetry into
short, human-readable operational narratives.

It is **not** a packet sniffer, an enterprise monitoring product, or a
generic sysadmin dashboard. The goal is to make invisible local-network
trouble *legible*.

## Status

Phase 1 bootstrap. Initial target: macOS. Linux portability is a deliberate
architectural goal — see [`docs/architecture.md`](docs/architecture.md) and
[`docs/sensor-model.md`](docs/sensor-model.md).

## What works today

- live **connected-link** card (SSID, BSSID, RSSI, noise, SNR, channel,
  PHY mode) driven by `system_profiler -xml SPAirPortDataType` on macOS,
  with a legacy `airport` fallback. Includes a longitudinal "held for"
  duration, a 60 s Δ RSSI callout, and a recent-RSSI sparkline.
- **RF environment** panel anchored on the connected channel: header
  reads `connected ch44 · pressure: moderate · density stable`, body
  is a flat, relevance-ranked channel-occupancy histogram (connected
  first → same-band neighbours by proximity → other bands by AP count
  → background), each row band-annotated. The identity-oriented AP
  table is one `d` keypress away when you want it
- sensor-health surface — when Wi-Fi is off, redacted, or a backend is
  missing, the card shows the actual state instead of going silent
- per-observation confidence tags (`Direct` / `Inferred` / `Estimated`
  / `Stale`) so the UI can distinguish "we measured this" from
  "we inferred this" from "we used to know this"
- gateway latency probe + rolling sparkline
- DNS latency probe + rolling sparkline
- lightweight correlation rules with confidence + evidence, including
  longitudinal trend findings (`SignalTrend` for connected-link RSSI
  drift; `RfDensityTrend` for ambient AP-count change)
- lifecycle-aware findings (Active / Escalating / Recovering / Resolved)
  — the dashboard reacts to *transitions*, not every poll, so the feed
  stays calm even when a condition holds for minutes
- rolling event feed
- graceful resize, clean shutdown, structured file logging

## What's intentionally **not** here yet

- raw 802.11 frames, monitor mode, packet capture
- offensive tooling
- web dashboard / distributed agents
- packet injection
- persistent event storage (everything is in-memory)
- replay

These are tracked in `DIRECTORS_NOTES.md` and in `docs/architecture.md`.

## Building & running

```sh
# requires a recent stable Rust toolchain
cargo build --release
cargo run --release -p signalscope-tui
```

Environment variables:

| Variable             | Meaning                                          | Default        |
| -------------------- | ------------------------------------------------ | -------------- |
| `SIGNALSCOPE_LOG`    | `tracing` filter directive (e.g. `debug`)        | `info`         |
| `SIGNALSCOPE_LOG_DIR`| Directory for daily-rotated log files            | `./logs`       |

Keyboard:

| Key            | Action            |
| -------------- | ----------------- |
| `q` / `Esc`    | quit              |
| `Ctrl-C`       | quit              |
| `Tab` / `f`    | cycle focus       |
| `d`            | toggle RF view (occupancy histogram ↔ AP table) |
| `?` / `h`      | toggle help       |

## Workspace layout

```
signalscope-events/    normalized, platform-agnostic event/observation types
signalscope-core/      event bus, clock, logging — the runtime backbone
signalscope-sensors/   platform adapters that emit normalized events
signalscope-analysis/  correlation rules over the event stream
signalscope-tui/       ratatui dashboard (binary: `signalscope`)
```

The dependency arrow always points *down* this list: TUI knows about
everything, sensors know about events + core, events knows about nothing.

## Caveats

- The macOS Wi-Fi sensor's primary backend is
  `system_profiler -xml SPAirPortDataType`. Without Location Services
  permission for the invoking terminal, macOS redacts SSIDs as
  `<redacted>` and omits BSSIDs entirely. SignalScope still surfaces
  channel + signal density (useful for RF analysis) and tags the
  observation `Inferred` so the UI can show it's a partial reading.
  Grant Location Services to your terminal of choice to get full
  identifiers.
- The legacy `airport` backend is retained for pre-Sonoma hosts only.
  On macOS 14.4+ it doesn't exist; the sensor will pick
  `system_profiler` automatically.
- Gateway probes use `ping(8)`. Replacing this with a `socket2` ICMP path
  (no subprocess overhead) is intentional future work.

## License

MIT. See `LICENSE`.

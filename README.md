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

- live Wi-Fi link card (SSID, BSSID, RSSI, noise, SNR, channel)
- neighbor AP list with band and channel
- gateway latency probe + rolling sparkline
- DNS latency probe + rolling sparkline
- lightweight correlation rules with confidence + evidence
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

- The macOS Wi-Fi adapter shells out to the legacy `airport` binary. On
  macOS 14.4+ that binary was removed; SignalScope logs a warning and runs
  without live Wi-Fi telemetry. A `system_profiler` / `wdutil` adapter is
  intentional future work.
- Gateway probes use `ping(8)`. Replacing this with a `socket2` ICMP path
  (no subprocess overhead) is intentional future work.

## License

MIT. See `LICENSE`.

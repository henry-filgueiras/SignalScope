# Wi-Fi fixtures

Frozen `system_profiler -xml SPAirPortDataType` outputs used by the
`system_profiler` backend parser tests in `signalscope-sensors`.

All SSIDs and BSSIDs in these files are **synthetic**. Real captures
from human hosts contain identifying network information that should not
land in source control — when adding a fixture, anonymize first.

| File                                  | Scenario                                                                                  |
| ------------------------------------- | ----------------------------------------------------------------------------------------- |
| `system_profiler_associated.xml`      | macOS host with Location Services granted: full SSID/BSSID/signal visible.                |
| `system_profiler_redacted.xml`        | Modern macOS default: SSID `<redacted>`, no BSSIDs, neighbors carry channel only.         |
| `system_profiler_no_association.xml`  | Wi-Fi powered on but not currently associated; neighbor scan still populates.             |
| `system_profiler_wifi_off.xml`        | `spairport_status_off` — the parser should raise `BackendError::HardwareDisabled`.        |

## Adding a fixture

1. Capture: `system_profiler -xml SPAirPortDataType > capture.xml`
2. Anonymize all `_name`, `spairport_network_bssid`, and
   `spairport_wireless_mac_address` strings. Keep RSSI / channel /
   security values realistic.
3. Trim to the minimum that exercises the scenario (the parser doesn't
   need `spairport_supported_channels`, the version dictionary, etc.).
4. Add a test in `signalscope-sensors/src/wifi/macos/system_profiler.rs`
   that pins the parser's behavior on the new fixture.

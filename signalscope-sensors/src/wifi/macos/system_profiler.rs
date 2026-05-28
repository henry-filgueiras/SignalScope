//! Primary macOS Wi-Fi backend: `system_profiler -xml SPAirPortDataType`.
//!
//! The output is a property list (XML); we parse it with the `plist` crate
//! and walk to `_items[0].spairport_airport_interfaces[<iface>]`. Most
//! fields are optional — modern macOS redacts SSIDs as `"<redacted>"` and
//! omits BSSIDs entirely unless the invoking process has been granted
//! Location Services. The parser tolerates all of that and downgrades
//! `ObservationConfidence` rather than fabricating values.

use std::time::Duration;

use plist::{Dictionary, Value};
use signalscope_events::{
    BandClass, Bssid, Channel, ChannelWidth, NeighborAp, ObservationConfidence, ScanResult,
    Security, Ssid, WifiObservation,
};
use tokio::process::Command;

use super::{BackendError, WifiSnapshot};

const SYSTEM_PROFILER_BIN: &str = "/usr/sbin/system_profiler";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

const REDACTED: &str = "<redacted>";

pub async fn snapshot(interface: &str) -> Result<WifiSnapshot, BackendError> {
    let bytes = run().await?;
    parse(&bytes, interface)
}

async fn run() -> Result<Vec<u8>, BackendError> {
    let fut = Command::new(SYSTEM_PROFILER_BIN)
        .args(["-xml", "SPAirPortDataType"])
        .output();
    let out = match tokio::time::timeout(COMMAND_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BackendError::BinaryMissing(SYSTEM_PROFILER_BIN.into()));
        }
        Ok(Err(e)) => return Err(BackendError::Io(e)),
        Err(_) => return Err(BackendError::Timeout),
    };
    if !out.status.success() {
        return Err(BackendError::Other(format!(
            "system_profiler exited {}",
            out.status
        )));
    }
    Ok(out.stdout)
}

/// Pure parser. Public-in-crate so tests can drive it against fixtures
/// without launching a subprocess.
pub(crate) fn parse(xml: &[u8], interface: &str) -> Result<WifiSnapshot, BackendError> {
    let root: Value =
        plist::from_bytes(xml).map_err(|e| BackendError::Parse(e.to_string()))?;
    let iface = find_interface(&root, interface).ok_or_else(|| {
        BackendError::Other(format!(
            "interface '{interface}' not present in SPAirPortDataType"
        ))
    })?;

    // Status string check first — distinguishes "Wi-Fi off" from "couldn't
    // associate". The platform uses `spairport_status_off` for the off
    // case.
    if let Some(status) = iface
        .get("spairport_status_information")
        .and_then(Value::as_string)
    {
        if status.eq_ignore_ascii_case("spairport_status_off") {
            return Err(BackendError::HardwareDisabled);
        }
    }

    let link = parse_current_network(iface, interface);
    let scan = parse_neighbors(iface, interface);
    Ok(WifiSnapshot { link, scan })
}

/// Walk the plist root down to the requested interface's dictionary. The
/// layout is:
///
/// ```text
/// Array
///  └─ Dict { _dataType: "SPAirPortDataType", _items: Array
///       └─ Dict { spairport_airport_interfaces: Array
///            └─ Dict { _name: "en0", ... }    ← we return this
///       }
///  }
/// ```
fn find_interface<'a>(root: &'a Value, interface: &str) -> Option<&'a Dictionary> {
    let top = root.as_array()?;
    let entry = top
        .iter()
        .find_map(|v| {
            let d = v.as_dictionary()?;
            if d.get("_dataType").and_then(Value::as_string)? == "SPAirPortDataType" {
                Some(d)
            } else {
                None
            }
        })
        .or_else(|| top.first().and_then(Value::as_dictionary))?;

    let items = entry.get("_items").and_then(Value::as_array)?;
    for item in items {
        let dict = match item.as_dictionary() {
            Some(d) => d,
            None => continue,
        };
        let interfaces = match dict
            .get("spairport_airport_interfaces")
            .and_then(Value::as_array)
        {
            Some(a) => a,
            None => continue,
        };
        for iface in interfaces {
            let d = match iface.as_dictionary() {
                Some(d) => d,
                None => continue,
            };
            if d.get("_name").and_then(Value::as_string) == Some(interface) {
                return Some(d);
            }
        }
    }
    None
}

fn parse_current_network(iface: &Dictionary, interface: &str) -> Option<WifiObservation> {
    let net = iface
        .get("spairport_current_network_information")
        .and_then(Value::as_dictionary)?;

    // A station entry with *only* `spairport_network_type` and nothing else
    // (seen on `awdl0`) is uninteresting noise — skip it.
    if net.len() < 2 {
        return None;
    }

    let (ssid, ssid_confidence) = parse_ssid(net.get("_name").and_then(Value::as_string));
    let bssid = net
        .get("spairport_network_bssid")
        .and_then(Value::as_string)
        .map(Bssid::new);

    let (rssi_dbm, noise_dbm) = parse_signal_noise(
        net.get("spairport_signal_noise")
            .and_then(Value::as_string),
    );

    let tx_rate_mbps = net
        .get("spairport_network_rate")
        .and_then(value_as_f32);

    let channel = net
        .get("spairport_network_channel")
        .and_then(Value::as_string)
        .and_then(parse_channel_string);

    let security = net
        .get("spairport_security_mode")
        .and_then(Value::as_string)
        .map(parse_security_string);

    let phy_mode = net
        .get("spairport_network_phymode")
        .and_then(Value::as_string)
        .map(str::to_string);

    // If the source redacted the SSID, downgrade the whole observation's
    // confidence — the operator should see at a glance that this isn't a
    // full-fidelity reading.
    let confidence = if ssid_confidence == ObservationConfidence::Inferred || bssid.is_none() {
        ObservationConfidence::Inferred
    } else {
        ObservationConfidence::Direct
    };

    Some(WifiObservation {
        interface: interface.to_string(),
        ssid,
        bssid,
        rssi_dbm,
        noise_dbm,
        tx_rate_mbps,
        channel,
        security,
        phy_mode,
        confidence,
    })
}

fn parse_neighbors(iface: &Dictionary, interface: &str) -> Option<ScanResult> {
    let arr = iface
        .get("spairport_airport_other_local_wireless_networks")
        .and_then(Value::as_array)?;

    let mut neighbors = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(d) = v.as_dictionary() else { continue };
        let (ssid, ssid_confidence) = parse_ssid(d.get("_name").and_then(Value::as_string));
        let bssid = d
            .get("spairport_network_bssid")
            .and_then(Value::as_string)
            .map(Bssid::new);
        let channel = d
            .get("spairport_network_channel")
            .and_then(Value::as_string)
            .and_then(parse_channel_string);
        let (rssi_dbm, _noise) = parse_signal_noise(
            d.get("spairport_signal_noise")
                .and_then(Value::as_string),
        );
        let security = d
            .get("spairport_security_mode")
            .and_then(Value::as_string)
            .map(parse_security_string);
        let phy_mode = d
            .get("spairport_network_phymode")
            .and_then(Value::as_string)
            .map(str::to_string);

        // Drop entries that carry essentially nothing useful. Channel
        // alone is enough to keep an entry — it powers density analysis.
        if ssid.is_none() && channel.is_none() && rssi_dbm.is_none() && bssid.is_none() {
            continue;
        }

        let confidence = if ssid_confidence == ObservationConfidence::Inferred
            || bssid.is_none()
            || rssi_dbm.is_none()
        {
            ObservationConfidence::Inferred
        } else {
            ObservationConfidence::Direct
        };

        neighbors.push(NeighborAp {
            bssid,
            ssid,
            rssi_dbm,
            channel,
            security,
            phy_mode,
            confidence,
        });
    }
    Some(ScanResult {
        interface: interface.to_string(),
        neighbors,
    })
}

fn parse_ssid(raw: Option<&str>) -> (Option<Ssid>, ObservationConfidence) {
    match raw {
        Some(s) if s.eq_ignore_ascii_case(REDACTED) || s.is_empty() => {
            (None, ObservationConfidence::Inferred)
        }
        Some(s) => (Some(Ssid::new(s)), ObservationConfidence::Direct),
        None => (None, ObservationConfidence::Inferred),
    }
}

/// Parse `"-42 dBm / -97 dBm"` into `(rssi, noise)`. Either side may be
/// missing; callers should treat `None` as "unknown", not "zero".
fn parse_signal_noise(raw: Option<&str>) -> (Option<i32>, Option<i32>) {
    let Some(s) = raw else { return (None, None) };
    let mut parts = s.split('/');
    let rssi = parts.next().and_then(extract_signed_dbm);
    let noise = parts.next().and_then(extract_signed_dbm);
    (rssi, noise)
}

fn extract_signed_dbm(token: &str) -> Option<i32> {
    let cleaned: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || c.is_whitespace())
        .collect();
    cleaned.trim().parse().ok()
}

/// Parse the SPAirPort channel string. Common shapes:
///
/// * `"149 (5GHz, 80MHz)"` — preferred, has band + width
/// * `"6 (2GHz, 20MHz)"`
/// * `"1 (6GHz, 80MHz)"` — Wi-Fi 6E
/// * `"149,80"`           — older variant carrying just number,width
/// * `"149"`              — minimal
fn parse_channel_string(s: &str) -> Option<Channel> {
    let s = s.trim();
    let mut iter = s.chars();
    let number: u16 = (&mut iter)
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;

    let rest = &s[s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len())..];
    let lower = rest.to_ascii_lowercase();

    let band = if lower.contains("6ghz") || lower.contains("6 ghz") {
        BandClass::SixGhz
    } else if lower.contains("5ghz") || lower.contains("5 ghz") {
        BandClass::FiveGhz
    } else if lower.contains("2ghz") || lower.contains("2 ghz") || lower.contains("2.4") {
        BandClass::TwoPointFourGhz
    } else {
        BandClass::from_channel_number(number)
    };

    let width = extract_width_mhz(&lower).and_then(channel_width);

    Some(Channel {
        number,
        band,
        width,
    })
}

fn extract_width_mhz(lower: &str) -> Option<u16> {
    // Preferred shape: digits immediately preceding "mhz".
    if let Some(mhz_pos) = lower.find("mhz") {
        let prefix = &lower.as_bytes()[..mhz_pos];
        let digits: String = prefix
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_digit())
            .map(|b| *b as char)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if let Ok(n) = digits.parse() {
            return Some(n);
        }
    }
    // Legacy shape ("149,80"): first run of digits after the channel
    // number. We've already stripped the channel number itself, so this
    // skips over the comma/whitespace and reads the width.
    let trimmed = lower.trim_start_matches(|c: char| !c.is_ascii_digit());
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn channel_width(mhz: u16) -> Option<ChannelWidth> {
    match mhz {
        20 => Some(ChannelWidth::Mhz20),
        40 => Some(ChannelWidth::Mhz40),
        80 => Some(ChannelWidth::Mhz80),
        160 => Some(ChannelWidth::Mhz160),
        _ => None,
    }
}

/// Map `spairport_security_mode_*` strings to a `Security`. Apple has at
/// least one typo in the wild (`pairport_security_mode_wpa3_transition`
/// — missing leading `s`); we accept both.
fn parse_security_string(raw: &str) -> Security {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("wpa3_transition") {
        Security::Wpa3Transition
    } else if lower.contains("wpa3") {
        Security::Wpa3
    } else if lower.contains("wpa2") {
        Security::Wpa2
    } else if lower.contains("wpa") {
        Security::Wpa
    } else if lower.contains("wep") {
        Security::Wep
    } else if lower.contains("_none") || lower == "none" || lower.is_empty() {
        Security::Open
    } else {
        Security::Unknown
    }
}

fn value_as_f32(v: &Value) -> Option<f32> {
    if let Some(i) = v.as_signed_integer() {
        return Some(i as f32);
    }
    if let Some(i) = v.as_unsigned_integer() {
        return Some(i as f32);
    }
    if let Some(r) = v.as_real() {
        return Some(r as f32);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ASSOCIATED: &[u8] =
        include_bytes!("../../../../examples/fixtures/wifi/system_profiler_associated.xml");
    const FIXTURE_REDACTED: &[u8] =
        include_bytes!("../../../../examples/fixtures/wifi/system_profiler_redacted.xml");
    const FIXTURE_OFF: &[u8] =
        include_bytes!("../../../../examples/fixtures/wifi/system_profiler_wifi_off.xml");
    const FIXTURE_NO_ASSOCIATION: &[u8] =
        include_bytes!("../../../../examples/fixtures/wifi/system_profiler_no_association.xml");

    #[test]
    fn parses_modern_associated_link() {
        let snap = parse(FIXTURE_ASSOCIATED, "en0").expect("parse");
        let link = snap.link.expect("expected link");
        assert_eq!(link.interface, "en0");
        assert_eq!(link.ssid.as_ref().map(|s| s.as_str()), Some("HomeAP"));
        assert_eq!(
            link.bssid.as_ref().map(|b| b.as_str()),
            Some("aa:bb:cc:dd:ee:01")
        );
        assert_eq!(link.rssi_dbm, Some(-42));
        assert_eq!(link.noise_dbm, Some(-97));
        let ch = link.channel.expect("channel");
        assert_eq!(ch.number, 149);
        assert_eq!(ch.band, BandClass::FiveGhz);
        assert_eq!(ch.width, Some(ChannelWidth::Mhz80));
        assert_eq!(link.security, Some(Security::Wpa2));
        assert_eq!(link.phy_mode.as_deref(), Some("802.11ax"));
        assert_eq!(link.confidence, ObservationConfidence::Direct);

        let scan = snap.scan.expect("scan");
        assert!(!scan.neighbors.is_empty());
        // The Wi-Fi 6E neighbor exercises the 6 GHz parse path.
        let six = scan
            .neighbors
            .iter()
            .find(|n| n.channel.is_some_and(|c| c.band == BandClass::SixGhz))
            .expect("expected a 6 GHz neighbor");
        assert!(matches!(
            six.channel.unwrap().width,
            Some(ChannelWidth::Mhz160) | Some(ChannelWidth::Mhz80)
        ));
    }

    #[test]
    fn parses_modern_redacted_link() {
        let snap = parse(FIXTURE_REDACTED, "en0").expect("parse");
        let link = snap.link.expect("link");
        assert_eq!(link.ssid, None, "redacted SSID should normalize to None");
        assert_eq!(link.bssid, None, "no BSSID without location permission");
        assert_eq!(
            link.confidence,
            ObservationConfidence::Inferred,
            "redacted-source observations must be tagged Inferred"
        );
        assert_eq!(link.rssi_dbm, Some(-58));

        let scan = snap.scan.expect("scan");
        // Even when names + BSSIDs are missing, neighbors carry useful
        // density signal.
        assert!(scan.neighbors.len() >= 3);
        assert!(scan.neighbors.iter().all(|n| n.bssid.is_none()));
        assert!(scan
            .neighbors
            .iter()
            .all(|n| n.confidence == ObservationConfidence::Inferred));
    }

    #[test]
    fn parses_apple_wpa3_transition_typo() {
        // Real Apple output has at least one record where the security
        // string is missing its leading `s` — make sure we still map it.
        assert_eq!(
            parse_security_string("pairport_security_mode_wpa3_transition"),
            Security::Wpa3Transition
        );
        assert_eq!(
            parse_security_string("spairport_security_mode_wpa3_transition"),
            Security::Wpa3Transition
        );
    }

    #[test]
    fn wifi_off_maps_to_hardware_disabled() {
        match parse(FIXTURE_OFF, "en0") {
            Err(BackendError::HardwareDisabled) => {}
            other => panic!("expected HardwareDisabled, got {other:?}"),
        }
    }

    #[test]
    fn no_association_returns_scan_only() {
        let snap = parse(FIXTURE_NO_ASSOCIATION, "en0").expect("parse");
        assert!(
            snap.link.is_none(),
            "no associated network → no link observation"
        );
        let scan = snap.scan.expect("scan");
        assert!(
            !scan.neighbors.is_empty(),
            "neighbors should still be reported"
        );
    }

    #[test]
    fn channel_string_handles_band_and_width() {
        let c = parse_channel_string("149 (5GHz, 80MHz)").unwrap();
        assert_eq!(c.number, 149);
        assert_eq!(c.band, BandClass::FiveGhz);
        assert_eq!(c.width, Some(ChannelWidth::Mhz80));

        let c = parse_channel_string("1 (6GHz, 80MHz)").unwrap();
        assert_eq!(c.band, BandClass::SixGhz);

        let c = parse_channel_string("6 (2GHz, 20MHz)").unwrap();
        assert_eq!(c.band, BandClass::TwoPointFourGhz);

        let c = parse_channel_string("149,80").unwrap();
        assert_eq!(c.number, 149);
        assert_eq!(c.width, Some(ChannelWidth::Mhz80));

        let c = parse_channel_string("6").unwrap();
        assert_eq!(c.number, 6);
    }

    #[test]
    fn signal_noise_extraction() {
        assert_eq!(
            parse_signal_noise(Some("-42 dBm / -97 dBm")),
            (Some(-42), Some(-97))
        );
        assert_eq!(parse_signal_noise(Some("-42 dBm")), (Some(-42), None));
        assert_eq!(parse_signal_noise(None), (None, None));
    }
}

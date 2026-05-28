//! macOS Wi-Fi adapter.
//!
//! ## What this adapter does
//!
//! Shells out to the legacy `airport` binary at
//! `/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport`
//! and parses its human-readable output into normalized
//! [`signalscope_events::WifiObservation`] / [`signalscope_events::ScanResult`].
//!
//! ## Caveats
//!
//! * `airport` was deprecated/removed in macOS Sonoma 14.4. On hosts where
//!   it's missing, the sensor emits nothing and logs a single warning. A
//!   future adapter should prefer `system_profiler -xml SPAirPortDataType`,
//!   `wdutil info`, or direct CoreWLAN FFI.
//! * Output formats vary across macOS versions; the parser is forgiving but
//!   not exhaustive.
//! * Some fields (noise floor, channel width) are unavailable on recent
//!   macOS versions even when `airport` is present.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use signalscope_events::{
    BandClass, Bssid, Channel, ChannelWidth, NeighborAp, ScanResult, Security, Ssid,
    WifiObservation,
};
use tokio::process::Command;

const AIRPORT_BIN: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

/// Read `airport -I` (current associated link). Returns `None` when the
/// interface exists but is not associated.
pub async fn current_link(interface: &str) -> Result<Option<WifiObservation>> {
    let out = run_airport(&[interface, "-I"]).await?;
    if out.trim().is_empty() || out.contains("AirPort: Off") {
        return Ok(None);
    }
    Ok(Some(parse_link(interface, &out)))
}

/// Perform a neighbor scan (`airport -s`).
pub async fn scan(interface: &str) -> Result<ScanResult> {
    let out = run_airport(&[interface, "-s"]).await?;
    let neighbors = parse_scan(&out);
    Ok(ScanResult {
        interface: interface.to_string(),
        neighbors,
    })
}

async fn run_airport(args: &[&str]) -> Result<String> {
    let fut = Command::new(AIRPORT_BIN).args(args).output();
    let output = match tokio::time::timeout(COMMAND_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "airport binary not found at {AIRPORT_BIN} (removed in macOS 14.4+)"
            ));
        }
        Ok(Err(e)) => return Err(e).context("invoking airport"),
        Err(_) => return Err(anyhow!("airport command timed out")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("airport exited with {}: {}", output.status, stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_link(interface: &str, text: &str) -> WifiObservation {
    let mut obs = WifiObservation {
        interface: interface.to_string(),
        ssid: None,
        bssid: None,
        rssi_dbm: None,
        noise_dbm: None,
        tx_rate_mbps: None,
        channel: None,
        security: None,
    };

    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "SSID" => obs.ssid = Some(Ssid::new(value)),
            "BSSID" => obs.bssid = Some(Bssid::new(value)),
            "agrCtlRSSI" => obs.rssi_dbm = value.parse().ok(),
            "agrCtlNoise" => obs.noise_dbm = value.parse().ok(),
            "lastTxRate" => obs.tx_rate_mbps = value.parse().ok(),
            "channel" => obs.channel = parse_channel_spec(value),
            "link auth" => obs.security = Some(parse_security(value)),
            _ => {}
        }
    }

    obs
}

/// `airport -s` is whitespace-aligned columns:
///
/// ```text
///                             SSID BSSID             RSSI CHANNEL HT CC SECURITY
///                            HomeAP aa:bb:cc:dd:ee:ff -54  149,80   Y  US WPA2(PSK/AES/AES)
/// ```
fn parse_scan(text: &str) -> Vec<NeighborAp> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };

    // Find column starts from the header so SSIDs containing spaces don't
    // break parsing.
    let cols = ["SSID", "BSSID", "RSSI", "CHANNEL", "HT", "CC", "SECURITY"];
    let starts: Vec<usize> = cols
        .iter()
        .filter_map(|name| header.find(name))
        .collect();
    if starts.len() < 4 {
        return Vec::new();
    }

    let mut neighbors = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        // Use the column starts to slice.
        let get = |i: usize| -> Option<&str> {
            let start = *starts.get(i)?;
            let end = starts.get(i + 1).copied().unwrap_or(line.len());
            line.get(start..end.min(line.len())).map(str::trim)
        };

        let ssid_field = get(0).unwrap_or("").to_string();
        let bssid_field = get(1).unwrap_or("");
        let rssi_field = get(2).unwrap_or("");
        let channel_field = get(3).unwrap_or("");
        let security_field = get(6).unwrap_or("");

        if bssid_field.is_empty() || rssi_field.is_empty() {
            continue;
        }

        let Ok(rssi) = rssi_field.parse::<i32>() else {
            continue;
        };

        neighbors.push(NeighborAp {
            bssid: Bssid::new(bssid_field),
            ssid: if ssid_field.is_empty() {
                None
            } else {
                Some(Ssid::new(ssid_field.trim()))
            },
            rssi_dbm: rssi,
            channel: parse_channel_spec(channel_field),
            security: Some(parse_security(security_field)),
        });
    }
    neighbors
}

fn parse_channel_spec(s: &str) -> Option<Channel> {
    // Formats observed:
    //   "149"
    //   "149,80"      (channel, width MHz)
    //   "149,80+"     (with channel-width annotation)
    //   "149 (5 GHz, 80 MHz)"
    let cleaned = s.split('(').next().unwrap_or(s).trim();
    let mut parts = cleaned.splitn(2, |c: char| c == ',' || c.is_whitespace());
    let number: u16 = parts.next()?.trim().parse().ok()?;
    let width = parts
        .next()
        .and_then(|rest| rest.trim_start_matches(|c: char| !c.is_ascii_digit()).chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u16>().ok())
        .and_then(channel_width);

    Some(Channel {
        number,
        band: BandClass::from_channel_number(number),
        width,
    })
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

fn parse_security(s: &str) -> Security {
    let lower = s.to_ascii_lowercase();
    if lower.contains("wpa3") {
        Security::Wpa3
    } else if lower.contains("wpa2") {
        Security::Wpa2
    } else if lower.contains("wpa") {
        Security::Wpa
    } else if lower.contains("wep") {
        Security::Wep
    } else if lower.contains("none") || lower.is_empty() {
        Security::Open
    } else {
        Security::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_parser_extracts_core_fields() {
        let sample = r#"
     agrCtlRSSI: -54
     agrExtRSSI: 0
    agrCtlNoise: -92
    agrExtNoise: 0
          state: running
        op mode: station
     lastTxRate: 526
        maxRate: 1300
      802.11 auth: open
        link auth: wpa2-psk
            BSSID: aa:bb:cc:dd:ee:ff
             SSID: HomeAP
              MCS: 9
          channel: 149,80
"#;
        let obs = parse_link("en0", sample);
        assert_eq!(obs.ssid.as_ref().map(|s| s.as_str()), Some("HomeAP"));
        assert_eq!(obs.bssid.as_ref().map(|b| b.as_str()), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(obs.rssi_dbm, Some(-54));
        assert_eq!(obs.noise_dbm, Some(-92));
        assert_eq!(obs.tx_rate_mbps, Some(526.0));
        let ch = obs.channel.expect("channel");
        assert_eq!(ch.number, 149);
        assert_eq!(ch.band, BandClass::FiveGhz);
        assert_eq!(ch.width, Some(ChannelWidth::Mhz80));
    }

    #[test]
    fn channel_spec_parses_variants() {
        assert_eq!(parse_channel_spec("6").map(|c| c.number), Some(6));
        assert_eq!(parse_channel_spec("149,80").map(|c| c.number), Some(149));
        assert_eq!(parse_channel_spec("36,40").and_then(|c| c.width), Some(ChannelWidth::Mhz40));
    }
}

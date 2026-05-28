//! Legacy macOS Wi-Fi backend: the `airport` CLI.
//!
//! Kept for pre-Sonoma macOS hosts where the binary still ships and
//! actually returns BSSIDs / signal numbers without the modern privacy
//! redaction. On macOS 14.4+ the binary was removed; the backend selector
//! will simply not pick this path.

use std::time::Duration;

use signalscope_events::{
    BandClass, Bssid, Channel, ChannelWidth, NeighborAp, ObservationConfidence, ScanResult,
    Security, Ssid, WifiObservation,
};
use tokio::process::Command;

use super::{BackendError, WifiSnapshot};

const AIRPORT_BIN: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

pub async fn snapshot(interface: &str) -> Result<WifiSnapshot, BackendError> {
    let link_text = run_airport(&[interface, "-I"]).await?;
    let link = if link_text.contains("AirPort: Off") {
        return Err(BackendError::HardwareDisabled);
    } else if link_text.trim().is_empty() {
        None
    } else {
        Some(parse_link(interface, &link_text))
    };

    let scan_text = run_airport(&[interface, "-s"]).await?;
    let scan = Some(ScanResult {
        interface: interface.to_string(),
        neighbors: parse_scan(&scan_text),
    });

    Ok(WifiSnapshot { link, scan })
}

async fn run_airport(args: &[&str]) -> Result<String, BackendError> {
    let fut = Command::new(AIRPORT_BIN).args(args).output();
    let out = match tokio::time::timeout(COMMAND_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BackendError::BinaryMissing(AIRPORT_BIN.into()));
        }
        Ok(Err(e)) => return Err(BackendError::Io(e)),
        Err(_) => return Err(BackendError::Timeout),
    };
    if !out.status.success() {
        return Err(BackendError::Other(format!(
            "airport exited with {}",
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
        phy_mode: None,
        confidence: ObservationConfidence::Direct,
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

fn parse_scan(text: &str) -> Vec<NeighborAp> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols = ["SSID", "BSSID", "RSSI", "CHANNEL", "HT", "CC", "SECURITY"];
    let starts: Vec<usize> = cols.iter().filter_map(|name| header.find(name)).collect();
    if starts.len() < 4 {
        return Vec::new();
    }

    let mut neighbors = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
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
            bssid: Some(Bssid::new(bssid_field)),
            ssid: if ssid_field.is_empty() {
                None
            } else {
                Some(Ssid::new(ssid_field.trim()))
            },
            rssi_dbm: Some(rssi),
            channel: parse_channel_spec(channel_field),
            security: Some(parse_security(security_field)),
            phy_mode: None,
            confidence: ObservationConfidence::Direct,
        });
    }
    neighbors
}

fn parse_channel_spec(s: &str) -> Option<Channel> {
    let cleaned = s.split('(').next().unwrap_or(s).trim();
    let mut parts = cleaned.splitn(2, |c: char| c == ',' || c.is_whitespace());
    let number: u16 = parts.next()?.trim().parse().ok()?;
    let width = parts
        .next()
        .and_then(|rest| {
            rest.trim_start_matches(|c: char| !c.is_ascii_digit())
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u16>()
                .ok()
        })
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
        assert_eq!(
            obs.bssid.as_ref().map(|b| b.as_str()),
            Some("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(obs.rssi_dbm, Some(-54));
        assert_eq!(obs.noise_dbm, Some(-92));
        let ch = obs.channel.unwrap();
        assert_eq!(ch.number, 149);
        assert_eq!(ch.width, Some(ChannelWidth::Mhz80));
    }
}

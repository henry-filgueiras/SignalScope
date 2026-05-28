//! Semantic Wi-Fi types.
//!
//! These types describe *what* was observed about Wi-Fi, not *how* the
//! observation was acquired. Sensor adapters are responsible for translating
//! CoreWLAN, nl80211, or any other source into these types.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ssid(pub String);

impl Ssid {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ssid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical MAC-style BSSID, normalized to lowercase `aa:bb:cc:dd:ee:ff`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bssid(String);

impl Bssid {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(s.as_ref().to_ascii_lowercase())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Bssid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BandClass {
    TwoPointFourGhz,
    FiveGhz,
    SixGhz,
    Unknown,
}

impl BandClass {
    /// Best-effort band classification from a channel number. This is a
    /// convenience for adapters that report only a channel index.
    pub fn from_channel_number(n: u16) -> Self {
        match n {
            1..=14 => BandClass::TwoPointFourGhz,
            32..=177 => BandClass::FiveGhz,
            // Wi-Fi 6E uses 1..233 in the 6 GHz band; channels overlap with
            // 2.4 GHz numerically. Adapters that know the band should set it
            // explicitly rather than relying on this helper.
            _ => BandClass::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelWidth {
    Mhz20,
    Mhz40,
    Mhz80,
    Mhz160,
    /// 80+80 non-contiguous configuration, included for completeness.
    Mhz80Plus80,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Channel {
    pub number: u16,
    pub band: BandClass,
    pub width: Option<ChannelWidth>,
}

impl Channel {
    pub fn new(number: u16, band: BandClass, width: Option<ChannelWidth>) -> Self {
        Self { number, band, width }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Security {
    Open,
    Wep,
    Wpa,
    Wpa2,
    Wpa3,
    Unknown,
}

/// Snapshot of the *currently associated* Wi-Fi link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiObservation {
    pub interface: String,
    pub ssid: Option<Ssid>,
    pub bssid: Option<Bssid>,
    pub rssi_dbm: Option<i32>,
    pub noise_dbm: Option<i32>,
    pub tx_rate_mbps: Option<f32>,
    pub channel: Option<Channel>,
    pub security: Option<Security>,
}

impl WifiObservation {
    /// Approximate SNR in dB when both RSSI and noise are known.
    pub fn snr_db(&self) -> Option<i32> {
        match (self.rssi_dbm, self.noise_dbm) {
            (Some(r), Some(n)) => Some(r - n),
            _ => None,
        }
    }
}

/// A single neighbor AP observed during a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborAp {
    pub bssid: Bssid,
    pub ssid: Option<Ssid>,
    pub rssi_dbm: i32,
    pub channel: Option<Channel>,
    pub security: Option<Security>,
}

/// Result of a full neighbor scan at a moment in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub interface: String,
    pub neighbors: Vec<NeighborAp>,
}

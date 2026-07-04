//! RF waterfall projection — channels × time.
//!
//! Pure math for the `w`-toggled third view of the RF environment panel:
//! given the retained per-scan channel-occupancy history, produce a grid
//! of channel rows × scan columns ready for rendering. No ratatui imports
//! here (mirrors the `strip` module pattern) so the projection is easy to
//! unit-test.
//!
//! Design invariants:
//!
//! - Rows are in **fixed spectral order** (band, then channel number) —
//!   deliberately the opposite of the occupancy view's relevance ranking,
//!   because a time-axis display must never rerank rows between frames.
//! - Columns are **per-scan and event-anchored** — no wall-clock
//!   interpolation. A column exists because a scan happened.
//! - When channels outnumber rows, every channel that was *connected*
//!   anywhere in the visible window survives aggregation (the roam trace
//!   must never lose its tail), then highest total density; the rest fold
//!   into a single "other" row.

use std::collections::{BTreeMap, BTreeSet};

use signalscope_analysis::{pressure_tier, PressureTier};
use signalscope_core::TemporalSample;
use signalscope_events::{BandClass, Channel, ScanResult};
use time::OffsetDateTime;

/// Spectral identity of a channel row. The derived `Ord` on
/// `(band_rank, number)` IS the fixed spectral row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelKey {
    /// 0 = 2.4 GHz, 1 = 5 GHz, 2 = 6 GHz, 3 = unknown band.
    pub band_rank: u8,
    pub number: u16,
}

impl ChannelKey {
    pub fn band_label(self) -> &'static str {
        match self.band_rank {
            0 => "2.4",
            1 => "5",
            2 => "6",
            _ => "—",
        }
    }
}

impl From<Channel> for ChannelKey {
    fn from(ch: Channel) -> Self {
        // Sources sometimes report a channel number with an Unknown band;
        // normalize through the number-based classifier so ch6/Unknown and
        // ch6/2.4GHz don't split into two rows.
        let band = match ch.band {
            BandClass::Unknown => BandClass::from_channel_number(ch.number),
            known => known,
        };
        let band_rank = match band {
            BandClass::TwoPointFourGhz => 0,
            BandClass::FiveGhz => 1,
            BandClass::SixGhz => 2,
            BandClass::Unknown => 3,
        };
        Self {
            band_rank,
            number: ch.number,
        }
    }
}

/// Per-scan snapshot retained in `AppState::scan_history`. The connected
/// channel is captured at scan-ingest time so a replay rebuild produces
/// the identical grid without consulting any live state.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanSample {
    /// AP count per channel. Neighbors without channel data are skipped —
    /// they still count toward the panel title's total, just not here.
    pub channel_counts: BTreeMap<ChannelKey, usize>,
    pub connected: Option<ChannelKey>,
}

impl ScanSample {
    pub fn from_scan(scan: &ScanResult, connected: Option<ChannelKey>) -> Self {
        let mut channel_counts: BTreeMap<ChannelKey, usize> = BTreeMap::new();
        for ap in &scan.neighbors {
            let Some(ch) = ap.channel else { continue };
            *channel_counts.entry(ChannelKey::from(ch)).or_insert(0) += 1;
        }
        Self {
            channel_counts,
            connected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaterfallCell {
    pub count: usize,
    pub connected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Channel(ChannelKey),
    /// Aggregate of the channels that didn't fit the row budget.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaterfallRow {
    pub kind: RowKind,
    /// One cell per grid column, oldest → newest.
    pub cells: Vec<WaterfallCell>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaterfallGrid {
    /// Channel rows in fixed spectral order; `Other` (if present) last.
    pub rows: Vec<WaterfallRow>,
    /// Number of scan columns — `min(samples.len(), max_cols)`.
    pub columns: usize,
    /// Wall-clock timestamps of the oldest and newest column shown.
    pub span: Option<(OffsetDateTime, OffsetDateTime)>,
    /// How many channels were folded into the `Other` row.
    pub hidden_channels: usize,
}

impl WaterfallGrid {
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            columns: 0,
            span: None,
            hidden_channels: 0,
        }
    }
}

/// Project the retained scan history onto a rows × columns grid.
///
/// `samples` must be chronological (as yielded by `TemporalSeries::iter`).
/// `max_rows` is the channel-row budget (the caller excludes any axis
/// row); `max_cols` bounds how many of the newest scans are shown.
pub fn compute_waterfall(
    samples: &[&TemporalSample<ScanSample>],
    max_rows: usize,
    max_cols: usize,
) -> WaterfallGrid {
    if samples.is_empty() || max_rows == 0 || max_cols == 0 {
        return WaterfallGrid::empty();
    }

    let start = samples.len().saturating_sub(max_cols);
    let window = &samples[start..];
    let columns = window.len();
    let span = Some((window[0].at, window[columns - 1].at));

    // Universe: every channel seen in the window, plus every channel that
    // was connected in the window — a 0-neighbor connected channel still
    // gets a row (the trace must exist even when the scan sees nobody).
    let mut universe: BTreeSet<ChannelKey> = BTreeSet::new();
    for s in window {
        universe.extend(s.value.channel_counts.keys().copied());
        if let Some(c) = s.value.connected {
            universe.insert(c);
        }
    }

    let (keep, hidden) = if universe.len() <= max_rows {
        (universe, BTreeSet::new())
    } else {
        // Reserve one row for the aggregate.
        let budget = max_rows.saturating_sub(1);
        let mut keep: BTreeSet<ChannelKey> = BTreeSet::new();

        // (a) Channels connected anywhere in the window, newest first —
        // the roam trace is privileged over raw density.
        for s in window.iter().rev() {
            if keep.len() >= budget {
                break;
            }
            if let Some(c) = s.value.connected {
                keep.insert(c);
            }
        }

        // (b) Remaining budget by total count across the window,
        // descending; ties break toward spectral order.
        let mut totals: BTreeMap<ChannelKey, usize> = BTreeMap::new();
        for s in window {
            for (k, c) in &s.value.channel_counts {
                *totals.entry(*k).or_insert(0) += c;
            }
        }
        let mut by_density: Vec<(ChannelKey, usize)> = universe
            .iter()
            .filter(|k| !keep.contains(k))
            .map(|k| (*k, totals.get(k).copied().unwrap_or(0)))
            .collect();
        by_density.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (k, _) in by_density {
            if keep.len() >= budget {
                break;
            }
            keep.insert(k);
        }

        let hidden: BTreeSet<ChannelKey> = universe.difference(&keep).copied().collect();
        (keep, hidden)
    };

    // BTreeSet iteration order == ChannelKey order == spectral order.
    let mut rows: Vec<WaterfallRow> = keep
        .iter()
        .map(|key| WaterfallRow {
            kind: RowKind::Channel(*key),
            cells: window
                .iter()
                .map(|s| WaterfallCell {
                    count: s.value.channel_counts.get(key).copied().unwrap_or(0),
                    connected: s.value.connected == Some(*key),
                })
                .collect(),
        })
        .collect();

    let hidden_channels = hidden.len();
    if !hidden.is_empty() {
        rows.push(WaterfallRow {
            kind: RowKind::Other,
            cells: window
                .iter()
                .map(|s| WaterfallCell {
                    count: s
                        .value
                        .channel_counts
                        .iter()
                        .filter(|(k, _)| hidden.contains(*k))
                        .map(|(_, c)| *c)
                        .sum(),
                    // Pathological fallback: if a connected channel was
                    // folded (more connected channels in the window than
                    // rows), the aggregate row carries the trace rather
                    // than letting it silently vanish.
                    connected: s.value.connected.is_some_and(|c| hidden.contains(&c)),
                })
                .collect(),
        });
    }

    WaterfallGrid {
        rows,
        columns,
        span,
        hidden_channels,
    }
}

/// Density glyph for one cell. 0 is special-cased ahead of the tier
/// ladder: `pressure_tier(0)` is `Low`, but "a scan ran and measured
/// quiet" gets a lattice dot, not a shade block — distinct from the blank
/// left-pad that means "no scan exists here".
pub fn glyph_for_count(count: usize) -> &'static str {
    if count == 0 {
        return "·";
    }
    match pressure_tier(count) {
        PressureTier::Low => "░",
        PressureTier::Moderate => "▒",
        PressureTier::Elevated => "▓",
        PressureTier::Severe => "█",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(band_rank: u8, number: u16) -> ChannelKey {
        ChannelKey { band_rank, number }
    }

    fn sample(
        offset_secs: i64,
        counts: &[(ChannelKey, usize)],
        connected: Option<ChannelKey>,
    ) -> TemporalSample<ScanSample> {
        TemporalSample {
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(offset_secs),
            value: ScanSample {
                channel_counts: counts.iter().copied().collect(),
                connected,
            },
        }
    }

    fn compute(
        samples: &[TemporalSample<ScanSample>],
        max_rows: usize,
        max_cols: usize,
    ) -> WaterfallGrid {
        let refs: Vec<&TemporalSample<ScanSample>> = samples.iter().collect();
        compute_waterfall(&refs, max_rows, max_cols)
    }

    fn row_keys(grid: &WaterfallGrid) -> Vec<RowKind> {
        grid.rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn empty_samples_returns_empty_grid() {
        let grid = compute(&[], 10, 30);
        assert_eq!(grid.rows.len(), 0);
        assert_eq!(grid.columns, 0);
        assert_eq!(grid.span, None);
        assert_eq!(grid.hidden_channels, 0);
    }

    #[test]
    fn zero_rows_or_cols_returns_empty_grid() {
        let samples = vec![sample(0, &[(key(0, 6), 3)], None)];
        assert_eq!(compute(&samples, 0, 30), WaterfallGrid::empty());
        assert_eq!(compute(&samples, 10, 0), WaterfallGrid::empty());
    }

    #[test]
    fn single_sample_produces_one_column_grid() {
        let samples = vec![sample(5, &[(key(0, 6), 3), (key(1, 44), 1)], None)];
        let grid = compute(&samples, 10, 30);
        assert_eq!(grid.columns, 1);
        assert_eq!(grid.rows.len(), 2);
        assert!(grid.rows.iter().all(|r| r.cells.len() == 1));
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(5);
        assert_eq!(grid.span, Some((at, at)));
    }

    #[test]
    fn rows_sorted_spectrally_band_then_number() {
        // Deliberately fed out of order: 6 GHz ch5, 2.4 ch11, 5 GHz ch36, 2.4 ch1.
        let samples = vec![sample(
            0,
            &[
                (key(2, 5), 1),
                (key(0, 11), 1),
                (key(1, 36), 1),
                (key(0, 1), 1),
            ],
            None,
        )];
        let grid = compute(&samples, 10, 30);
        assert_eq!(
            row_keys(&grid),
            vec![
                RowKind::Channel(key(0, 1)),
                RowKind::Channel(key(0, 11)),
                RowKind::Channel(key(1, 36)),
                RowKind::Channel(key(2, 5)),
            ]
        );
    }

    #[test]
    fn row_order_is_stable_when_densities_shift_between_columns() {
        // ch6 dominates the first scan, ch149 the second — order must not move.
        let samples = vec![
            sample(0, &[(key(0, 6), 9), (key(1, 149), 1)], None),
            sample(10, &[(key(0, 6), 1), (key(1, 149), 9)], None),
        ];
        let grid = compute(&samples, 10, 30);
        assert_eq!(
            row_keys(&grid),
            vec![
                RowKind::Channel(key(0, 6)),
                RowKind::Channel(key(1, 149)),
            ]
        );
    }

    #[test]
    fn window_takes_newest_samples_when_more_than_max_cols() {
        let samples: Vec<_> = (0..10)
            .map(|i| sample(i * 10, &[(key(0, 6), i as usize)], None))
            .collect();
        let grid = compute(&samples, 10, 4);
        assert_eq!(grid.columns, 4);
        let counts: Vec<usize> = grid.rows[0].cells.iter().map(|c| c.count).collect();
        assert_eq!(counts, vec![6, 7, 8, 9]); // the four newest scans
        let (oldest, newest) = grid.span.unwrap();
        assert_eq!(oldest, OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60));
        assert_eq!(newest, OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(90));
    }

    #[test]
    fn channel_absent_from_one_scan_yields_zero_count_cell() {
        let samples = vec![
            sample(0, &[(key(0, 6), 3), (key(1, 44), 2)], None),
            sample(10, &[(key(1, 44), 2)], None), // ch6 vanished this scan
        ];
        let grid = compute(&samples, 10, 30);
        let ch6 = &grid.rows[0];
        assert_eq!(ch6.kind, RowKind::Channel(key(0, 6)));
        assert_eq!(ch6.cells[0].count, 3);
        assert_eq!(ch6.cells[1].count, 0);
    }

    #[test]
    fn connected_flag_set_on_matching_cell_per_column() {
        let conn = key(1, 44);
        let samples = vec![
            sample(0, &[(conn, 2), (key(0, 6), 3)], Some(conn)),
            sample(10, &[(conn, 2), (key(0, 6), 3)], None), // dropped association
        ];
        let grid = compute(&samples, 10, 30);
        let row44 = grid
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Channel(conn))
            .unwrap();
        assert!(row44.cells[0].connected);
        assert!(!row44.cells[1].connected);
        let row6 = grid
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Channel(key(0, 6)))
            .unwrap();
        assert!(row6.cells.iter().all(|c| !c.connected));
    }

    #[test]
    fn connected_channel_with_zero_neighbors_still_gets_a_row() {
        let conn = key(1, 44);
        // The scan never observed any AP on ch44 — but we're connected to it.
        let samples = vec![sample(0, &[(key(0, 6), 3)], Some(conn))];
        let grid = compute(&samples, 10, 30);
        let row = grid
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Channel(conn))
            .expect("connected channel must have a row");
        assert_eq!(row.cells[0].count, 0);
        assert!(row.cells[0].connected);
    }

    #[test]
    fn roam_moves_connected_flag_between_rows_across_columns() {
        let a = key(1, 44);
        let b = key(1, 149);
        let samples = vec![
            sample(0, &[(a, 2), (b, 2)], Some(a)),
            sample(10, &[(a, 2), (b, 2)], Some(a)),
            sample(20, &[(a, 2), (b, 2)], Some(b)), // roam
        ];
        let grid = compute(&samples, 10, 30);
        let row_a = grid.rows.iter().find(|r| r.kind == RowKind::Channel(a)).unwrap();
        let row_b = grid.rows.iter().find(|r| r.kind == RowKind::Channel(b)).unwrap();
        assert_eq!(
            row_a.cells.iter().map(|c| c.connected).collect::<Vec<_>>(),
            vec![true, true, false]
        );
        assert_eq!(
            row_b.cells.iter().map(|c| c.connected).collect::<Vec<_>>(),
            vec![false, false, true]
        );
    }

    #[test]
    fn aggregation_keeps_connected_plus_top_density_and_sums_other() {
        let conn = key(1, 44);
        // Five channels, three rows: connected + the densest fit; the two
        // quietest fold into Other.
        let samples = vec![sample(
            0,
            &[
                (key(0, 1), 1),
                (key(0, 6), 9),
                (key(0, 11), 2),
                (conn, 1),
                (key(1, 149), 7),
            ],
            Some(conn),
        )];
        let grid = compute(&samples, 3, 30);
        // Budget = 2 (Other reserves one): connected ch44 + densest ch6.
        assert_eq!(
            row_keys(&grid),
            vec![
                RowKind::Channel(key(0, 6)),
                RowKind::Channel(conn),
                RowKind::Other,
            ]
        );
        assert_eq!(grid.hidden_channels, 3);
        let other = grid.rows.last().unwrap();
        assert_eq!(other.cells[0].count, 1 + 2 + 7); // ch1 + ch11 + ch149
        assert!(!other.cells[0].connected);
    }

    #[test]
    fn all_window_connected_channels_survive_aggregation_over_denser_rows() {
        let pre = key(1, 44); // connected before the roam, quiet
        let post = key(1, 149); // connected after the roam, quiet
        // Three loud channels that would out-rank both on density alone.
        let loud = [(key(0, 1), 9), (key(0, 6), 9), (key(0, 11), 9)];
        let mut counts_pre = loud.to_vec();
        counts_pre.push((pre, 1));
        let mut counts_post = loud.to_vec();
        counts_post.push((post, 1));
        let samples = vec![
            sample(0, &counts_pre, Some(pre)),
            sample(10, &counts_post, Some(post)),
        ];
        // Row budget 3 → keep budget 2: both connected channels must win
        // over all three louder rows.
        let grid = compute(&samples, 3, 30);
        assert_eq!(
            row_keys(&grid),
            vec![
                RowKind::Channel(pre),
                RowKind::Channel(post),
                RowKind::Other,
            ]
        );
        // The trace is intact: pre connected in col 0, post in col 1.
        assert!(grid.rows[0].cells[0].connected);
        assert!(grid.rows[1].cells[1].connected);
    }

    #[test]
    fn channel_key_from_unknown_band_normalizes_via_channel_number() {
        let ch6_unknown = Channel::new(6, BandClass::Unknown, None);
        let ch6_known = Channel::new(6, BandClass::TwoPointFourGhz, None);
        assert_eq!(ChannelKey::from(ch6_unknown), ChannelKey::from(ch6_known));

        // A number outside every classifiable range stays Unknown and
        // sorts after all real bands.
        let mystery = ChannelKey::from(Channel::new(200, BandClass::Unknown, None));
        assert_eq!(mystery.band_rank, 3);
        assert_eq!(mystery.band_label(), "—");
        assert!(ChannelKey::from(ch6_known) < mystery);
        assert!(key(2, 233) < mystery);
    }

    #[test]
    fn glyph_ladder_boundaries() {
        assert_eq!(glyph_for_count(0), "·");
        assert_eq!(glyph_for_count(1), "░");
        assert_eq!(glyph_for_count(2), "░");
        assert_eq!(glyph_for_count(3), "▒");
        assert_eq!(glyph_for_count(5), "▒");
        assert_eq!(glyph_for_count(6), "▓");
        assert_eq!(glyph_for_count(8), "▓");
        assert_eq!(glyph_for_count(9), "█");
        assert_eq!(glyph_for_count(99), "█");
    }
}

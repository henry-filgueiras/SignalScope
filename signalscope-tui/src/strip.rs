//! Timeline strip — a one-row minimap of a recording.
//!
//! In replay mode a thin strip sits below the header and projects the
//! entire recording onto the available terminal width. Every column
//! represents a slice of wall-clock time; each column shows either:
//!
//! * a colored glyph representing the highest-severity landmark in
//!   that slice, with glyph weight (`·` → `•` → `●`) growing with
//!   landmark density;
//! * a bold accent marker (`┃`) where the playhead currently sits;
//! * a dim baseline (`─`) otherwise.
//!
//! The point is recognition before reading. An operator scanning the
//! strip should immediately see whether the recording is uniformly
//! quiet, clustered around one incident, or steadily eventful — and
//! where in that shape they're standing.
//!
//! This module owns just the *math*. Rendering is in `ui.rs`. Keeping
//! the projection function pure lets the strip be unit-tested without
//! reaching into ratatui or the playback type.

use crate::landmarks::{LandmarkSeverity, TimelineLandmark};

/// What to render at one column of the strip. Density carries the
/// count separately from severity because the renderer wants to vary
/// glyph weight by density while picking color by severity (worst
/// severity wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripCell {
    /// No landmark in this column. Renders as the baseline `─`.
    Empty,
    /// One or more landmarks fall in this column.
    Landmarks {
        count: u32,
        worst_severity: LandmarkSeverity,
    },
}

impl StripCell {
    pub fn empty() -> Self {
        StripCell::Empty
    }
}

/// Project a recording's landmarks onto a fixed-width strip.
///
/// `total_secs` is the wall-clock span of the recording (last_at -
/// first_at). `cols` is the width of the strip in terminal columns.
/// Each landmark contributes to exactly one column, the one whose
/// time slice contains the landmark's offset from `first_at`.
///
/// Edge cases:
///
/// * `cols == 0` → empty vec.
/// * `total_secs == 0.0` or the recording has fewer than two events
///   spaced in time → returns `cols` empty cells (the strip is
///   informationless for instantaneous recordings, but the renderer
///   still draws a baseline so the operator sees something).
pub fn compute_strip_columns(
    landmarks: &[TimelineLandmark],
    total_secs: f64,
    landmark_offsets_secs: impl IntoIterator<Item = f64>,
    cols: usize,
) -> Vec<StripCell> {
    if cols == 0 {
        return Vec::new();
    }
    let mut out = vec![StripCell::empty(); cols];
    if total_secs <= 0.0 || landmarks.is_empty() {
        return out;
    }

    let secs_per_col = total_secs / cols as f64;
    for (l, offset) in landmarks.iter().zip(landmark_offsets_secs) {
        if offset.is_nan() || offset < 0.0 {
            continue;
        }
        // Floor into the column the offset falls in. Clamp to cols-1
        // so a landmark exactly at the end-of-recording lands in the
        // last column rather than past it.
        let col = ((offset / secs_per_col) as usize).min(cols - 1);
        out[col] = match out[col] {
            StripCell::Empty => StripCell::Landmarks {
                count: 1,
                worst_severity: l.severity,
            },
            StripCell::Landmarks {
                count,
                worst_severity,
            } => StripCell::Landmarks {
                count: count + 1,
                worst_severity: pick_worst(worst_severity, l.severity),
            },
        };
    }
    out
}

/// Column index for a wall-clock offset (typically the playhead's
/// offset from the recording's start). Same flooring + clamping rule
/// as the landmark projection so the playhead lands in the same
/// column as a landmark that shares its event.
pub fn column_for_offset(offset_secs: f64, total_secs: f64, cols: usize) -> Option<usize> {
    if cols == 0 || total_secs <= 0.0 || offset_secs.is_nan() || offset_secs < 0.0 {
        return None;
    }
    let secs_per_col = total_secs / cols as f64;
    Some(((offset_secs / secs_per_col) as usize).min(cols - 1))
}

/// Severity ordering for the "worst wins" rule. `Alarm` > `Recovery`
/// > `Notable`. Recovery beats Notable because a recovery is an
/// operationally meaningful transition (the system fixed itself);
/// Notable is "interesting context" that should yield to either.
fn pick_worst(a: LandmarkSeverity, b: LandmarkSeverity) -> LandmarkSeverity {
    use LandmarkSeverity::*;
    match (a, b) {
        (Alarm, _) | (_, Alarm) => Alarm,
        (Recovery, _) | (_, Recovery) => Recovery,
        _ => Notable,
    }
}

/// Glyph weight for a column based on landmark density. The visual
/// step matters more than the precise threshold — the eye picks up
/// "more dots = busier."
pub fn glyph_for_density(count: u32) -> &'static str {
    match count {
        0 => "─",
        1 => "·",
        2..=3 => "•",
        _ => "●",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::landmarks::{LandmarkCategory, LandmarkSeverity, TimelineLandmark};
    use time::OffsetDateTime;

    fn landmark(offset_secs: i64, severity: LandmarkSeverity) -> TimelineLandmark {
        TimelineLandmark {
            at: OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset_secs).unwrap(),
            event_index: offset_secs as usize,
            category: LandmarkCategory::Finding,
            severity,
            headline: format!("event at +{offset_secs}s"),
        }
    }

    fn project(landmarks: &[TimelineLandmark], total_secs: f64, cols: usize) -> Vec<StripCell> {
        let start = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let offsets = landmarks
            .iter()
            .map(|l| (l.at - start).as_seconds_f64())
            .collect::<Vec<_>>();
        compute_strip_columns(landmarks, total_secs, offsets, cols)
    }

    #[test]
    fn zero_columns_returns_empty_vec() {
        assert!(project(&[], 60.0, 0).is_empty());
    }

    #[test]
    fn empty_landmarks_returns_all_baseline_cells() {
        let strip = project(&[], 60.0, 20);
        assert_eq!(strip.len(), 20);
        assert!(strip.iter().all(|c| matches!(c, StripCell::Empty)));
    }

    #[test]
    fn zero_total_span_returns_all_empty_even_with_landmarks() {
        let landmarks = vec![landmark(0, LandmarkSeverity::Alarm)];
        let strip = project(&landmarks, 0.0, 10);
        assert!(strip.iter().all(|c| matches!(c, StripCell::Empty)));
    }

    #[test]
    fn single_landmark_lands_in_expected_column() {
        // 60-sec recording, 10 cols → 6 sec per col. A landmark at
        // offset 12s lands in column 2 (cells 0..1 cover 0..12 secs).
        let landmarks = vec![landmark(12, LandmarkSeverity::Alarm)];
        let strip = project(&landmarks, 60.0, 10);
        for (i, cell) in strip.iter().enumerate() {
            if i == 2 {
                assert!(matches!(
                    cell,
                    StripCell::Landmarks {
                        count: 1,
                        worst_severity: LandmarkSeverity::Alarm
                    }
                ));
            } else {
                assert!(matches!(cell, StripCell::Empty), "col {i} should be empty");
            }
        }
    }

    #[test]
    fn end_of_recording_lands_in_last_column_not_past_it() {
        // Landmark at the very last second of a 60s recording. With
        // 10 cols, offset/secs_per_col = 60/6 = exactly 10, which
        // would index out-of-bounds without the clamp.
        let landmarks = vec![landmark(60, LandmarkSeverity::Recovery)];
        let strip = project(&landmarks, 60.0, 10);
        assert!(matches!(
            strip[9],
            StripCell::Landmarks { count: 1, worst_severity: LandmarkSeverity::Recovery }
        ));
    }

    #[test]
    fn multiple_landmarks_in_one_column_accumulate_and_pick_worst_severity() {
        // Three landmarks, all in column 0 (offsets 0, 1, 2 sec in a
        // 60-sec / 10-col strip).
        let landmarks = vec![
            landmark(0, LandmarkSeverity::Notable),
            landmark(1, LandmarkSeverity::Recovery),
            landmark(2, LandmarkSeverity::Alarm),
        ];
        let strip = project(&landmarks, 60.0, 10);
        assert!(matches!(
            strip[0],
            StripCell::Landmarks {
                count: 3,
                worst_severity: LandmarkSeverity::Alarm
            }
        ));
    }

    #[test]
    fn recovery_beats_notable_when_no_alarm_present() {
        let landmarks = vec![
            landmark(0, LandmarkSeverity::Notable),
            landmark(1, LandmarkSeverity::Recovery),
        ];
        let strip = project(&landmarks, 60.0, 10);
        assert!(matches!(
            strip[0],
            StripCell::Landmarks {
                worst_severity: LandmarkSeverity::Recovery,
                ..
            }
        ));
    }

    #[test]
    fn column_for_offset_matches_landmark_projection() {
        // Sanity: a landmark's projection column should equal
        // column_for_offset on the same offset.
        let landmarks = vec![landmark(34, LandmarkSeverity::Alarm)];
        let strip = project(&landmarks, 60.0, 20);
        let col = column_for_offset(34.0, 60.0, 20).unwrap();
        assert!(matches!(strip[col], StripCell::Landmarks { .. }));
    }

    #[test]
    fn column_for_offset_clamps_to_last_column_at_end() {
        assert_eq!(column_for_offset(60.0, 60.0, 10), Some(9));
    }

    #[test]
    fn column_for_offset_rejects_bad_inputs() {
        assert_eq!(column_for_offset(10.0, 60.0, 0), None);
        assert_eq!(column_for_offset(10.0, 0.0, 10), None);
        assert_eq!(column_for_offset(f64::NAN, 60.0, 10), None);
        assert_eq!(column_for_offset(-5.0, 60.0, 10), None);
    }

    #[test]
    fn glyph_thickness_grows_with_density() {
        assert_eq!(glyph_for_density(0), "─");
        assert_eq!(glyph_for_density(1), "·");
        assert_eq!(glyph_for_density(2), "•");
        assert_eq!(glyph_for_density(3), "•");
        assert_eq!(glyph_for_density(4), "●");
        assert_eq!(glyph_for_density(99), "●");
    }

    #[test]
    fn dense_recording_compresses_cleanly_to_narrow_strip() {
        // 100 landmarks across a 100-second recording, projected
        // onto 5 columns. Each column covers 20 seconds → should
        // contain ~20 landmarks.
        let landmarks: Vec<_> = (0..100)
            .map(|i| landmark(i, LandmarkSeverity::Notable))
            .collect();
        let strip = project(&landmarks, 100.0, 5);
        let total: u32 = strip
            .iter()
            .map(|c| match c {
                StripCell::Landmarks { count, .. } => *count,
                StripCell::Empty => 0,
            })
            .sum();
        assert_eq!(total, 100, "every landmark must be accounted for");
        // Each column should have roughly 20 (give or take 1 at the
        // boundaries due to flooring).
        for cell in &strip {
            if let StripCell::Landmarks { count, .. } = cell {
                assert!(
                    *count >= 19 && *count <= 21,
                    "uneven distribution: {count}"
                );
            }
        }
    }
}

//! Finding lifecycle tracking.
//!
//! The rules layer produces `CandidateFinding`s every time it runs. If we
//! emitted each candidate onto the bus, the event feed would scroll with
//! the same "RF congestion" line forever. That's noise, not observability.
//!
//! This module owns a small per-fingerprint state table and emits
//! `CorrelationFinding`s only on transitions: a new active condition, a
//! material change in confidence, or a resolution. Quiescent
//! re-evaluations are dropped on the floor.
//!
//! The thresholds are deliberately coarse and configurable. The default
//! cadence is "respond promptly, but never twice for the same wobble."

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use signalscope_events::{Confidence, CorrelationFinding, FindingKind, FindingLifecycle};
use time::OffsetDateTime;

use crate::rules::CandidateFinding;

/// Tuning knobs for the lifecycle tracker. The defaults aim at a calm,
/// trustworthy feel: a finding shouldn't flicker faster than a human can
/// read it.
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    /// Minimum change in confidence required to count as material. A
    /// finding whose confidence oscillates within this band is suppressed.
    pub material_delta: f32,
    /// Minimum gap between consecutive emissions of the same fingerprint.
    /// Acts as a back-stop against rapid ping-pong between Escalating /
    /// Recovering even when the delta is large.
    pub min_cooldown: Duration,
    /// Time without a rule re-firing before the finding is declared
    /// Resolved. Tolerates short flickers (e.g. one healthy DNS sample
    /// during an otherwise pathological window) without prematurely
    /// retiring the finding.
    pub resolved_after: Duration,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            material_delta: 0.15,
            min_cooldown: Duration::from_secs(15),
            resolved_after: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveFinding {
    kind: FindingKind,
    fingerprint: String,
    first_seen: OffsetDateTime,
    last_seen: OffsetDateTime,
    last_emit: OffsetDateTime,
    last_emitted_confidence: f32,
    peak_confidence: f32,
    headline: String,
    evidence: Vec<String>,
}

#[derive(Debug)]
pub struct LifecycleTracker {
    active: HashMap<String, ActiveFinding>,
    config: LifecycleConfig,
}

impl LifecycleTracker {
    pub fn new(config: LifecycleConfig) -> Self {
        Self {
            active: HashMap::new(),
            config,
        }
    }

    /// Feed the current cycle's candidate set in. Returns the
    /// `CorrelationFinding`s the engine should publish — possibly empty.
    pub fn step(
        &mut self,
        candidates: Vec<CandidateFinding>,
        now: OffsetDateTime,
    ) -> Vec<CorrelationFinding> {
        let mut emitted = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for cand in candidates {
            seen.insert(cand.fingerprint.clone());
            match self.active.get_mut(&cand.fingerprint) {
                None => {
                    let active = ActiveFinding {
                        kind: cand.kind,
                        fingerprint: cand.fingerprint.clone(),
                        first_seen: now,
                        last_seen: now,
                        last_emit: now,
                        last_emitted_confidence: cand.confidence,
                        peak_confidence: cand.confidence,
                        headline: cand.headline.clone(),
                        evidence: cand.evidence.clone(),
                    };
                    emitted.push(synthesize(
                        &active,
                        FindingLifecycle::Active,
                        cand.confidence,
                        &cand.headline,
                        cand.evidence.clone(),
                    ));
                    self.active.insert(cand.fingerprint.clone(), active);
                }
                Some(state) => {
                    state.last_seen = now;
                    state.peak_confidence = state.peak_confidence.max(cand.confidence);
                    state.headline = cand.headline.clone();
                    state.evidence = cand.evidence.clone();

                    let delta = cand.confidence - state.last_emitted_confidence;
                    let cooldown_elapsed =
                        now - state.last_emit >= time::Duration::try_from(self.config.min_cooldown)
                            .unwrap_or(time::Duration::ZERO);

                    if delta.abs() >= self.config.material_delta && cooldown_elapsed {
                        let lifecycle = if delta > 0.0 {
                            FindingLifecycle::Escalating
                        } else {
                            FindingLifecycle::Recovering
                        };
                        emitted.push(synthesize(
                            state,
                            lifecycle,
                            cand.confidence,
                            &cand.headline,
                            cand.evidence.clone(),
                        ));
                        state.last_emit = now;
                        state.last_emitted_confidence = cand.confidence;
                    }
                }
            }
        }

        // Resolve anything not seen long enough.
        let resolved_after =
            time::Duration::try_from(self.config.resolved_after).unwrap_or(time::Duration::ZERO);
        let to_resolve: Vec<String> = self
            .active
            .iter()
            .filter(|(k, state)| !seen.contains(*k) && now - state.last_seen >= resolved_after)
            .map(|(k, _)| k.clone())
            .collect();

        for key in to_resolve {
            if let Some(state) = self.active.remove(&key) {
                emitted.push(synthesize_resolved(&state, now));
            }
        }

        emitted
    }

    /// Snapshot of currently-active fingerprints. Useful for tests; the
    /// engine doesn't read this directly because the bus is the source of
    /// truth for downstream consumers.
    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

fn synthesize(
    state: &ActiveFinding,
    lifecycle: FindingLifecycle,
    current_confidence: f32,
    base_headline: &str,
    evidence: Vec<String>,
) -> CorrelationFinding {
    let headline = decorate(base_headline, lifecycle);
    CorrelationFinding {
        kind: state.kind,
        fingerprint: state.fingerprint.clone(),
        headline,
        confidence: Confidence::new(current_confidence),
        peak_confidence: Confidence::new(state.peak_confidence),
        evidence,
        lifecycle,
        first_seen: state.first_seen,
        last_seen: state.last_seen,
    }
}

fn synthesize_resolved(state: &ActiveFinding, now: OffsetDateTime) -> CorrelationFinding {
    // `last_seen` is preserved as the last time the rule actually fired
    // positive — useful for "active for 4m" callouts in the UI.
    CorrelationFinding {
        kind: state.kind,
        fingerprint: state.fingerprint.clone(),
        headline: decorate(&state.headline, FindingLifecycle::Resolved),
        confidence: Confidence::new(0.0),
        peak_confidence: Confidence::new(state.peak_confidence),
        evidence: state.evidence.clone(),
        lifecycle: FindingLifecycle::Resolved,
        first_seen: state.first_seen,
        last_seen: now,
    }
}

fn decorate(base: &str, lifecycle: FindingLifecycle) -> String {
    match lifecycle {
        FindingLifecycle::Active => base.to_string(),
        FindingLifecycle::Escalating => format!("{base} — worsening"),
        FindingLifecycle::Recovering => format!("{base} — easing"),
        FindingLifecycle::Resolved => format!("{base} — resolved"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(fingerprint: &str, confidence: f32, headline: &str) -> CandidateFinding {
        CandidateFinding {
            kind: FindingKind::RfCongestion,
            fingerprint: fingerprint.to_string(),
            headline: headline.to_string(),
            confidence,
            evidence: vec![],
        }
    }

    fn fast_tracker() -> LifecycleTracker {
        // Cooldown / resolved-after collapsed to zero so step-by-step
        // tests don't have to fake the system clock.
        LifecycleTracker::new(LifecycleConfig {
            material_delta: 0.15,
            min_cooldown: Duration::ZERO,
            resolved_after: Duration::ZERO,
        })
    }

    fn t0() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn first_emit_is_active() {
        let mut t = fast_tracker();
        let out = t.step(vec![cand("rf:ch11", 0.5, "RF congestion on channel 11")], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Active);
        assert_eq!(out[0].fingerprint, "rf:ch11");
        assert_eq!(out[0].confidence.value(), 0.5);
        assert_eq!(out[0].peak_confidence.value(), 0.5);
    }

    #[test]
    fn repeated_emit_with_same_confidence_is_suppressed() {
        let mut t = fast_tracker();
        let _ = t.step(vec![cand("rf:ch11", 0.5, "x")], t0());
        let out = t.step(vec![cand("rf:ch11", 0.5, "x")], t0());
        assert!(out.is_empty(), "no transition → no emission");
    }

    #[test]
    fn small_confidence_jitter_is_suppressed() {
        let mut t = fast_tracker();
        let _ = t.step(vec![cand("rf:ch11", 0.5, "x")], t0());
        let out = t.step(vec![cand("rf:ch11", 0.55, "x")], t0());
        assert!(out.is_empty(), "delta 0.05 is below the 0.15 material threshold");
    }

    #[test]
    fn material_rise_emits_escalating() {
        let mut t = fast_tracker();
        let _ = t.step(vec![cand("rf:ch11", 0.5, "x")], t0());
        let out = t.step(vec![cand("rf:ch11", 0.7, "x")], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Escalating);
        assert_eq!(out[0].peak_confidence.value(), 0.7);
    }

    #[test]
    fn material_drop_emits_recovering() {
        let mut t = fast_tracker();
        let _ = t.step(vec![cand("rf:ch11", 0.7, "x")], t0());
        let out = t.step(vec![cand("rf:ch11", 0.5, "x")], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Recovering);
        // peak is sticky
        assert_eq!(out[0].peak_confidence.value(), 0.7);
    }

    #[test]
    fn absent_candidate_emits_resolved() {
        let mut t = fast_tracker();
        let _ = t.step(vec![cand("rf:ch11", 0.7, "x")], t0());
        let out = t.step(vec![], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Resolved);
        assert_eq!(t.active_count(), 0, "resolved findings are dropped");
    }

    #[test]
    fn cooldown_suppresses_rapid_oscillation() {
        let mut t = LifecycleTracker::new(LifecycleConfig {
            material_delta: 0.1,
            min_cooldown: Duration::from_secs(30),
            resolved_after: Duration::from_secs(60),
        });
        let t0 = t0();
        let _ = t.step(vec![cand("rf:ch11", 0.5, "x")], t0);
        // 5s later, conf shoots up: would be a material rise, but cooldown
        // hasn't elapsed.
        let later = t0 + time::Duration::seconds(5);
        let out = t.step(vec![cand("rf:ch11", 0.8, "x")], later);
        assert!(out.is_empty(), "cooldown should suppress within 30s");
        // 35s after t0, same rise emits.
        let later = t0 + time::Duration::seconds(35);
        let out = t.step(vec![cand("rf:ch11", 0.8, "x")], later);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Escalating);
    }

    #[test]
    fn brief_flicker_does_not_resolve() {
        let mut t = LifecycleTracker::new(LifecycleConfig {
            material_delta: 0.15,
            min_cooldown: Duration::ZERO,
            resolved_after: Duration::from_secs(20),
        });
        let t0 = t0();
        let _ = t.step(vec![cand("rf:ch11", 0.5, "x")], t0);
        // 10s later: rule misses (transient), but we shouldn't resolve.
        let mid = t0 + time::Duration::seconds(10);
        let out = t.step(vec![], mid);
        assert!(out.is_empty(), "10s gap below 20s resolved_after → no Resolved");
        // Rule fires again — no new transition, so still no emission.
        let out = t.step(vec![cand("rf:ch11", 0.5, "x")], mid + time::Duration::seconds(1));
        assert!(out.is_empty());
    }

    #[test]
    fn different_fingerprints_are_independent() {
        let mut t = fast_tracker();
        let out = t.step(
            vec![
                cand("rf:ch11", 0.5, "ch11"),
                cand("rf:ch36", 0.5, "ch36"),
            ],
            t0(),
        );
        assert_eq!(out.len(), 2);
        // Only one resolves; the other persists.
        let out = t.step(vec![cand("rf:ch36", 0.5, "ch36")], t0());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].lifecycle, FindingLifecycle::Resolved);
        assert_eq!(out[0].fingerprint, "rf:ch11");
    }

    #[test]
    fn resolved_headline_carries_resolved_suffix() {
        let mut t = fast_tracker();
        let _ = t.step(
            vec![cand("rf:ch11", 0.7, "RF congestion on channel 11")],
            t0(),
        );
        let out = t.step(vec![], t0());
        assert!(out[0].headline.contains("resolved"));
    }
}

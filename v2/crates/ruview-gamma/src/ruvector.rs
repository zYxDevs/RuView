//! RuVector self-learning layer (ADR-250 §10, items 3–6).
//!
//! The adaptive memory *across* people: anonymized 20-field response vectors
//! ([`crate::response::PersonResponseVector::as_array`]) stored in a
//! [`ProfileStore`], queried by deterministic k-nearest-neighbor to
//! **warm-start a new person's optimizer** from similar responders (instead of
//! the flat 40 Hz prior), plus per-person **drift detection** (item 4) and
//! cohort **response clustering** (item 5).
//!
//! Privacy posture: profiles carry only a one-way hashed tag (never a
//! `person_id`) and the 20 normalized response fields — no identity, no raw
//! sensor data. Cohort knowledge enters the optimizer exclusively as
//! down-weighted pseudo-observations ([`crate::optimizer::BayesianOptimizer::
//! observe_prior`]) that can shape *where to look first* but never define what
//! this person's measured response is.
//!
//! Everything here is deterministic: distances are fixed-range normalized,
//! ties break by insertion index, clustering uses farthest-point seeding from
//! index 0 — same inputs, same outputs, on every machine.

use serde::{Deserialize, Serialize};

use crate::math::clamp_safe;
use crate::simulator::stable_hash;

/// Dimensionality of the response vector (ADR-250 §6).
pub const VECTOR_DIM: usize = 20;

/// Fixed per-field normalization ranges `(lo, hi)` for distance computation,
/// in the ADR-250 §6 field order. Constants (not data-derived statistics) so
/// distances are stable as the store grows.
pub const NORM_RANGES: [(f64, f64); VECTOR_DIM] = [
    (0.0, 1.0),   // baseline_gamma
    (0.0, 1.0),   // baseline_alpha
    (0.0, 5.0),   // alpha_gamma_ratio
    (0.0, 1.0),   // gamma_power_gain
    (0.0, 1.0),   // phase_locking_value
    (6.0, 30.0),  // breathing_rate (bpm)
    (0.0, 1.0),   // breathing_stability
    (0.0, 1.0),   // motion_artifact
    (0.0, 1.0),   // posture_state
    (0.0, 1.0),   // sleep_state
    (0.0, 1.0),   // restlessness_score
    (36.0, 44.0), // stimulus_frequency (Hz)
    (0.0, 1.0),   // brightness_level
    (0.0, 1.0),   // sound_level
    (0.0, 1.0),   // duty_cycle
    (-5.0, 5.0),  // phase_offset (ms)
    (0.0, 15.0),  // session_duration (min)
    (0.0, 1.0),   // comfort_score
    (0.0, 1.0),   // adherence_score
    (0.0, 1.0),   // adverse_event_flag
];

/// Normalize a raw response vector to the unit hypercube using
/// [`NORM_RANGES`]. Non-finite fields clamp to the range floor.
pub fn normalize(v: &[f64; VECTOR_DIM]) -> [f64; VECTOR_DIM] {
    let mut out = [0.0; VECTOR_DIM];
    for (i, (&val, &(lo, hi))) in v.iter().zip(NORM_RANGES.iter()).enumerate() {
        out[i] = clamp_safe((val - lo) / (hi - lo), 0.0, 1.0);
    }
    out
}

/// Euclidean distance between two normalized vectors.
fn unit_distance(a: &[f64; VECTOR_DIM], b: &[f64; VECTOR_DIM]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

/// One anonymized responder profile: the hashed tag, the response vector, and
/// the per-frequency scores their sessions established.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnonymizedProfile {
    /// One-way tag: first 16 hex chars of SHA-256("gamma-profile" ‖ person_id).
    /// Never the `person_id` itself.
    pub profile_tag: String,
    /// Raw (un-normalized) 20-field response vector.
    pub vector: [f64; VECTOR_DIM],
    /// `(frequency_hz, safe-entrainment score)` summaries from this profile's
    /// safe sessions — the transferable response surface.
    pub frequency_scores: Vec<(f64, f64)>,
}

impl AnonymizedProfile {
    /// Derive the one-way profile tag from a pseudonymous person id.
    pub fn tag_for(person_id: &str) -> String {
        let h = stable_hash(&[b"gamma-profile", person_id.as_bytes()]);
        let mut s = String::with_capacity(16);
        for b in &h[..8] {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

/// A cohort prior for one frequency, produced by [`ProfileStore::warm_start_prior`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CohortPrior {
    pub frequency_hz: f64,
    /// Similarity-weighted mean score across the k nearest profiles.
    pub expected_score: f64,
    /// Noise variance to attach to the pseudo-observation: grows with cohort
    /// disagreement and with distance, so dissimilar or conflicting cohorts
    /// are trusted less.
    pub noise_var: f64,
}

/// Deterministic in-memory store of anonymized profiles.
///
/// Linear-scan kNN: exact, allocation-light, and fast at research-cohort scale
/// (sub-µs at hundreds of profiles). An HNSW backend (the `ruvector` crates)
/// is a drop-in replacement once cohorts grow past ~10⁵ — the public API is
/// already shaped for it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileStore {
    profiles: Vec<AnonymizedProfile>,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored profiles.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Insert or replace (by `profile_tag`) a profile.
    pub fn upsert(&mut self, profile: AnonymizedProfile) {
        if let Some(p) = self
            .profiles
            .iter_mut()
            .find(|p| p.profile_tag == profile.profile_tag)
        {
            *p = profile;
        } else {
            self.profiles.push(profile);
        }
    }

    /// k nearest profiles to `query` (raw vector) as `(index, distance)`,
    /// ascending distance, ties broken by insertion index (deterministic).
    pub fn k_nearest(&self, query: &[f64; VECTOR_DIM], k: usize) -> Vec<(usize, f64)> {
        let q = normalize(query);
        let mut d: Vec<(usize, f64)> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(i, p)| (i, unit_distance(&q, &normalize(&p.vector))))
            .collect();
        d.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        d.truncate(k);
        d
    }

    /// Profile by index (as returned from [`k_nearest`](Self::k_nearest)).
    pub fn profile(&self, idx: usize) -> Option<&AnonymizedProfile> {
        self.profiles.get(idx)
    }

    /// Build cohort priors for a new person: for each integer frequency the
    /// k nearest profiles have scored, the similarity-weighted mean score and
    /// an honesty-scaled noise variance (ADR-250 §10 item 3 + item 6
    /// "protocol recommendation").
    ///
    /// `base_noise` is the optimizer's real-observation noise; priors carry at
    /// least `PRIOR_NOISE_FLOOR ×` that, inflated further by cohort variance
    /// and mean neighbor distance.
    pub fn warm_start_prior(
        &self,
        query: &[f64; VECTOR_DIM],
        k: usize,
        base_noise: f64,
    ) -> Vec<CohortPrior> {
        let neighbors = self.k_nearest(query, k);
        if neighbors.is_empty() {
            return Vec::new();
        }
        // Bucket scores by quantized frequency (0.1 Hz) across neighbors,
        // weighting each neighbor by 1/(1+distance).
        use std::collections::BTreeMap;
        let mut buckets: BTreeMap<i64, Vec<(f64, f64)>> = BTreeMap::new(); // q_hz -> (score, weight)
        for &(idx, dist) in &neighbors {
            let w = 1.0 / (1.0 + dist);
            for &(hz, score) in &self.profiles[idx].frequency_scores {
                if !hz.is_finite() || !score.is_finite() {
                    continue;
                }
                let q = (hz * 10.0).round() as i64;
                buckets.entry(q).or_default().push((score, w));
            }
        }
        let mean_dist =
            neighbors.iter().map(|(_, d)| d).sum::<f64>() / neighbors.len() as f64;
        buckets
            .into_iter()
            .map(|(q, entries)| {
                let wsum: f64 = entries.iter().map(|(_, w)| w).sum();
                let mean: f64 = entries.iter().map(|(s, w)| s * w).sum::<f64>() / wsum;
                let var: f64 = entries
                    .iter()
                    .map(|(s, w)| w * (s - mean) * (s - mean))
                    .sum::<f64>()
                    / wsum;
                CohortPrior {
                    frequency_hz: q as f64 / 10.0,
                    expected_score: mean,
                    // Floor × base, inflated by cohort disagreement and distance.
                    noise_var: base_noise * Self::PRIOR_NOISE_FLOOR * (1.0 + mean_dist)
                        + var,
                }
            })
            .collect()
    }

    /// Minimum factor by which a cohort prior is noisier than a real
    /// observation (priors must never outweigh measured sessions).
    pub const PRIOR_NOISE_FLOOR: f64 = 25.0;

    /// Deterministic k-means over normalized vectors (ADR-250 §10 item 5):
    /// farthest-point seeding from index 0, fixed `iters` Lloyd steps, ties to
    /// the lowest cluster index. Returns each profile's cluster assignment.
    /// Returns an empty vec if the store is empty or `k == 0`.
    pub fn cluster(&self, k: usize, iters: usize) -> Vec<usize> {
        let n = self.profiles.len();
        if n == 0 || k == 0 {
            return Vec::new();
        }
        let k = k.min(n);
        let pts: Vec<[f64; VECTOR_DIM]> =
            self.profiles.iter().map(|p| normalize(&p.vector)).collect();

        // Farthest-point initialization (deterministic, no RNG).
        let mut centers: Vec<[f64; VECTOR_DIM]> = vec![pts[0]];
        while centers.len() < k {
            let (far_idx, _) = pts
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let dmin = centers
                        .iter()
                        .map(|c| unit_distance(p, c))
                        .fold(f64::INFINITY, f64::min);
                    (i, dmin)
                })
                .fold((0usize, -1.0f64), |acc, (i, d)| if d > acc.1 { (i, d) } else { acc });
            centers.push(pts[far_idx]);
        }

        let mut assign = vec![0usize; n];
        for _ in 0..iters {
            // Assignment step.
            for (i, p) in pts.iter().enumerate() {
                let mut best = 0usize;
                let mut bd = f64::INFINITY;
                for (c, center) in centers.iter().enumerate() {
                    let d = unit_distance(p, center);
                    if d < bd {
                        bd = d;
                        best = c;
                    }
                }
                assign[i] = best;
            }
            // Update step.
            for (c, center) in centers.iter_mut().enumerate() {
                let members: Vec<&[f64; VECTOR_DIM]> = pts
                    .iter()
                    .zip(&assign)
                    .filter(|(_, &a)| a == c)
                    .map(|(p, _)| p)
                    .collect();
                if members.is_empty() {
                    continue; // keep the old center (deterministic)
                }
                let mut mean = [0.0; VECTOR_DIM];
                for m in &members {
                    for (dst, src) in mean.iter_mut().zip(m.iter()) {
                        *dst += src;
                    }
                }
                for dst in mean.iter_mut() {
                    *dst /= members.len() as f64;
                }
                *center = mean;
            }
        }
        assign
    }
}

/// Fields that participate in **drift** distance: the person's physiology and
/// response, *not* the stimulus parameters (indices 11–16) — those are inputs
/// the protocol changes deliberately (the calibration sweep swings frequency
/// across the whole band) and must not register as the person drifting.
pub const DRIFT_MASK: [bool; VECTOR_DIM] = [
    true,  // baseline_gamma
    true,  // baseline_alpha
    true,  // alpha_gamma_ratio
    true,  // gamma_power_gain
    true,  // phase_locking_value
    true,  // breathing_rate
    true,  // breathing_stability
    true,  // motion_artifact
    true,  // posture_state
    true,  // sleep_state
    true,  // restlessness_score
    false, // stimulus_frequency   (protocol input)
    false, // brightness_level     (protocol input)
    false, // sound_level          (protocol input)
    false, // duty_cycle           (protocol input)
    false, // phase_offset         (protocol input)
    false, // session_duration     (protocol input)
    true,  // comfort_score
    true,  // adherence_score
    true,  // adverse_event_flag
];

/// Euclidean distance over the [`DRIFT_MASK`]-selected fields of two
/// normalized vectors.
fn drift_distance(a: &[f64; VECTOR_DIM], b: &[f64; VECTOR_DIM]) -> f64 {
    a.iter()
        .zip(b)
        .zip(DRIFT_MASK.iter())
        .filter(|(_, &m)| m)
        .map(|((x, y), _)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

/// Drift status for one person (ADR-250 §10 item 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriftStatus {
    /// Not enough sessions yet to judge.
    Warmup,
    /// Latest vector is consistent with this person's running centroid.
    Stable,
    /// Latest vector departed from the centroid — recommend recalibration
    /// (re-run the Phase-1 sweep) before trusting further optimization.
    Drifted,
}

/// Per-person drift detector: running mean (Welford) of the normalized
/// response vector; a session whose [`DRIFT_MASK`]-restricted distance from
/// the centroid exceeds `threshold` flags drift. Deterministic and O(1) per
/// update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftDetector {
    centroid: [f64; VECTOR_DIM],
    count: u64,
    /// Distance (in normalized space) above which a session counts as drifted.
    pub threshold: f64,
    /// Sessions required before drift can be judged.
    pub warmup: u64,
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self {
            centroid: [0.0; VECTOR_DIM],
            count: 0,
            threshold: 0.35,
            warmup: 3,
        }
    }
}

impl DriftDetector {
    /// Feed the post-session response vector; returns the drift judgment for
    /// this session. The centroid update happens *after* the judgment, so a
    /// drifted session is compared against the pre-drift baseline.
    pub fn update(&mut self, raw: &[f64; VECTOR_DIM]) -> DriftStatus {
        let v = normalize(raw);
        let status = if self.count < self.warmup {
            DriftStatus::Warmup
        } else if drift_distance(&v, &self.centroid) > self.threshold {
            DriftStatus::Drifted
        } else {
            DriftStatus::Stable
        };
        // Welford running-mean update.
        self.count += 1;
        let inv = 1.0 / self.count as f64;
        for (c, x) in self.centroid.iter_mut().zip(v.iter()) {
            *c += (x - *c) * inv;
        }
        status
    }

    /// Sessions observed so far.
    pub fn sessions(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(tag: &str, freq: f64, peak_score: f64) -> AnonymizedProfile {
        let mut vector = [0.5; VECTOR_DIM];
        vector[5] = 13.0; // breathing_rate in range
        vector[11] = freq; // stimulus_frequency
        AnonymizedProfile {
            profile_tag: tag.into(),
            vector,
            frequency_scores: vec![
                (freq - 1.0, peak_score - 0.2),
                (freq, peak_score),
                (freq + 1.0, peak_score - 0.2),
            ],
        }
    }

    #[test]
    fn tag_is_one_way_and_stable() {
        let t1 = AnonymizedProfile::tag_for("subject-A");
        let t2 = AnonymizedProfile::tag_for("subject-A");
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 16);
        assert!(!t1.contains("subject"));
        assert_ne!(t1, AnonymizedProfile::tag_for("subject-B"));
    }

    #[test]
    fn knn_orders_by_distance_with_deterministic_ties() {
        let mut store = ProfileStore::new();
        store.upsert(profile("a", 38.0, 0.8));
        store.upsert(profile("b", 42.0, 0.8));
        store.upsert(profile("c", 38.0, 0.8)); // identical vector to "a"

        let mut q = [0.5; VECTOR_DIM];
        q[5] = 13.0;
        q[11] = 38.0;
        let nn = store.k_nearest(&q, 2);
        assert_eq!(nn.len(), 2);
        // "a" (index 0) and "c" (index 2) are equidistant; tie → lower index first.
        assert_eq!(nn[0].0, 0);
        assert_eq!(nn[1].0, 2);
    }

    #[test]
    fn upsert_replaces_by_tag() {
        let mut store = ProfileStore::new();
        store.upsert(profile("a", 38.0, 0.8));
        store.upsert(profile("a", 42.0, 0.9));
        assert_eq!(store.len(), 1);
        assert_eq!(store.profile(0).unwrap().frequency_scores[1].0, 42.0);
    }

    #[test]
    fn warm_start_prior_is_noisier_than_real_observations() {
        let mut store = ProfileStore::new();
        store.upsert(profile("a", 39.0, 0.8));
        store.upsert(profile("b", 39.0, 0.7));
        let mut q = [0.5; VECTOR_DIM];
        q[5] = 13.0;
        q[11] = 39.0;
        let base_noise = 1e-4;
        let priors = store.warm_start_prior(&q, 2, base_noise);
        assert!(!priors.is_empty());
        for p in &priors {
            assert!(p.noise_var >= base_noise * ProfileStore::PRIOR_NOISE_FLOOR);
            assert!(p.expected_score.is_finite());
        }
        // The shared peak frequency carries the highest expected score.
        let best = priors
            .iter()
            .max_by(|a, b| a.expected_score.partial_cmp(&b.expected_score).unwrap())
            .unwrap();
        assert_eq!(best.frequency_hz, 39.0);
    }

    #[test]
    fn warm_start_empty_store_returns_nothing() {
        let store = ProfileStore::new();
        let q = [0.5; VECTOR_DIM];
        assert!(store.warm_start_prior(&q, 3, 1e-4).is_empty());
    }

    #[test]
    fn clustering_separates_detuned_groups() {
        let mut store = ProfileStore::new();
        // Two clear groups: peaks near 37 Hz and near 43 Hz.
        for i in 0..4 {
            store.upsert(profile(&format!("lo{i}"), 37.0, 0.8));
            store.upsert(profile(&format!("hi{i}"), 43.0, 0.8));
        }
        let assign = store.cluster(2, 10);
        assert_eq!(assign.len(), 8);
        // All "lo" profiles share a cluster, all "hi" share the other.
        let lo: Vec<usize> = (0..8).step_by(2).map(|i| assign[i]).collect();
        let hi: Vec<usize> = (1..8).step_by(2).map(|i| assign[i]).collect();
        assert!(lo.iter().all(|&c| c == lo[0]));
        assert!(hi.iter().all(|&c| c == hi[0]));
        assert_ne!(lo[0], hi[0]);
    }

    #[test]
    fn clustering_is_deterministic() {
        let mut store = ProfileStore::new();
        for i in 0..6 {
            store.upsert(profile(&format!("p{i}"), 36.0 + i as f64, 0.7));
        }
        assert_eq!(store.cluster(3, 5), store.cluster(3, 5));
    }

    #[test]
    fn drift_detector_warmup_then_stable_then_drift() {
        let mut d = DriftDetector::default();
        let mut calm = [0.5; VECTOR_DIM];
        calm[5] = 13.0;
        // Warmup sessions.
        for _ in 0..3 {
            assert_eq!(d.update(&calm), DriftStatus::Warmup);
        }
        // Consistent sessions are stable.
        assert_eq!(d.update(&calm), DriftStatus::Stable);
        // A strongly departed vector flags drift.
        let mut shifted = calm;
        shifted[3] = 0.0; // gamma gain collapsed
        shifted[7] = 1.0; // motion artifact saturated
        shifted[10] = 1.0; // restlessness saturated
        shifted[17] = 0.0; // comfort collapsed
        assert_eq!(d.update(&shifted), DriftStatus::Drifted);
    }

    #[test]
    fn normalize_handles_non_finite() {
        let mut v = [0.5; VECTOR_DIM];
        v[0] = f64::NAN;
        v[5] = f64::INFINITY;
        let n = normalize(&v);
        assert_eq!(n[0], 0.0);
        assert_eq!(n[5], 0.0);
    }
}

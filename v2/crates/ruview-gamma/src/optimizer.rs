//! Constrained optimizer — Phase 1 calibration, Phase 2 Bayesian optimization,
//! Phase 4 closed-loop control (ADR-250 §8).
//!
//! **Invariant (enforced by construction and asserted in tests):** every
//! [`StimulusParameters`] this module emits satisfies
//! [`SafetyEnvelope::contains`]. The optimizer searches *frequency* over the
//! envelope's 0.1 Hz grid (the ±0.1 Hz control spec, ADR-250 §18) while holding
//! intensity at conservative values — it never widens the envelope (ADR-250 §12).

use crate::math::{
    back_subst_transpose, cholesky, dot, forward_subst, normal_cdf, normal_pdf, rbf_kernel,
};
use crate::stimulus::{SafetyEnvelope, StimulusParameters};

/// Phase-1 conservative calibration sweep (ADR-250 §8): short sessions at each
/// integer Hz in the band. Hands its results to the Bayesian optimizer.
#[derive(Debug, Clone)]
pub struct CalibrationPlan {
    frequencies: Vec<f64>,
    next: usize,
}

impl CalibrationPlan {
    pub fn new(envelope: &SafetyEnvelope) -> Self {
        Self {
            frequencies: envelope.calibration_frequencies(),
            next: 0,
        }
    }

    /// Number of calibration sessions still pending.
    pub fn remaining(&self) -> usize {
        self.frequencies.len().saturating_sub(self.next)
    }

    /// The next calibration stimulus (short duration, conservative intensity),
    /// or `None` when the sweep is complete. Always inside the envelope.
    pub fn next_stimulus(
        &mut self,
        envelope: &SafetyEnvelope,
        session_minutes: f64,
    ) -> Option<StimulusParameters> {
        let f = *self.frequencies.get(self.next)?;
        self.next += 1;
        let mut s = StimulusParameters::prior();
        s.frequency_hz = f;
        s.duration_minutes = session_minutes;
        Some(envelope.clamp(s))
    }
}

/// Gaussian-process surrogate over the 1-D frequency axis with an
/// Expected-Improvement acquisition (ADR-250 §8 Phase 2).
///
/// Supports two observation classes: **real** sessions from this person
/// ([`observe`](Self::observe), noise `noise_var`) and **cohort priors** from
/// anonymized similar responders ([`observe_prior`](Self::observe_prior),
/// caller-supplied larger noise). Priors shape the posterior mean where the
/// person has no data yet, but are honestly down-weighted and never define the
/// EI incumbent — only the person's own sessions can do that.
#[derive(Debug, Clone)]
pub struct BayesianOptimizer {
    /// RBF length scale in Hz.
    pub length_scale: f64,
    /// GP signal variance.
    pub signal_var: f64,
    /// Observation noise variance (jitter; also keeps K SPD).
    pub noise_var: f64,
    /// EI exploration margin.
    pub xi: f64,
    /// Observed `(frequency_hz, score)` pairs.
    obs_x: Vec<f64>,
    obs_y: Vec<f64>,
    /// Per-observation noise variance (diagonal of the noise term in K).
    obs_noise: Vec<f64>,
    /// `true` for cohort pseudo-observations (excluded from the incumbent).
    obs_prior: Vec<bool>,
}

impl Default for BayesianOptimizer {
    fn default() -> Self {
        Self {
            length_scale: 1.5,
            signal_var: 1.0,
            noise_var: 1e-4,
            xi: 0.01,
            obs_x: Vec::new(),
            obs_y: Vec::new(),
            obs_noise: Vec::new(),
            obs_prior: Vec::new(),
        }
    }
}

impl BayesianOptimizer {
    /// Record a calibration/optimization result from a **real** session.
    pub fn observe(&mut self, frequency_hz: f64, score: f64) {
        self.obs_x.push(frequency_hz);
        self.obs_y.push(score);
        self.obs_noise.push(self.noise_var);
        self.obs_prior.push(false);
    }

    /// Record a **cohort prior** pseudo-observation (ADR-250 §10 item 3):
    /// the expected score at `frequency_hz` inferred from anonymized similar
    /// responders, with `noise_var` reflecting how little it is trusted
    /// (must be ≥ the real-observation noise; clamped up if not).
    pub fn observe_prior(&mut self, frequency_hz: f64, score: f64, noise_var: f64) {
        self.obs_x.push(frequency_hz);
        self.obs_y.push(score);
        self.obs_noise.push(noise_var.max(self.noise_var));
        self.obs_prior.push(true);
    }

    /// Number of observations so far (real + prior).
    pub fn n_obs(&self) -> usize {
        self.obs_x.len()
    }

    /// Number of **real** (non-prior) observations.
    pub fn n_real_obs(&self) -> usize {
        self.obs_prior.iter().filter(|p| !**p).count()
    }

    /// Best **real** observed score, or `None` if no real observations.
    /// Cohort priors are deliberately excluded: a borrowed expectation must
    /// never masquerade as this person's measured response.
    pub fn best(&self) -> Option<(f64, f64)> {
        let mut best: Option<(f64, f64)> = None;
        for ((&x, &y), &prior) in self.obs_x.iter().zip(&self.obs_y).zip(&self.obs_prior) {
            if prior {
                continue;
            }
            if best.map(|(_, by)| y > by).unwrap_or(true) {
                best = Some((x, y));
            }
        }
        best
    }

    /// EI incumbent: the best real observation, falling back to the best prior
    /// when the person has no sessions yet (so cohort-seeded recommendation
    /// still explores sensibly rather than treating 0 as the bar).
    fn incumbent(&self) -> Option<f64> {
        if let Some((_, by)) = self.best() {
            return Some(by);
        }
        self.obs_y.iter().copied().fold(None, |acc, y| {
            Some(match acc {
                Some(a) if a >= y => a,
                _ => y,
            })
        })
    }

    /// Factorize the GP once: Cholesky `L` of `K = RBF(X,X)+diag(noise)` and
    /// the weight vector `alpha = K⁻¹ y`. Both depend only on the observations,
    /// not on any query point, so a single fit serves the whole acquisition
    /// grid. Returns `None` when there are no observations or `K` is not SPD.
    ///
    /// The per-query arithmetic in [`GpFit::predict`] is identical to the old
    /// inline path, so predictions (and therefore the session witness) are
    /// bit-for-bit unchanged — this is a pure work-elimination optimization.
    fn fit(&self) -> Option<GpFit<'_>> {
        let n = self.obs_x.len();
        if n == 0 {
            return None;
        }
        // K (lower triangle is all Cholesky reads) = RBF(X,X) + diag(noise).
        let mut k = vec![0.0f64; n * n];
        for i in 0..n {
            for j in 0..=i {
                let mut v = rbf_kernel(
                    &[self.obs_x[i]],
                    &[self.obs_x[j]],
                    self.length_scale,
                    self.signal_var,
                );
                if i == j {
                    v += self.obs_noise[i];
                }
                k[i * n + j] = v;
            }
        }
        let l = cholesky(&k, n)?;
        // alpha = K⁻¹ y  (solve L Lᵀ alpha = y) — computed once.
        let y1 = forward_subst(&l, &self.obs_y, n);
        let alpha = back_subst_transpose(&l, &y1, n);
        Some(GpFit {
            obs_x: &self.obs_x,
            l,
            alpha,
            n,
            length_scale: self.length_scale,
            signal_var: self.signal_var,
        })
    }

    /// GP posterior `(mean, variance)` at `x`. Falls back to `(0, signal_var)`
    /// (the prior) when there are no observations or K is not SPD.
    pub fn predict(&self, x: f64) -> (f64, f64) {
        match self.fit() {
            Some(fit) => fit.predict(x),
            None => (0.0, self.signal_var),
        }
    }

    /// Expected Improvement (for maximization) at `x`.
    pub fn expected_improvement(&self, x: f64) -> f64 {
        let best = match self.incumbent() {
            Some(by) => by,
            None => return self.signal_var.sqrt(), // pure exploration
        };
        match self.fit() {
            Some(fit) => fit.expected_improvement(x, best, self.xi),
            None => self.signal_var.sqrt(),
        }
    }

    /// Recommend the next stimulus by maximizing EI over the envelope's 0.1 Hz
    /// grid, holding `base`'s intensity (clamped). The result is guaranteed
    /// inside the envelope. With no observations it returns the 40 Hz prior.
    ///
    /// Fits the GP **once** and reuses the factorization across every grid
    /// candidate (was: a full Cholesky per candidate, ~82× the work).
    pub fn recommend(
        &self,
        envelope: &SafetyEnvelope,
        base: &StimulusParameters,
    ) -> Recommendation {
        let fit = match self.fit() {
            Some(f) => f,
            None => {
                let s = envelope.clamp(*base);
                return Recommendation {
                    stimulus: s,
                    expected_improvement: 0.0,
                    predicted_score: 0.0,
                    confidence: 0.0,
                };
            }
        };
        // incumbent() is Some here (fit exists ⇒ ≥1 observation).
        let best = self.incumbent().unwrap_or(0.0);
        let grid = fine_grid(envelope);
        let mut best_f = base.frequency_hz;
        let mut best_ei = f64::NEG_INFINITY;
        for &f in &grid {
            let ei = fit.expected_improvement(f, best, self.xi);
            if ei > best_ei {
                best_ei = ei;
                best_f = f;
            }
        }
        let (mu, var) = fit.predict(best_f);
        let mut s = *base;
        s.frequency_hz = best_f;
        let s = envelope.clamp(s);
        Recommendation {
            stimulus: s,
            expected_improvement: best_ei.max(0.0),
            predicted_score: mu,
            // Confidence shrinks with posterior variance (more data near the
            // pick → tighter → higher confidence), squashed to [0,1].
            confidence: 1.0 / (1.0 + var.sqrt()),
        }
    }
}

/// A cached GP factorization (Cholesky `L` + weights `alpha`) over a fixed set
/// of observations, reused across many query points in one acquisition pass.
struct GpFit<'a> {
    obs_x: &'a [f64],
    /// Cholesky factor of `K` (lower-triangular, `n×n` row-major).
    l: Vec<f64>,
    /// `alpha = K⁻¹ y`.
    alpha: Vec<f64>,
    n: usize,
    length_scale: f64,
    signal_var: f64,
}

impl GpFit<'_> {
    /// Posterior `(mean, variance)` at `x`. Bit-identical to the former inline
    /// computation; only the shared `L`/`alpha` are now precomputed.
    fn predict(&self, x: f64) -> (f64, f64) {
        let n = self.n;
        // k* = RBF(X, x)
        let kstar: Vec<f64> = (0..n)
            .map(|i| rbf_kernel(&[self.obs_x[i]], &[x], self.length_scale, self.signal_var))
            .collect();
        let mean = dot(&kstar, &self.alpha);
        // var = k(x,x) - v·v, where L v = k*
        let v = forward_subst(&self.l, &kstar, n);
        let var = (self.signal_var - dot(&v, &v)).max(0.0);
        (mean, var)
    }

    /// Expected Improvement at `x` given the incumbent `best` and margin `xi`.
    fn expected_improvement(&self, x: f64, best: f64, xi: f64) -> f64 {
        let (mu, var) = self.predict(x);
        let sigma = var.sqrt();
        if sigma <= 1e-12 {
            return 0.0;
        }
        let imp = mu - best - xi;
        let z = imp / sigma;
        imp * normal_cdf(z) + sigma * normal_pdf(z)
    }
}

/// A recommendation plus its explainability fields (ADR-250 §14 response,
/// §16 "explainable recommendation").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recommendation {
    pub stimulus: StimulusParameters,
    pub expected_improvement: f64,
    pub predicted_score: f64,
    pub confidence: f64,
}

/// Phase-4 closed-loop controller (ADR-250 §8). Applies bounded mid-session
/// adjustments; every output is re-clamped to the envelope.
#[derive(Debug, Clone, Copy)]
pub struct ClosedLoopController {
    /// Max single-step frequency nudge in Hz (kept small for safety).
    pub max_freq_step_hz: f64,
    /// Entrainment below this triggers a corrective nudge.
    pub entrainment_floor: f64,
    /// Comfort below this triggers an intensity reduction.
    pub comfort_floor: f64,
    /// Multiplicative intensity reduction on discomfort.
    pub intensity_backoff: f64,
}

impl Default for ClosedLoopController {
    fn default() -> Self {
        Self {
            max_freq_step_hz: 0.5,
            entrainment_floor: 0.3,
            comfort_floor: 0.5,
            intensity_backoff: 0.8,
        }
    }
}

/// One closed-loop action recommendation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopAction {
    /// Continue unchanged.
    Hold,
    /// Adjust to a new (already envelope-clamped) stimulus.
    Adjust(StimulusParameters),
}

impl ClosedLoopController {
    /// Decide the next in-session action from live entrainment/comfort.
    /// `gradient_sign` indicates which frequency direction recently improved
    /// entrainment (+1 up, −1 down, 0 unknown); the nudge respects it.
    pub fn step(
        &self,
        envelope: &SafetyEnvelope,
        current: &StimulusParameters,
        live_entrainment: f64,
        live_comfort: f64,
        gradient_sign: f64,
    ) -> LoopAction {
        // Comfort first: discomfort always reduces intensity (safety-leaning).
        if live_comfort < self.comfort_floor {
            let mut s = *current;
            s.brightness_level *= self.intensity_backoff;
            s.volume_level *= self.intensity_backoff;
            return LoopAction::Adjust(envelope.clamp(s));
        }
        // Entrainment fading: small bounded frequency nudge toward improvement.
        if live_entrainment < self.entrainment_floor {
            let dir = if gradient_sign >= 0.0 { 1.0 } else { -1.0 };
            let mut s = *current;
            s.frequency_hz += dir * self.max_freq_step_hz;
            return LoopAction::Adjust(envelope.clamp(s));
        }
        LoopAction::Hold
    }
}

/// The 0.1 Hz candidate grid over the envelope (ADR-250 §18 ±0.1 Hz precision).
fn fine_grid(envelope: &SafetyEnvelope) -> Vec<f64> {
    let lo = (envelope.min_hz * 10.0).round() as i64;
    let hi = (envelope.max_hz * 10.0).round() as i64;
    (lo..=hi).map(|i| i as f64 / 10.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_sweep_covers_band_and_stays_safe() {
        let env = SafetyEnvelope::conservative();
        let mut plan = CalibrationPlan::new(&env);
        let mut seen = Vec::new();
        while let Some(s) = plan.next_stimulus(&env, 5.0) {
            assert!(env.contains(&s));
            seen.push(s.frequency_hz);
        }
        assert_eq!(seen.first(), Some(&36.0));
        assert_eq!(seen.last(), Some(&44.0));
        assert_eq!(plan.remaining(), 0);
    }

    #[test]
    fn gp_recovers_a_quadratic_peak() {
        // Synthetic score surface peaked at 39.5 Hz.
        let env = SafetyEnvelope::conservative();
        let mut bo = BayesianOptimizer::default();
        let truth = |f: f64| 1.0 - 0.05 * (f - 39.5).powi(2);
        for f in env.calibration_frequencies() {
            bo.observe(f, truth(f));
        }
        let rec = bo.recommend(&env, &StimulusParameters::prior());
        assert!(env.contains(&rec.stimulus));
        // Should land within ±1 Hz of the true peak (ADR-250 §18 repeatability).
        assert!((rec.stimulus.frequency_hz - 39.5).abs() <= 1.0);
    }

    #[test]
    fn recommendation_is_always_in_envelope() {
        let env = SafetyEnvelope::conservative();
        let mut bo = BayesianOptimizer::default();
        // Adversarial: pretend the band edge is best.
        for f in env.calibration_frequencies() {
            bo.observe(f, if f >= 44.0 { 10.0 } else { 0.0 });
        }
        let rec = bo.recommend(&env, &StimulusParameters::prior());
        assert!(env.contains(&rec.stimulus));
        assert!(rec.stimulus.frequency_hz <= env.max_hz);
    }

    #[test]
    fn no_observations_returns_prior() {
        let env = SafetyEnvelope::conservative();
        let bo = BayesianOptimizer::default();
        let rec = bo.recommend(&env, &StimulusParameters::prior());
        assert_eq!(rec.stimulus.frequency_hz, 40.0);
    }

    #[test]
    fn closed_loop_reduces_intensity_on_discomfort() {
        let env = SafetyEnvelope::conservative();
        let ctl = ClosedLoopController::default();
        let cur = StimulusParameters::prior();
        match ctl.step(&env, &cur, 0.6, 0.2, 0.0) {
            LoopAction::Adjust(s) => {
                assert!(s.brightness_level < cur.brightness_level);
                assert!(env.contains(&s));
            }
            LoopAction::Hold => panic!("should have adjusted intensity"),
        }
    }

    #[test]
    fn closed_loop_nudge_stays_in_envelope() {
        let env = SafetyEnvelope::conservative();
        let ctl = ClosedLoopController::default();
        let mut cur = StimulusParameters::prior();
        cur.frequency_hz = 43.8; // near the upper edge
        match ctl.step(&env, &cur, 0.1, 0.9, 1.0) {
            LoopAction::Adjust(s) => assert!(env.contains(&s)),
            LoopAction::Hold => {}
        }
    }

    #[test]
    fn closed_loop_holds_when_healthy() {
        let env = SafetyEnvelope::conservative();
        let ctl = ClosedLoopController::default();
        let cur = StimulusParameters::prior();
        assert_eq!(ctl.step(&env, &cur, 0.8, 0.9, 0.0), LoopAction::Hold);
    }
}

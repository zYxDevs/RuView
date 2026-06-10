//! # ruview-gamma — Adaptive Gamma Entrainment (ADR-250)
//!
//! Governed, deterministic, safety-constrained personalization of 40 Hz-prior
//! multisensory stimulation. Treats 40 Hz as the evidence-based **starting
//! prior**, then learns each person's safe entrainment response curve using
//! passive [RuView] sensing, optional EEG, a constrained optimizer, and
//! auditable [RuFlo] workflows.
//!
//! > **Not medical advice / not a medical device.** This crate is a research
//! > and engineering platform. The only product claim it makes is
//! > [`ruflo::PRODUCT_CLAIM`] — *"personalized entrainment optimization"* — and
//! > never Alzheimer's treatment, amyloid clearance, or any clinical outcome
//! > (ADR-250 §19 Non-Goals). It performs no hardware actuation: real stimulus
//! > delivery, RF sensing, and EEG arrive through external adapters behind
//! > feature flags after this governed software core ships (ADR-250 §21).
//!
//! ## Design discipline
//!
//! A deterministic, dependency-light **leaf crate** (no internal RuView deps;
//! ChaCha20 PRNG; SHA-256 witness) — the same posture as `nvsim`. The
//! optimizer, safety envelope, RuVector update logic, and session witness are
//! all testable and replayable bit-exactly *before any human exposure*. See
//! [`proof::Proof`] for the deterministic bundle proof.
//!
//! ## Pipeline (ADR-250 §4)
//!
//! ```text
//! enroll (consent + exclusion screen)        ── ruflo
//!   → calibration sweep 36–44 Hz             ── optimizer::CalibrationPlan
//!   → simulated/observed response            ── simulator (M1) / external (M2-4)
//!   → safety monitor (hard stop, latched)    ── safety
//!   → safe-entrainment score                 ── objective
//!   → personal response vector update        ── response (RuVector)
//!   → Bayesian / bandit recommendation       ── optimizer / bandit
//!   → witnessed audit record                 ── session + ruflo
//! ```
//!
//! ## The safety invariant
//!
//! **No recommendation, calibration step, bandit arm, or closed-loop nudge can
//! ever emit a [`stimulus::StimulusParameters`] outside the
//! [`stimulus::SafetyEnvelope`].** Every emitting path clamps to the envelope
//! and is asserted against [`stimulus::SafetyEnvelope::contains`] in tests. The
//! optimizer never widens the envelope — only an operator constructs a wider
//! one deliberately (ADR-250 §12).
//!
//! ## Quick start
//!
//! ```
//! use ruview_gamma::{
//!     ruflo::{Consent, RufloGovernor},
//!     response::RuViewState,
//!     simulator::{LatentPerson, ResponseSimulator},
//!     stimulus::{SafetyEnvelope, StimulusParameters},
//! };
//!
//! let envelope = SafetyEnvelope::conservative();
//! let mut gov = RufloGovernor::enroll("subject-001", envelope, &[], Consent::Granted)
//!     .expect("cleared to participate");
//!
//! // Milestone 1: drive the governed loop with the deterministic simulator.
//! let sim = ResponseSimulator::new(42);
//! let latent = LatentPerson::from_id("subject-001");
//! let state = RuViewState::calm_baseline();
//! gov.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();
//!
//! let rec = gov.recommend(&StimulusParameters::prior());
//! assert!(envelope.contains(&rec.stimulus)); // always inside the envelope
//! ```

pub mod bandit;
pub mod math;
pub mod objective;
pub mod optimizer;
pub mod proof;
pub mod response;
pub mod ruflo;
pub mod ruvector;
pub mod safety;
pub mod session;
pub mod simulator;
pub mod stimulus;

use thiserror::Error;

/// Crate-level error type (re-exported for callers who want a single error to
/// match on; most modules expose their own typed errors).
#[derive(Debug, Error)]
pub enum GammaError {
    /// A governance refusal (consent / exclusion / envelope).
    #[error(transparent)]
    Governance(#[from] ruflo::GovernanceError),
    /// JSON (de)serialization of a session record failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// The single allowed product claim. Re-exported at crate root for prominence.
pub use ruflo::PRODUCT_CLAIM;

#[cfg(test)]
mod integration_tests {
    use crate::response::RuViewState;
    use crate::ruflo::{Consent, RufloGovernor, TrialMode};
    use crate::ruvector::{DriftStatus, ProfileStore};
    use crate::simulator::{LatentPerson, ResponseSimulator};
    use crate::stimulus::{SafetyEnvelope, StimulusParameters};

    /// ADR-250 §10 item 3 end-to-end: a cohort of calibrated responders with
    /// similar physiology warm-starts a new person's optimizer — the very
    /// first recommendation already points near the cohort's peak band instead
    /// of the flat 40 Hz prior, while never counting as measured data
    /// (`n_real_obs == 0`, `best() == None`).
    #[test]
    fn cohort_warm_start_improves_first_recommendation() {
        let env = SafetyEnvelope::conservative();
        let sim = ResponseSimulator::new(404);
        let state = RuViewState::calm_baseline();

        // Find a latent subject with a clearly detuned peak, then build a
        // cohort of 3 donors with the *same* latent physiology (similar
        // responders) who each ran a full calibration.
        let mut chosen = None;
        for n in 0..50 {
            let id = format!("cohort-seed-{n}");
            let p = LatentPerson::from_id(&id);
            if (p.peak_hz - 40.0).abs() > 2.0 && p.peak_hz > 37.0 && p.peak_hz < 43.0 {
                chosen = Some((id, p));
                break;
            }
        }
        let (seed_id, latent) = chosen.expect("a detuned subject exists");

        let mut store = ProfileStore::new();
        for d in 0..3 {
            let donor_id = format!("{seed_id}-donor-{d}");
            let mut donor =
                RufloGovernor::enroll(&donor_id, env, &[], Consent::Granted).unwrap();
            donor.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();
            store.upsert(donor.export_anonymized_profile());
        }

        // New person, zero sessions: cold start recommends the 40 Hz prior...
        let cold = RufloGovernor::enroll("newcomer", env, &[], Consent::Granted).unwrap();
        let cold_rec = cold.recommend(&StimulusParameters::prior());
        assert_eq!(cold_rec.stimulus.frequency_hz, 40.0);

        // ...while a cohort-seeded start points into the cohort's peak band.
        let mut warm = RufloGovernor::enroll("newcomer", env, &[], Consent::Granted).unwrap();
        let n_priors = warm.seed_from_cohort(&store, 3);
        assert!(n_priors > 0);
        let warm_rec = warm.recommend(&StimulusParameters::prior());
        assert!(env.contains(&warm_rec.stimulus));
        let cold_err = (cold_rec.stimulus.frequency_hz - latent.peak_hz).abs();
        let warm_err = (warm_rec.stimulus.frequency_hz - latent.peak_hz).abs();
        assert!(
            warm_err < cold_err,
            "warm-start ({} Hz) should beat cold start ({} Hz) for peak {} Hz",
            warm_rec.stimulus.frequency_hz,
            cold_rec.stimulus.frequency_hz,
            latent.peak_hz
        );
        // Honesty: priors are not measured data.
        assert!(warm.audit_log().is_empty());
        assert_eq!(warm.clinician_report().n_sessions, 0);
    }

    /// ADR-250 §10 item 4: a stable participant stays `Stable`; collapsing
    /// their physiology (restless, uncomfortable, no entrainment) flags
    /// `Drifted`, recommending recalibration.
    #[test]
    fn drift_is_flagged_when_response_collapses() {
        let env = SafetyEnvelope::conservative();
        let sim = ResponseSimulator::new(77);
        let latent = LatentPerson::from_id("drift-subject");
        let calm = RuViewState::calm_baseline();
        let mut gov = RufloGovernor::enroll("drift-subject", env, &[], Consent::Granted).unwrap();

        // Settle in: calibration sweep (9 sessions) → stable.
        gov.run_calibration(&sim, &latent, &calm, 5.0, 0).unwrap();
        assert_eq!(gov.drift_status(), DriftStatus::Stable);

        // Physiology collapses: restless, fragmented breathing, low stillness.
        let mut collapsed = calm;
        collapsed.restlessness_score = 1.0;
        collapsed.stillness_score = 0.0;
        collapsed.breathing_stability = 0.1;
        collapsed.motion_artifact = 0.9;
        let stim = StimulusParameters::prior();
        let mut drifted = false;
        for i in 0..6 {
            gov.run_session(&sim, &latent, &collapsed, &stim, 100 + i).unwrap();
            if gov.drift_status() == DriftStatus::Drifted {
                drifted = true;
                break;
            }
        }
        assert!(drifted, "collapsed physiology must flag drift");
    }

    /// ADR-250 §18 acceptance: adaptive recommendation beats the fixed 40 Hz
    /// prior in mean simulated entrainment for a person whose peak is away
    /// from 40 Hz. (We assert improvement, not the exact ≥20% figure, which is
    /// simulator-dependent; the harness for the quantitative claim is here.)
    #[test]
    fn adaptive_beats_fixed_40hz_for_detuned_person() {
        let env = SafetyEnvelope::conservative();
        let sim = ResponseSimulator::new(2024);
        // Pick a subject whose latent peak is clearly off 40 Hz.
        let mut chosen = None;
        for n in 0..50 {
            let id = format!("detuned-{n}");
            let p = LatentPerson::from_id(&id);
            if (p.peak_hz - 40.0).abs() > 1.5 {
                chosen = Some((id, p));
                break;
            }
        }
        let (id, latent) = chosen.expect("a detuned subject exists");
        let state = RuViewState::calm_baseline();

        // Learn via calibration.
        let mut gov = RufloGovernor::enroll(&id, env, &[], Consent::Granted).unwrap();
        gov.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();
        let rec = gov.recommend(&StimulusParameters::prior());

        // Compare mean simulated entrainment over repeated sessions: fixed 40 Hz
        // vs the adaptive recommendation. Use fresh session indices.
        let fixed = {
            let mut s = StimulusParameters::prior();
            s.frequency_hz = 40.0;
            s
        };
        let mean = |stim: &StimulusParameters| -> f64 {
            (0..20)
                .map(|i| sim.simulate(&latent, &state, stim, 1000 + i).eeg.gamma_power_gain)
                .sum::<f64>()
                / 20.0
        };
        assert!(env.contains(&rec.stimulus));
        assert!(mean(&rec.stimulus) > mean(&fixed));
    }

    /// End-to-end: a blinded sham arm yields lower mean entrainment than the
    /// open arm across a small cohort — the controlled-trial primitive.
    #[test]
    fn sham_arm_shows_no_entrainment_across_cohort() {
        let env = SafetyEnvelope::conservative();
        let sim = ResponseSimulator::new(7);
        let state = RuViewState::calm_baseline();
        let mut open_sum = 0.0;
        let mut sham_sum = 0.0;
        for n in 0..8 {
            let id = format!("cohort-{n}");
            let latent = LatentPerson::from_id(&id);
            let mut stim = StimulusParameters::prior();
            stim.frequency_hz = (latent.peak_hz * 10.0).round() / 10.0;
            stim.frequency_hz = stim.frequency_hz.clamp(36.0, 44.0);

            let mut open = RufloGovernor::enroll(&id, env, &[], Consent::Granted).unwrap();
            let o = open.run_session(&sim, &latent, &state, &stim, 0).unwrap();
            open_sum += o.outcome.entrainment_score;

            let mut sham = RufloGovernor::enroll(&id, env, &[], Consent::Granted).unwrap();
            sham.set_mode(TrialMode::Sham);
            let s = sham.run_session(&sim, &latent, &state, &stim, 0).unwrap();
            sham_sum += s.outcome.entrainment_score;
        }
        assert!(open_sum > sham_sum);
    }
}

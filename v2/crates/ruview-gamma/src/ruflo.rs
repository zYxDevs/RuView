//! RuFlo governance layer (ADR-250 §11).
//!
//! The governor is the only public entry point that *runs* sessions. It
//! enforces, in order: consent → inclusion/exclusion screen → envelope check →
//! simulated/observed session → safety monitor → objective score → RuVector
//! update → witnessed audit record. It also owns trial-mode separation (sham /
//! blinding) and the **claim-discipline** statement (ADR-250 §18: "no disease
//! treatment claim").

use crate::objective::{SafeEntrainmentObjective, ScoreInputs};
use crate::optimizer::{BayesianOptimizer, CalibrationPlan, Recommendation};
use crate::response::{PersonResponseVector, RuViewState, SessionObservation, SubjectiveReport};
use crate::ruvector::{AnonymizedProfile, DriftDetector, DriftStatus, ProfileStore};
use crate::safety::{
    ExclusionCondition, ExclusionScreen, SafetyMonitor, SafetyTick, ScreenOutcome, StopReason,
};
use crate::session::{Outcome, SessionBuilder, SessionRecord, VersionTriple};
use crate::simulator::{LatentPerson, ResponseSimulator};
use crate::stimulus::{SafetyEnvelope, StimulusParameters};

/// The single, immutable product claim (ADR-250 §22). Exposed so any UI/report
/// can render exactly this and nothing stronger.
pub const PRODUCT_CLAIM: &str = "personalized entrainment optimization";

/// Consent state (ADR-250 §11 RuFlo responsibility 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Granted,
    Withdrawn,
}

/// Trial mode for controlled studies (ADR-250 §21 Milestone 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialMode {
    /// Normal operation: real stimulation, adaptive optimization.
    Open,
    /// Sham: the participant-facing protocol is logged, but no entrainment is
    /// delivered (blinding). Outcomes show the no-treatment baseline.
    Sham,
}

/// Governance refusals — every one is a *safe* refusal (fail closed).
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GovernanceError {
    #[error("participant is excluded from unsupervised use: {0:?}")]
    Excluded(Vec<ExclusionCondition>),
    #[error("clinical supervision required for: {0:?}")]
    SupervisionRequired(Vec<ExclusionCondition>),
    #[error("consent not granted (or withdrawn)")]
    NoConsent,
    #[error("requested stimulus is outside the approved safety envelope")]
    OutsideEnvelope,
}

/// Clinician-facing export summary (ADR-250 §11 responsibility 9, §17).
#[derive(Debug, Clone, PartialEq)]
pub struct ClinicianReport {
    pub person_id: String,
    pub n_sessions: usize,
    pub n_safety_stops: usize,
    pub best_frequency_hz: Option<f64>,
    pub mean_entrainment: f64,
    pub adverse_event_recorded: bool,
    pub claim: &'static str,
}

/// The governed adaptive-gamma protocol runner for one participant.
pub struct RufloGovernor {
    person_id: String,
    envelope: SafetyEnvelope,
    objective: SafeEntrainmentObjective,
    optimizer: BayesianOptimizer,
    response: PersonResponseVector,
    versions: VersionTriple,
    consent: Consent,
    mode: TrialMode,
    confidence_floor: f64,
    audit: Vec<SessionRecord>,
    next_index: u64,
    // ADR-250 §10 item 4: per-person drift detection over the response vector.
    drift: DriftDetector,
    drift_status: DriftStatus,
}

impl RufloGovernor {
    /// Enroll a participant. Fails closed on exclusion or missing consent.
    pub fn enroll(
        person_id: impl Into<String>,
        envelope: SafetyEnvelope,
        conditions: &[ExclusionCondition],
        consent: Consent,
    ) -> Result<Self, GovernanceError> {
        if consent != Consent::Granted {
            return Err(GovernanceError::NoConsent);
        }
        match ExclusionScreen.evaluate(conditions) {
            ScreenOutcome::Excluded(c) => return Err(GovernanceError::Excluded(c)),
            ScreenOutcome::RequiresClinicalSupervision(c) => {
                return Err(GovernanceError::SupervisionRequired(c))
            }
            ScreenOutcome::Cleared => {}
        }
        let baseline_ruview = RuViewState::calm_baseline();
        Ok(Self {
            person_id: person_id.into(),
            objective: SafeEntrainmentObjective::new(Default::default(), envelope),
            optimizer: BayesianOptimizer::default(),
            response: PersonResponseVector::baseline(0.2, 0.5, &baseline_ruview),
            versions: VersionTriple::default(),
            consent,
            mode: TrialMode::Open,
            confidence_floor: 0.5,
            envelope,
            audit: Vec::new(),
            next_index: 0,
            drift: DriftDetector::default(),
            drift_status: DriftStatus::Warmup,
        })
    }

    /// Seed the optimizer from a cohort of anonymized similar responders
    /// (ADR-250 §10 item 3): the `k` nearest profiles' frequency responses
    /// enter as **down-weighted pseudo-observations**, shaping where the
    /// optimizer looks first without ever counting as this person's measured
    /// data ([`BayesianOptimizer::observe_prior`]). Returns how many priors
    /// were installed.
    pub fn seed_from_cohort(&mut self, store: &ProfileStore, k: usize) -> usize {
        let query = self.response.as_array();
        let priors =
            store.warm_start_prior(&query, k, self.optimizer.noise_var);
        for p in &priors {
            // Only frequencies inside this participant's envelope are usable.
            if p.frequency_hz >= self.envelope.min_hz && p.frequency_hz <= self.envelope.max_hz {
                self.optimizer
                    .observe_prior(p.frequency_hz, p.expected_score, p.noise_var);
            }
        }
        priors.len()
    }

    /// Export this participant as an anonymized profile for the cohort store
    /// (ADR-250 §10 items 3/6). Carries the one-way hashed tag, the response
    /// vector, and per-frequency scores from **safe sessions only** — never
    /// the `person_id`, never raw sensor data.
    pub fn export_anonymized_profile(&self) -> AnonymizedProfile {
        let frequency_scores: Vec<(f64, f64)> = self
            .audit
            .iter()
            .filter(|r| r.outcome.safety_pass)
            .map(|r| (r.stimulus.frequency_hz, r.outcome.entrainment_score))
            .collect();
        AnonymizedProfile {
            profile_tag: AnonymizedProfile::tag_for(&self.person_id),
            vector: self.response.as_array(),
            frequency_scores,
        }
    }

    /// Latest drift judgment (ADR-250 §10 item 4). `Drifted` recommends
    /// re-running the Phase-1 calibration sweep before trusting further
    /// optimization.
    pub fn drift_status(&self) -> DriftStatus {
        self.drift_status
    }

    /// Switch trial mode (e.g., to `Sham` for a blinded arm).
    pub fn set_mode(&mut self, mode: TrialMode) {
        self.mode = mode;
    }

    /// Withdraw consent — all subsequent `run_session` calls fail closed.
    pub fn withdraw_consent(&mut self) {
        self.consent = Consent::Withdrawn;
    }

    /// Immutable view of the audit trail (every session is witnessed).
    pub fn audit_log(&self) -> &[SessionRecord] {
        &self.audit
    }

    /// Current personal response vector (RuVector memory).
    pub fn response_vector(&self) -> &PersonResponseVector {
        &self.response
    }

    /// Run the Phase-1 calibration sweep against a simulated participant,
    /// recording every session and seeding the optimizer.
    pub fn run_calibration(
        &mut self,
        sim: &ResponseSimulator,
        latent: &LatentPerson,
        state: &RuViewState,
        session_minutes: f64,
        base_timestamp_ms: u64,
    ) -> Result<(), GovernanceError> {
        let mut plan = CalibrationPlan::new(&self.envelope);
        while let Some(stim) = plan.next_stimulus(&self.envelope, session_minutes) {
            self.run_session(sim, latent, state, &stim, base_timestamp_ms)?;
        }
        Ok(())
    }

    /// Recommend the next protocol given the current state (ADR-250 §14).
    pub fn recommend(&self, base: &StimulusParameters) -> Recommendation {
        self.optimizer.recommend(&self.envelope, base)
    }

    /// Run one governed session end-to-end. Returns the witnessed record.
    ///
    /// Fails closed if consent is absent or the stimulus is outside the
    /// envelope. Any safety stop is logged into the record (ADR-250 §18).
    pub fn run_session(
        &mut self,
        sim: &ResponseSimulator,
        latent: &LatentPerson,
        state: &RuViewState,
        stimulus: &StimulusParameters,
        timestamp_ms: u64,
    ) -> Result<SessionRecord, GovernanceError> {
        if self.consent != Consent::Granted {
            return Err(GovernanceError::NoConsent);
        }
        if !self.envelope.contains(stimulus) {
            return Err(GovernanceError::OutsideEnvelope);
        }

        let idx = self.next_index;
        self.next_index += 1;

        // --- Observe (simulated) response. ---
        let mut resp = sim.simulate(latent, state, stimulus, idx);
        if self.mode == TrialMode::Sham {
            // Blinding: no entrainment is actually delivered.
            resp.eeg.gamma_power_gain *= 0.05;
            resp.eeg.phase_locking_value *= 0.05;
        }

        // --- Safety monitor over the (single-summary) tick. ---
        let mut monitor = SafetyMonitor::new(self.confidence_floor);
        let mut safety_events = Vec::new();
        let adverse = if resp.adverse_event {
            Some(crate::safety::AdverseEvent::AbnormalDistress)
        } else {
            None
        };
        if let Some(stop) = monitor.evaluate(SafetyTick {
            adverse,
            sensor_confidence: resp.ruview.sensor_confidence,
            stimulus_in_envelope: true,
        }) {
            safety_events.push(stop);
        }
        let safety_pass = !safety_events.iter().any(StopReason::is_safety_stop);

        // --- Score the session. ---
        let subjective = SubjectiveReport {
            comfort: resp.comfort,
            fatigue: 0.2,
        };
        let score = self.objective.score(&ScoreInputs {
            stimulus,
            ruview: &resp.ruview,
            eeg: Some(&resp.eeg),
            subjective: &subjective,
            adverse_event_risk: if resp.adverse_event { 1.0 } else { 0.0 },
        });

        // --- Feed the optimizer only when the session was safe. ---
        if safety_pass {
            self.optimizer.observe(stimulus.frequency_hz, score);
        }

        // --- Update RuVector memory + drift detection (ADR-250 §10 item 4). ---
        self.response.update(&SessionObservation {
            stimulus: *stimulus,
            ruview: resp.ruview,
            eeg: Some(resp.eeg),
            subjective,
            safety_pass,
            adverse_event: resp.adverse_event,
        });
        self.drift_status = self.drift.update(&self.response.as_array());

        // --- Recommend next frequency for the record. ---
        let next = self.optimizer.recommend(&self.envelope, stimulus);

        // --- Witnessed audit record. ---
        let record = SessionBuilder::new(
            self.person_id.clone(),
            self.versions.clone(),
            timestamp_ms,
            *stimulus,
            resp.ruview,
            subjective,
            Outcome {
                entrainment_score: score,
                safety_pass,
                recommended_next_frequency_hz: next.stimulus.frequency_hz,
            },
        )
        .with_eeg(resp.eeg)
        .with_safety_events(safety_events)
        .finalize();

        self.audit.push(record.clone());
        Ok(record)
    }

    /// Build the clinician export (ADR-250 §11 responsibility 9).
    pub fn clinician_report(&self) -> ClinicianReport {
        let n = self.audit.len();
        let n_stops = self
            .audit
            .iter()
            .flat_map(|r| &r.safety_events)
            .filter(|e| e.is_safety_stop())
            .count();
        let mean = if n > 0 {
            self.audit
                .iter()
                .map(|r| r.outcome.entrainment_score)
                .sum::<f64>()
                / n as f64
        } else {
            0.0
        };
        ClinicianReport {
            person_id: self.person_id.clone(),
            n_sessions: n,
            n_safety_stops: n_stops,
            best_frequency_hz: self.optimizer.best().map(|(f, _)| f),
            mean_entrainment: mean,
            adverse_event_recorded: self.response.adverse_event_flag >= 1.0,
            claim: PRODUCT_CLAIM,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn governor() -> RufloGovernor {
        RufloGovernor::enroll(
            "subject-A",
            SafetyEnvelope::conservative(),
            &[],
            Consent::Granted,
        )
        .unwrap()
    }

    #[test]
    fn enroll_refuses_without_consent() {
        let r = RufloGovernor::enroll(
            "x",
            SafetyEnvelope::conservative(),
            &[],
            Consent::Withdrawn,
        );
        assert_eq!(r.err(), Some(GovernanceError::NoConsent));
    }

    #[test]
    fn enroll_refuses_excluded_condition() {
        let r = RufloGovernor::enroll(
            "x",
            SafetyEnvelope::conservative(),
            &[ExclusionCondition::EpilepsyOrSeizureHistory],
            Consent::Granted,
        );
        assert!(matches!(r, Err(GovernanceError::Excluded(_))));
    }

    #[test]
    fn enroll_requires_supervision_for_migraine() {
        let r = RufloGovernor::enroll(
            "x",
            SafetyEnvelope::conservative(),
            &[ExclusionCondition::SevereMigraineSensitivity],
            Consent::Granted,
        );
        assert!(matches!(r, Err(GovernanceError::SupervisionRequired(_))));
    }

    #[test]
    fn run_session_refuses_out_of_envelope_stimulus() {
        let mut g = governor();
        let sim = ResponseSimulator::new(1);
        let latent = LatentPerson::from_id("subject-A");
        let state = RuViewState::calm_baseline();
        let mut bad = StimulusParameters::prior();
        bad.frequency_hz = 60.0;
        let r = g.run_session(&sim, &latent, &state, &bad, 0);
        assert_eq!(r.err(), Some(GovernanceError::OutsideEnvelope));
        assert!(g.audit_log().is_empty());
    }

    #[test]
    fn withdrawn_consent_blocks_further_sessions() {
        let mut g = governor();
        let sim = ResponseSimulator::new(1);
        let latent = LatentPerson::from_id("subject-A");
        let state = RuViewState::calm_baseline();
        g.run_session(&sim, &latent, &state, &StimulusParameters::prior(), 0)
            .unwrap();
        g.withdraw_consent();
        let r = g.run_session(&sim, &latent, &state, &StimulusParameters::prior(), 1);
        assert_eq!(r.err(), Some(GovernanceError::NoConsent));
    }

    #[test]
    fn calibration_then_recommendation_lands_near_latent_peak() {
        let mut g = governor();
        let sim = ResponseSimulator::new(99);
        let latent = LatentPerson::from_id("subject-peak");
        let state = RuViewState::calm_baseline();
        g.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();
        let rec = g.recommend(&StimulusParameters::prior());
        assert!(g.envelope.contains(&rec.stimulus));
        // Optimizer should prefer a frequency within ±2 Hz of the true peak
        // (calibration is short/noisy; ±2 Hz is a robust bound for the test).
        assert!((rec.stimulus.frequency_hz - latent.peak_hz).abs() <= 2.0);
    }

    #[test]
    fn every_session_is_witnessed_and_logged() {
        let mut g = governor();
        let sim = ResponseSimulator::new(5);
        let latent = LatentPerson::from_id("subject-A");
        let state = RuViewState::calm_baseline();
        g.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();
        assert_eq!(g.audit_log().len(), 9); // 36..44 Hz
        for rec in g.audit_log() {
            assert_eq!(rec.session_hash.len(), 64); // hex SHA-256
        }
    }

    #[test]
    fn sham_mode_suppresses_entrainment() {
        let latent = LatentPerson::from_id("subject-strong");
        let state = RuViewState::calm_baseline();
        let sim = ResponseSimulator::new(11);
        let mut peak = StimulusParameters::prior();
        peak.frequency_hz = (latent.peak_hz * 10.0).round() / 10.0;
        peak.frequency_hz = peak.frequency_hz.clamp(36.0, 44.0);

        let mut open = governor();
        let open_rec = open.run_session(&sim, &latent, &state, &peak, 0).unwrap();

        let mut sham = governor();
        sham.set_mode(TrialMode::Sham);
        let sham_rec = sham.run_session(&sim, &latent, &state, &peak, 0).unwrap();

        let open_g = open_rec.eeg_optional.unwrap().gamma_power_gain;
        let sham_g = sham_rec.eeg_optional.unwrap().gamma_power_gain;
        assert!(sham_g < open_g);
    }

    #[test]
    fn clinician_report_uses_only_allowed_claim() {
        let g = governor();
        assert_eq!(g.clinician_report().claim, PRODUCT_CLAIM);
        assert!(!PRODUCT_CLAIM.to_lowercase().contains("alzheimer"));
        assert!(!PRODUCT_CLAIM.to_lowercase().contains("treat"));
    }
}

# ADR-250: Adaptive Gamma Entrainment Using RuVector and RuView

| Field | Value |
|-------|-------|
| **Status** | Proposed |
| **Date** | 2026-06-09 |
| **Owner** | RuView, RuVector, RuFlo clinical systems |
| **Decision type** | Architecture, safety, research platform |
| **Scope** | Personalized noninvasive sensory stimulation and passive state sensing |
| **Codebase target** | `v2/crates/ruview-gamma` (this ADR's reference implementation) |

> **Not medical advice:** This ADR defines a research and engineering
> architecture. It does **not** define an approved Alzheimer's treatment. The
> immediate product claim is **personalized entrainment optimization**, never
> Alzheimer's treatment.

---

## 1. Context

Recent research suggests that noninvasive 40 Hz multisensory stimulation using
light and sound can influence gamma neural activity and may activate glymphatic
clearance pathways associated with amyloid removal in Alzheimer's mouse models.
The strongest mechanistic support comes from a 2024 Nature paper showing that
multisensory gamma stimulation promoted cerebrospinal fluid influx, interstitial
fluid efflux, aquaporin-4 polarization, meningeal lymphatic dilation, VIP
interneuron signaling, and amyloid clearance in 5XFAD mice. Blocking glymphatic
clearance abolished the amyloid-clearing effect.

Human evidence is promising but still early. A 2022 study in mild probable
Alzheimer's disease reported that 40 Hz sensory stimulation was feasible and
well tolerated, with exploratory signals around brain structure, connectivity,
sleep, and memory. A small 2025 long-term pilot reported daily 40 Hz
audiovisual stimulation over two years was safe and feasible and may slow
cognitive and biomarker progression, but the sample size was very small and not
definitive.

The field mostly treats 40 Hz as a fixed protocol. That is a useful population
prior, but individual brains may differ by baseline gamma state, arousal, sleep
quality, sensory acuity, medication, age, disease stage, and comfort tolerance.
A 2025 PLOS One study re-evaluated gamma stimulation frequency across 36–44 Hz,
supporting the idea that frequency choice should be empirically measured rather
than assumed.

## 2. Problem

Fixed 40 Hz stimulation creates four engineering limits:

1. It assumes the same frequency works for everyone.
2. It does not continuously verify entrainment.
3. It ignores physiological state (sleep, motion, breathing, restlessness, comfort).
4. It cannot safely optimize stimulation parameters over time.

For clinical or wellness-grade deployment, the system must answer: *which
frequency, modality, intensity, timing, and session structure produces the
strongest safe entrainment for this person in this state?*

## 3. Decision

We build an **Adaptive Gamma Entrainment Architecture** where 40 Hz is the
initial prior, not the hard-coded answer. The system uses:

1. **RuView** as the passive state-sensing layer.
2. **RuVector** as the personal response-modeling layer.
3. A **constrained optimizer** to select stimulation parameters.
4. **RuFlo** as the governed workflow, audit, safety, and protocol-execution layer.
5. **Clinical-mode separation** to prevent unsupported therapeutic claims.

The system optimizes stimulation **only within a predefined safety envelope**
and separates entrainment optimization from disease-outcome claims.

## 4. Architecture Overview

```
Person baseline → RuView passive sensing → optional EEG → stimulus session
   → response extraction → RuVector personal response vector
   → constrained optimizer → next best protocol → RuFlo audit + governance
```

| Component | Role | Output |
|-----------|------|--------|
| RuView | Passive sensing of body and environment | breathing, motion, posture, stillness, sleep state, adherence |
| EEG (optional) | Direct entrainment measurement | gamma power, phase locking, artifact score |
| Stimulus controller | Light + sound actuator | frequency, intensity, phase, duty cycle, duration |
| RuVector | Learns personal response surface | individual entrainment vector |
| Optimizer | Selects next safe stimulation setting | recommended protocol |
| RuFlo | Governance and audit | protocol record, safety log, reproducibility trail |

## 5. Stimulus Search Space

| Parameter | Default range | Notes |
|-----------|---------------|-------|
| Frequency | 36–44 Hz | published exploratory range |
| Starting prior | 40 Hz | strongest preclinical literature |
| Modality | audio, visual, combined | combined preferred (GENUS-style) |
| Brightness | bounded low–moderate | avoid unsafe flicker intensity |
| Volume | bounded low–moderate | comfort-constrained |
| Duty cycle | continuous, ramped, pulsed | start conservative |
| Phase | synchronized, offset | explore only after baseline |
| Duration | short calibration first | longer only after tolerance |
| Time of day | morning, evening, quiet wake | state-dependent |

## 6. Personal Response Vector

RuVector represents each person with a compact 20-field adaptive vector
(`baseline_gamma, baseline_alpha, alpha_gamma_ratio, gamma_power_gain,
phase_locking_value, breathing_rate, breathing_stability, motion_artifact,
posture_state, sleep_state, restlessness_score, stimulus_frequency,
brightness_level, sound_level, duty_cycle, phase_offset, session_duration,
comfort_score, adherence_score, adverse_event_flag`), updated after each
session: `R_{t+1} = update(R_t, stimulus_t, response_t, safety_t)`.

## 7. Optimization Objective

The optimizer maximizes **safe, stable entrainment**, not raw gamma power:

```
score =  w1·gamma_power_gain + w2·phase_locking_gain + w3·breathing_stability
       + w4·adherence + w5·comfort
       − w6·motion_artifact − w7·adverse_event_risk − w8·overstimulation_penalty
```

Default weights: gamma 0.30, phase-locking 0.25, comfort 0.15, breathing 0.10,
adherence 0.10, motion penalty 0.05; **safety penalty is a hard constraint, not
negotiable**.

## 8. Learning Method (staged loop)

- **Phase 1 — Conservative calibration:** short sessions at 36–44 Hz (1 Hz steps).
- **Phase 2 — Bayesian optimization:** GP surrogate + Expected Improvement,
  subject to `safety==true ∧ comfort≥threshold ∧ adverse_event_risk≤threshold`.
- **Phase 3 — Contextual bandit:** once enough sessions exist, LinUCB over state
  context → stimulus action → safe-entrainment reward.
- **Phase 4 — Closed-loop control:** mid-session, bounded frequency nudges when
  entrainment drops, intensity reduction on discomfort, scoring pause on motion
  spikes, and hard terminate-and-lock on adverse events.

## 9–12. RuView / RuVector / RuFlo roles & Safety

RuView supplies non-camera passive context (breathing, motion, posture,
stillness, restlessness, sleep proxy, interference, adherence). RuVector
supplies adaptive memory (personal vector, session-to-session learning,
anonymized nearest-neighbor, drift detection, clustering, recommendation, edge
inference) and predicts **safe-entrainment / comfort / artifact / adherence
likelihood** — not Alzheimer's improvement. RuFlo governs (consent,
inclusion/exclusion, scheduling, safety-stop rules, parameter audit trail, ADR
linkage, model-version tracking, clinician export, trial-mode separation).
Every session is reproducible via
`session_hash = hash(protocol_version, model_version, device_version,
stimulus_parameters, sensor_summary, response_summary, safety_events)`.

**Hard-stop conditions:** headache, dizziness, nausea, agitation, visual
discomfort, abnormal distress, seizure-like symptoms, user-stop request, sensor
confidence below threshold, protocol outside approved envelope.

**Exclusion / clinical supervision:** epilepsy or seizure history,
photosensitivity, severe migraine sensitivity, severe psychiatric instability,
implanted neurological devices, significant sensory impairment affecting
protocol validity, medication changes affecting neural response. **The system
must never autonomously expand beyond the allowed safety envelope.**

## 18. Acceptance Criteria

| Criterion | Target |
|-----------|--------|
| Frequency control precision | ±0.1 Hz |
| Session audit completeness | 100% |
| Motion artifact detection | ≥90% valid/invalid classification |
| Adaptive protocol improvement | ≥20% entrainment gain vs fixed 40 Hz |
| Comfort | no worse than fixed 40 Hz |
| Safety stops | 100% logged |
| Repeatability | same optimal band within ±1 Hz across 3 sessions |
| Claim discipline | no disease-treatment claim in product UI |

## 19. Non-Goals

This ADR does **not** claim: RuView treats Alzheimer's; RuVector clears amyloid;
RF sensing measures amyloid directly; personalized frequency improves clinical
outcomes; consumer deployment is safe without screening; 40 Hz is always optimal.

## 21. Implementation Roadmap → reference crate `ruview-gamma`

| Milestone | Module(s) in `ruview-gamma` | Status in this ADR's impl |
|-----------|------------------------------|---------------------------|
| M1 Simulator | `simulator.rs` (deterministic ChaCha20 response surface) | **Implemented** |
| M2 Device harness (contract) | `stimulus.rs`, `safety.rs` (envelope + emergency stop) | **Interfaces + safety implemented** |
| M3 RuView integration (contract) | `response.rs` (`RuViewState`) | **State contract implemented** |
| M4 EEG validation (contract) | `response.rs` (`EegMeasurement`), `objective.rs` | **Optional input implemented** |
| M5 Adaptive optimizer | `optimizer.rs` (Phase 1+2), `bandit.rs` (Phase 3), closed-loop | **Implemented** |
| M6 Trial mode | `ruflo.rs` (consent, inclusion/exclusion, sham, audit, session hash) | **Implemented** |
| §10 RuVector self-learning | `ruvector.rs` (anonymized `ProfileStore`, deterministic kNN, cohort warm-start priors via down-weighted GP pseudo-observations, physiological drift detection, deterministic clustering) | **Implemented** |

The crate is a **deterministic, dependency-light leaf** (no internal RuView
deps, ChaCha20 PRNG, SHA-256 witness — same discipline as `nvsim`), so the
optimizer, safety envelope, and RuVector update logic can be tested and replayed
bit-exactly before any hardware or human exposure. Hardware actuation, real RF
sensing, and real EEG land behind feature flags / external adapters; this crate
implements the governed software core and its proofs.

## 22. Final Decision Statement

We build Adaptive Gamma Entrainment as a governed RuView + RuVector
architecture. The system treats 40 Hz as the evidence-based starting prior, then
learns each person's safe entrainment response curve using passive sensing,
optional EEG, constrained optimization, and auditable RuFlo workflows. The
immediate product claim is **personalized entrainment optimization** — not
Alzheimer's treatment. That distinction keeps the system scientifically
credible, clinically safer, and commercially defensible.

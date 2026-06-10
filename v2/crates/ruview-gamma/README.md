# ruview-gamma — Adaptive Gamma Entrainment (ADR-250)

Governed, deterministic, **safety-constrained** personalization of 40 Hz-prior
multisensory (light + sound) stimulation. Treats 40 Hz as the evidence-based
*starting prior*, then learns each person's safe entrainment response curve using
passive RuView sensing, optional EEG, a constrained optimizer, and auditable
RuFlo workflows.

> **Not medical advice / not a medical device.** This crate is a research and
> engineering platform. The only claim it makes is **"personalized entrainment
> optimization"** (`ruview_gamma::PRODUCT_CLAIM`) — never Alzheimer's treatment,
> amyloid clearance, or any clinical outcome (ADR-250 §19). It performs **no
> hardware actuation**: real stimulus delivery, RF sensing, and EEG arrive
> through external adapters behind feature flags after this governed software
> core ships (ADR-250 §21, Milestones 2–4).

## Why it exists

The field mostly treats 40 Hz as a fixed protocol. But individual brains differ
by baseline gamma, arousal, sleep, sensory acuity, medication, age, and comfort
(the 2025 PLOS One 36–44 Hz re-evaluation). Fixed 40 Hz (1) assumes one
frequency fits all, (2) never verifies entrainment, (3) ignores physiological
state, and (4) cannot safely optimize over time. This crate closes that loop.

## The safety invariant

**No recommendation, calibration step, bandit arm, or closed-loop nudge can ever
emit a `StimulusParameters` outside the `SafetyEnvelope`.** Every emitting path
clamps to the envelope and is asserted against `SafetyEnvelope::contains` in
tests. The optimizer never widens the envelope — only an operator constructs a
wider one deliberately (ADR-250 §12). Non-finite (NaN/∞) inputs clamp toward the
conservative floor, never the cap.

## Module map

| Module | Role (ADR-250 §) | Highlights |
|--------|------------------|------------|
| `stimulus` | §5, §12 | `StimulusParameters`, `SafetyEnvelope` (validate / clamp / grids) |
| `safety` | §12 | exclusion screen, latched `SafetyMonitor`, hard-stop reasons |
| `response` | §6, §9, §10 | `RuViewState`, optional `EegMeasurement`, 20-field `PersonResponseVector` (RuVector memory) with sticky adverse flag |
| `objective` | §7 | safe-entrainment score; safety is a hard gate, not a weight; RF-only proxy when EEG absent |
| `simulator` | §21 M1 | deterministic ChaCha20 `frequency_response_curve(person, state, stimulus)` |
| `optimizer` | §8 | Phase-1 calibration sweep, Phase-2 GP + Expected-Improvement, Phase-4 closed-loop control |
| `bandit` | §8 P3 | LinUCB contextual bandit over envelope-safe arms |
| `ruvector` | §10 items 3–6 | anonymized `ProfileStore` (one-way hashed tags), deterministic kNN, cohort warm-start priors (down-weighted pseudo-observations), `DriftDetector` over the physiological sub-vector, deterministic k-means clustering |
| `session` | §11, §13 | hashable `SessionRecord`, reproducible `session_hash` (SHA-256, quantized canonical form) |
| `ruflo` | §11 | consent → exclusion → envelope → run → monitor → score → update → witnessed audit; trial/sham mode; clinician export; claim discipline |
| `proof` | — | deterministic bundle witness (mirrors `nvsim` / `verify.py`) |
| `math` | — | dependency-light numerics (erf, normal CDF/PDF, Cholesky, RBF) |

## Quick start

```rust
use ruview_gamma::{
    ruflo::{Consent, RufloGovernor},
    response::RuViewState,
    simulator::{LatentPerson, ResponseSimulator},
    stimulus::{SafetyEnvelope, StimulusParameters},
};

let envelope = SafetyEnvelope::conservative();
let mut gov = RufloGovernor::enroll("subject-001", envelope, &[], Consent::Granted)
    .expect("cleared to participate");

// Milestone 1: drive the governed loop with the deterministic simulator.
let sim = ResponseSimulator::new(42);
let latent = LatentPerson::from_id("subject-001");
let state = RuViewState::calm_baseline();
gov.run_calibration(&sim, &latent, &state, 5.0, 0).unwrap();

let rec = gov.recommend(&StimulusParameters::prior());
assert!(envelope.contains(&rec.stimulus)); // always inside the envelope
```

## Test / validate / benchmark

```bash
cargo test  -p ruview-gamma --no-default-features    # 64 unit/integration + 1 doctest
cargo bench -p ruview-gamma --no-default-features     # criterion micro-benchmarks
```

Determinism is proven, not assumed: `proof::Proof::reference_witness()` runs a
fixed reference participant through the full governed pipeline and pins the
bundle SHA-256 (`Proof::EXPECTED_WITNESS`); the test fails on any silent drift in
the optimizer, simulator, response update, or session hashing.

### Measured (this container, indicative — not a regression gate)

| Bench | Median | Note |
|-------|--------|------|
| `gamma_safety_tick` | ~9.3 ns | vs ADR-250 §17 < 500 ms hard-stop latency bound |
| `gamma_bandit_select` | ~74 ns | LinUCB decision |
| `gamma_bayesian_recommend` | ~19 µs | GP + EI over the 0.1 Hz envelope grid (was ~105 µs: the GP is now factorized once per recommend, not once per grid candidate — −81%, bit-identical) |
| `gamma_calibration_sweep` | ~135 µs | full 9-session enroll → simulate → score → update → witness (was ~486 µs, −71%) |
| `gamma_cohort_knn_500` | ~15 µs | exact kNN over 500 anonymized profiles |
| `gamma_cohort_warm_start_500` | ~16 µs | full cohort prior construction (runs once per enrollment) |

## Self-learning across people (ADR-250 §10)

`RufloGovernor::export_anonymized_profile()` publishes a participant's 20-field
vector + per-frequency scores from **safe sessions only** under a one-way hashed
tag; `seed_from_cohort(&store, k)` warm-starts a new person's optimizer from the
k nearest responders as **down-weighted pseudo-observations**
(`observe_prior`, ≥25× the real-observation noise). Priors shape where the
optimizer looks first but never count as measured data — they are excluded from
the EI incumbent, the audit log, and the clinician report. Per-session
`drift_status()` (Welford centroid over the *physiological* sub-vector —
stimulus inputs masked out) flags when recalibration is warranted.

## Roadmap (ADR-250 §21)

M1 simulator ✅ · M2 device harness (envelope + e-stop contract) ✅ · M3 RuView
state contract ✅ · M4 optional EEG input ✅ · M5 adaptive optimizer (BO + bandit
+ closed-loop) ✅ · M6 trial mode (sham/blinding + clinician export) ✅ ·
§10 RuVector self-learning (cohort warm-start, drift detection, clustering) ✅.
Hardware actuation, real RF sensing, and real EEG land behind feature-flagged
adapters. An HNSW backend (the `ruvector` crates) drops in for `ProfileStore`
once cohorts grow past ~10⁵ profiles.

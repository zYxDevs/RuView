//! Criterion benchmarks for the adaptive-gamma governed loop (ADR-250 §17).
//!
//! Measures the latency-sensitive paths: a full calibration sweep, a single
//! Bayesian recommendation, a closed-loop safety tick, and a bandit decision.
//! The safety-stop tick is the figure compared against ADR-250 §17's < 500 ms
//! bound — it is O(1) and lands far below.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use ruview_gamma::bandit::{BanditContext, ContextualBandit};
use ruview_gamma::optimizer::BayesianOptimizer;
use ruview_gamma::response::RuViewState;
use ruview_gamma::ruflo::{Consent, RufloGovernor};
use ruview_gamma::ruvector::{AnonymizedProfile, ProfileStore, VECTOR_DIM};
use ruview_gamma::safety::{SafetyMonitor, SafetyTick};
use ruview_gamma::simulator::{LatentPerson, ResponseSimulator};
use ruview_gamma::stimulus::{SafetyEnvelope, StimulusParameters};

fn bench_calibration(c: &mut Criterion) {
    let env = SafetyEnvelope::conservative();
    let sim = ResponseSimulator::new(42);
    let latent = LatentPerson::from_id("bench-subject");
    let state = RuViewState::calm_baseline();
    c.bench_function("gamma_calibration_sweep", |b| {
        b.iter(|| {
            let mut gov =
                RufloGovernor::enroll("bench-subject", env, &[], Consent::Granted).unwrap();
            gov.run_calibration(black_box(&sim), &latent, &state, 5.0, 0)
                .unwrap();
            black_box(gov.audit_log().len())
        })
    });
}

fn bench_recommend(c: &mut Criterion) {
    let env = SafetyEnvelope::conservative();
    let mut bo = BayesianOptimizer::default();
    for f in env.calibration_frequencies() {
        bo.observe(f, 1.0 - 0.05 * (f - 39.5).powi(2));
    }
    let base = StimulusParameters::prior();
    c.bench_function("gamma_bayesian_recommend", |b| {
        b.iter(|| black_box(bo.recommend(black_box(&env), black_box(&base))))
    });
}

fn bench_safety_tick(c: &mut Criterion) {
    c.bench_function("gamma_safety_tick", |b| {
        b.iter(|| {
            let mut m = SafetyMonitor::default();
            black_box(m.evaluate(black_box(SafetyTick {
                adverse: None,
                sensor_confidence: 0.9,
                stimulus_in_envelope: true,
            })))
        })
    });
}

fn bench_bandit(c: &mut Criterion) {
    let env = SafetyEnvelope::conservative();
    let candidates: Vec<StimulusParameters> = [38.0, 40.0, 42.0]
        .iter()
        .map(|&f| {
            let mut s = StimulusParameters::prior();
            s.frequency_hz = f;
            s
        })
        .collect();
    let bandit = ContextualBandit::new(&env, &candidates, 1.0).unwrap();
    let ctx = BanditContext {
        sleep_quality: 0.7,
        time_of_day: 0.5,
        breathing_state: 0.8,
        motion_state: 0.1,
        fatigue_proxy: 0.2,
        prior_response: 0.6,
    };
    c.bench_function("gamma_bandit_select", |b| {
        b.iter(|| black_box(bandit.select(black_box(&ctx))))
    });
}

fn cohort_store(n: usize) -> ProfileStore {
    let mut store = ProfileStore::new();
    for i in 0..n {
        let mut vector = [0.5; VECTOR_DIM];
        vector[5] = 12.0 + (i % 8) as f64; // breathing_rate spread
        vector[11] = 36.0 + (i % 9) as f64; // frequency spread
        store.upsert(AnonymizedProfile {
            profile_tag: format!("p{i:04}"),
            vector,
            frequency_scores: (36..=44).map(|f| (f as f64, 0.5 + 0.01 * (i % 7) as f64)).collect(),
        });
    }
    store
}

fn bench_cohort_knn(c: &mut Criterion) {
    let store = cohort_store(500);
    let mut q = [0.5; VECTOR_DIM];
    q[5] = 14.0;
    q[11] = 39.0;
    c.bench_function("gamma_cohort_knn_500", |b| {
        b.iter(|| black_box(store.k_nearest(black_box(&q), 5)))
    });
    c.bench_function("gamma_cohort_warm_start_500", |b| {
        b.iter(|| black_box(store.warm_start_prior(black_box(&q), 5, 1e-4)))
    });
}

criterion_group!(
    benches,
    bench_calibration,
    bench_recommend,
    bench_safety_tick,
    bench_bandit,
    bench_cohort_knn
);
criterion_main!(benches);

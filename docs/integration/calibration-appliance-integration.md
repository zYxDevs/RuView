# Per-Room Calibration — Integration Overview (for `cognitum-one/v0-appliance`)

**Audience:** integrators wiring the RuView per-room calibration system (ADR-151) into the
Cognitum V0 appliance (`cognitum-v0`, Pi 5 + Hailo). This document is the contract +
deployment spec: data formats, API surface, crate API, and the appliance integration plan.

**Source of truth:** crate `v2/crates/wifi-densepose-calibration` + CLI `v2/crates/wifi-densepose-cli`
(`calibrate`, `calibrate-serve`, `enroll`, `train-room`, `room-status`, `room-watch`) on this PR's branch.

---

## 1. What it is

"Teach the room before you teach the model." A local-first pipeline that turns a few minutes of
clean human anchors — layered on an empty-room baseline — into a versioned **bank of small,
room-calibrated specialists** for presence, posture, breathing, heartbeat, restlessness, and anomaly.

```
baseline (ADR-135)  →  enroll (anchors + quality gate)  →  extract (features)  →  train (specialist bank)  →  runtime (mixture + veto)
   environmental         stand/sit/lie/breathe/move        periodicity/variance     6 small models             RoomState per window
   fingerprint           (re-prompts bad captures)                                  + STALE invalidation       (+ multistatic fusion)
```

**Design invariants (carry these into the appliance):**
- **Specialisation over scale** — six tiny models (threshold / nearest-prototype / autocorrelation), not one big model. They run in microseconds on a Pi CPU; **they do not need the Hailo HAT**.
- **Local-first** — baselines + per-room banks stay on the device. Cross-room sharing is *model deltas* (federation, ADR-105), **never raw CSI**.
- **Honest degradation** — baseline drift marks a bank `STALE`; a physically-implausible window is vetoed rather than emitting a hallucinated reading.

---

## 2. Tiering on the Pi 5 + Hailo (what runs where)

| Tier | Runs on | What | Status |
|------|---------|------|--------|
| **CSI source** | ESP32-S3/C6 nodes (`edge_tier=0` raw CSI) | `0xC5110001` frames over UDP | shipping (v0.7.1-esp32) |
| **Calibration service** | **Pi 5 CPU** (aarch64) | this crate: baseline/enroll/train/runtime + HTTP API | **this PR** |
| **Shared backbone (optional)** | **Hailo HAT (HAILO10H)** | ADR-150 RF Foundation Encoder + neural pose head as HEF | future (ADR-150) |

> The appliance's WiFi (`wlan0`) is `managed` with no nexmon — **the Pi is a CSI *processor*, not a CSI radio.** CSI arrives from the ESP32 nodes (the existing `ruview-vitals-worker:50054` already receives it). Calibration *consumes* that stream; it does not sense directly.

---

## 3. Data contracts (the integration surface)

### 3.1 CSI ingest — ESP32 `0xC5110001` (UDP, little-endian)

```
Offset  Size  Field
 0      4     magic = 0xC511_0001 (LE u32)
 4      1     node_id (u8)            ← group multistatic nodes by this
 5      1     n_antennas (u8)
 6      1     n_subcarriers (u8)      ← 52/64 (HT20), 114 (HT40), 242 (HE20)
 7      1     reserved
 8      2     freq_mhz (LE u16)
10      4     sequence (LE u32)
14      1     rssi (i8)
15      1     noise_floor (i8)
16      4     reserved
20      2·n_antennas·n_subcarriers   IQ pairs: i (i8), q (i8)
```
Parser reference: `wifi-densepose-cli/src/calibrate.rs::parse_csi_packet`. The appliance can reuse the
ESP32 stream the vitals worker already receives, or tee it to the calibration UDP port.

### 3.2 Baseline (ADR-135) — binary, magic `0xCA1B_0001`

```
Header (16 B LE): magic(4)=0xCA1B0001, version(1)=1, tier(1) {0=HT20,1=HT40,2=HE20,3=HE40},
                  reserved(2), captured_at_unix_s(8, i64)
Body:             frame_count(8,u64), num_subcarriers(4,u32),
                  per subcarrier: amp_mean(f32), amp_variance(f32), phase_mean(f32), phase_dispersion(f32)
```
Produced by `calibrate` / `calibrate-serve`; `BaselineCalibration::{to_bytes,from_bytes}`. A baseline's
UUID (`calibration_uuid()`) is the `baseline_id` referenced by enrollments and banks for STALE checks.

### 3.3 Enrollment output — JSON (`enroll` → `train-room`)

```jsonc
{
  "room_id": "living-room",
  "baseline_id": "<uuid>",
  "fs_hz": 15.0,
  "anchors": [
    { "room_id": "living-room", "label": "stand_still",
      "features": { "mean": f32, "variance": f32, "motion": f32,
                    "breathing_score": f32, "breathing_hz": f32,
                    "heart_score": f32, "heart_hz": f32 } }
  ],
  "session": { "room_id": "...", "baseline_id": "...", "events": [ /* event-sourced audit log */ ] }
}
```
Anchor labels (fixed sequence, **JSON wire = snake_case**, test-enforced): `empty, stand_still, sit, lie_down, breathe_slow, breathe_normal, small_move, sleep_posture`.

### 3.4 Specialist bank — JSON (`train-room` → `room-watch` / runtime)

```jsonc
{
  "room_id": "living-room",
  "baseline_id": "<uuid>",            // drift vs current → STALE
  "trained_at_unix_s": 0,
  "anchor_count": 6,
  "presence":     { "threshold": f32, "occupied_var": f32 } | null,
  "posture":      { "prototypes": [ ["Standing", [f32;5]], ... ] } | null,
  "breathing":    { "min_score": f32 },
  "heartbeat":    { "min_score": f32 },
  "restlessness": { "calm_motion": f32, "active_motion": f32 } | null,
  "anomaly":      { "prototypes": [ [f32;5], ... ], "scale": f32 } | null
}
```
`SpecialistBank::{to_json,from_json}`. A *partial* bank is valid (missing-anchor specialists are `null`).

### 3.5 Runtime output — `RoomState` JSON (per window)

```jsonc
{
  "presence":     { "kind":"Presence", "value":0|1, "confidence":f32, "label":"present|absent" } | null,
  "posture":      { "kind":"Posture", "value":f32, "confidence":f32, "label":"standing|sitting|lying" } | null,
  "breathing":    { "kind":"Breathing", "value": <BPM>, "confidence":f32, "label":null } | null,
  "heartbeat":    { "kind":"Heartbeat", "value": <BPM>, "confidence":f32, "label":null } | null,
  "restlessness": { "kind":"Restlessness", "value": 0.0..1.0, "confidence":f32 } | null,
  "anomaly":      { "kind":"Anomaly", "value": 0.0..1.0, "confidence":f32, "label":"normal|anomalous" } | null,
  "vetoed": bool,   // anomaly veto fired → vitals/posture suppressed
  "stale":  bool    // bank trained against a different baseline
}
```

---

## 4. HTTP API — `calibrate-serve` (CORS-enabled; this is what a UI/appliance drives)

| Method | Path | Body / returns |
|--------|------|----------------|
| GET | `/api/v1/calibration/health` | `{ udp_port, frames_seen, last_frame_age_ms, streaming, default_tier, output_dir, session_active }` |
| POST | `/api/v1/calibration/start` | `{ tier?, duration_s?, room_id?, min_frames? }` → `202` session snapshot |
| GET | `/api/v1/calibration/status` | live `{ state, frames_recorded, target_frames, progress, z_median, eta_s, ... }` |
| POST | `/api/v1/calibration/stop` | finalize early → result summary |
| GET | `/api/v1/calibration/result` | last finalized baseline summary |
| GET | `/api/v1/calibration/baselines` | list persisted `.bin` baselines |
| GET | `/api/v1/room/state?bank=<name>` | **live RoomState** (mixture-of-specialists over the CSI window; bank resolved as a sanitized name under `output_dir`) |
| POST | `/api/v1/room/train` | `{ room_id, baseline_id, anchors[]? }` → train + persist a specialist bank as `<output_dir>/<room_id>.json` (anchors[] optional if enrolled via `/enroll/anchor`; read back via `/room/state?bank=<room_id>`) |
| POST | `/api/v1/enroll/anchor` | `{ room_id, baseline, label, duration_s? }` → capture one guided anchor against a baseline (blocks for the capture); returns the gate verdict + progress |
| GET | `/api/v1/enroll/status?room=<id>` | enrollment progress (accepted anchors, next, complete) |

A single background task owns the UDP socket + recorder (handlers talk to it over an mpsc channel +
shared status snapshot), so the API is non-blocking. **The full pipeline is now drivable over HTTP** — baseline (`start`/`stop`) → `enroll/anchor` (×8) → `room/train` → `room/state` — so the appliance UI needs no CLI. (The CLI `enroll`/`train-room`/`room-watch` remain for scripted/headless use.)

---

## 5. Public crate API (`wifi-densepose-calibration`)

```rust
// Stage 2 — enrollment
anchor::{AnchorLabel, Anchor, AnchorQuality, EnrollmentEvent, EnrollmentSession, Posture}
enrollment::{AnchorQualityGate, AnchorRecorder}
// Stage 3 — features
extract::{Features, AnchorFeature, autocorr_dominant}
// Stage 4 — specialists + bank
specialist::{Specialist, SpecialistKind, SpecialistReading,
             PresenceSpecialist, PostureSpecialist, BreathingSpecialist,
             HeartbeatSpecialist, RestlessnessSpecialist, AnomalySpecialist}
bank::SpecialistBank
// Stage 5 — runtime
runtime::{MixtureOfSpecialists, RoomState}
multistatic::MultiNodeMixture            // fuse co-located nodes (ADR-029)
```
Pure Rust; deps are `wifi-densepose-core` + `wifi-densepose-signal` (default-features off) + serde/uuid.
**No GPU / no system BLAS** in the calibration path → builds cleanly on aarch64.

---

## 6. Appliance integration plan (`cognitum-one/v0-appliance`)

Verified on `cognitum-v0`: aarch64, `cargo 1.96.0`, Hailo `HAILO10H`, `ruview-vitals-worker:50054`.

**Step 1 — vendor / depend on the crate.** Add `wifi-densepose-calibration` (path or published crate)
to the appliance workspace. It builds natively on aarch64 — no BLAS/GPU, **and no ONNX/OpenSSL**:
the CLI's `mat`→`nn`→`ort`(ONNX)→`openssl-sys` chain is now feature-gated out of the calibration build.

```bash
# Pi/appliance calibration binary — cross-compiles clean (no ort/openssl):
cargo build -p wifi-densepose-cli --no-default-features --release
#   (omit `--no-default-features` only if you also need the MAT subcommands)
```
Verified: `cargo tree -p wifi-densepose-cli --no-default-features` shows **0** `ort`/`openssl-sys` deps;
`cross test --target aarch64-unknown-linux-gnu` passes the calibration suite under qemu.

**Step 2 — wire the CSI source.** Two options:
  - (a) Tee the ESP32 UDP stream the vitals worker already receives into the calibration ingest, or
  - (b) point ESP32 nodes (`edge_tier=0`) at the appliance's calibration UDP port directly.
  Reuse `parse_csi_packet` (or the rvCSI `CsiFrame` schema if you normalise upstream).

**Step 3 — run the calibration service.** Either embed the crate (call `CalibrationRecorder` /
`MixtureOfSpecialists` in-process from a worker like `ruview-vitals-worker`), or run the
`calibrate-serve` binary as a sidecar (systemd unit, bind `127.0.0.1` + reverse-proxy through the
appliance gateway on `:9000`). Persist baselines/banks under the appliance data dir, keyed by `room_id`.

**Step 4 — expose to the dashboard.** Surface the `/api/v1/calibration/*` endpoints (and add
`enroll`/`train`/`room-state` endpoints — small additive work) behind the appliance's bearer-token
auth + the existing `Seeds`/`Edge` nav. `RoomState` (§3.5) is the live readout payload.

**Step 5 — (optional) Hailo backbone tier.** Compile the ADR-150 RF Foundation Encoder + neural pose
head to Hailo HEF, serve via `ruvector-hailo-worker:50051`; the small specialists become heads over its
embedding. This is the ADR-150 follow-on — *not required* for the calibration service to run.

**Privacy / security:** keep baselines + banks local; if federating across appliances (ADR-105),
exchange bank/model deltas, never raw CSI. Hardening already in place:
- **`--token <T>`** (or `CALIBRATE_TOKEN` env) requires `Authorization: Bearer <T>` on every route; the
  server warns loudly if bound to a non-loopback address without a token.
- **`room_id` is sanitized** to `[A-Za-z0-9_-]` (≤64 chars) before it touches the baseline write path —
  no `../` / absolute-path traversal.
- CORS is permissive for dev — in production bind to loopback and reverse-proxy through the appliance
  gateway (which already enforces bearer auth).

---

## 7. Status & validation

- **Implemented:** all 5 stages + multistatic fusion; CLI + Stage-1 HTTP API (auth + path-traversal hardened). **55 tests** (35 calibration unit + 1 full-loop integration + 19 CLI), all passing under qemu-aarch64.

**Precise validation matrix (don't overstate this — no clean full calibration has run on-target yet):**

| Stage | Pi-5 (real nexmon→`0xC5110001`, 6,813 frames) | ESP32-S3 (COM8, `edge_tier=0`) | qemu / unit / integration |
|---|---|---|---|
| baseline capture + HTTP API + **auth gate** | ✅ | ✅ (120-frame) | full-loop ✅ |
| **clean** empty-room baseline | ❌ `motion_flagged` (artifact) | ❌ (occupied) | full-loop ✅ (synthetic, zero motion flags) |
| enroll → train-room | ❌ | ❌ (needs operator poses) | full-loop ✅ (8/8 anchors, 6 specialists, JSON round-trip) |
| runtime infer | ❌ on-target | ◐ single-node breathing ~16–31 BPM via the **stateless** head (not a trained bank) + node-id fusion | full-loop ✅ (trained bank: 18±2 BPM positive, absent negative, foreign-baseline STALE) |

The complete `baseline → enroll → train-room → infer` loop is now **proven in-process** on deterministic synthetic CSI (`wifi-densepose-calibration/tests/full_loop.rs` — drives the CLI's exact stage order through the public API, seed-robust across 5 seeds, runs with and without default features). Capture + API + auth are proven on real CSI (both boxes). What remains is strictly the **on-target** run: real CSI, a physically empty room for baseline, and an operator performing the 8 guided anchors — that hardware session is the last open item.

- **Known follow-ups (appliance backlog):** `--source-format adr018v6` to drive calibration from the Pi's own nexmon (no ESP32/transcoder); the on-target clean-room enroll→train→infer session (above); phase-based (vs mean-amplitude) breathing carrier; RVF/HNSW persistence (currently JSON); enroll/train HTTP endpoints (live `/room/state` already added); ADR-150 Hailo backbone; true 2-node multistatic; ADR-105 federation.
- **Behavioral findings from the full-loop test (pre-hardware-session fixes worth considering):** (1) *z-band squeeze*: `BaselineCalibration::deviation` flags motion at `amplitude_z_median > 2.0` while the still-anchor gate needs `presence_z ≥ 1.5` — a strongly-reflecting still person can be rejected as "moving"; presence strength and motion are conflated. Most likely on-hardware enroll failure mode. (2) `PresenceSpecialist` is variance-only — a motionless person raises the scalar *mean* but not variance, so a quiet subject can read "absent" at runtime even though enroll accepted them; adding mean/`presence_z` to the presence decision would close it. (3) `Features::from_series` emits a best-in-band `breathing_hz`/`heart_hz` even at negligible score, injecting random in-band frequencies into the prototype embeddings for noise windows; gating the hz fields on score would tighten posture/anomaly classification.

**Reference:** ADR-151 (`docs/adr/ADR-151-room-calibration-specialist-training.md`), ADR-135 (baseline),
ADR-029 (multistatic), ADR-150 (RF Foundation Encoder), ADR-105 (federation), ADR-147 (OccWorld/Hailo).

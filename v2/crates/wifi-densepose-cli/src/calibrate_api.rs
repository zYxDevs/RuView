//! `wifi-densepose calibrate-serve` — HTTP API around ADR-135 baseline calibration.
//!
//! Wraps the same [`wifi_densepose_signal::CalibrationRecorder`] used by the
//! `calibrate` subcommand in a small Axum server so a UI (or any client) can
//! drive an empty-room baseline capture remotely:
//!
//! | Method | Path                              | Purpose                                   |
//! |--------|-----------------------------------|-------------------------------------------|
//! | GET    | `/`                               | API descriptor (discovery)                |
//! | GET    | `/api/v1/calibration/health`      | liveness + UDP ingest stats               |
//! | POST   | `/api/v1/calibration/start`       | begin a baseline capture session          |
//! | GET    | `/api/v1/calibration/status`      | live session progress (poll this for UI)  |
//! | POST   | `/api/v1/calibration/stop`        | finalize the current session early        |
//! | GET    | `/api/v1/calibration/result`      | summary of the last finalized baseline    |
//! | GET    | `/api/v1/calibration/baselines`   | list persisted baseline files             |
//!
//! A single background task owns the UDP socket (ESP32 `0xC511_0001` frames) and
//! the optional active recorder; the HTTP handlers communicate with it over an
//! mpsc command channel and read a shared status snapshot. This keeps the
//! `&mut` recorder lock-free and the API non-blocking. CORS is permissive so a
//! browser UI served from any origin can call it during development.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, RwLock};
use tower_http::cors::CorsLayer;
use wifi_densepose_signal::{BaselineCalibration, CalibrationRecorder};

use crate::calibrate::{parse_csi_packet, tier_config};

const RECV_BUF: usize = 2048;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

/// Arguments for the `calibrate-serve` subcommand.
#[derive(Args, Debug, Clone)]
pub struct CalibrateServeArgs {
    /// TCP port for the HTTP API.
    #[arg(long, default_value_t = 8090)]
    pub http_port: u16,

    /// Bind address for the HTTP API. Default 127.0.0.1 (localhost only);
    /// use 0.0.0.0 to expose the API to the LAN for a remote UI.
    #[arg(long, default_value = "127.0.0.1")]
    pub http_bind: String,

    /// UDP port to receive CSI frames from the ESP32 (must match provisioned target-port).
    #[arg(long, default_value_t = 5005)]
    pub udp_port: u16,

    /// Bind address for the UDP CSI socket.
    #[arg(long, default_value = "0.0.0.0")]
    pub udp_bind: String,

    /// Default PHY tier when a start request omits one (ht20 / ht40 / he20 / he40).
    #[arg(long, default_value = "ht20")]
    pub tier: String,

    /// Directory where finalized baseline `.bin` files are written.
    #[arg(long, default_value = "./baselines")]
    pub output_dir: String,
}

// ---------------------------------------------------------------------------
// Wire types (request / response bodies)
// ---------------------------------------------------------------------------

/// Body for `POST /start`. All fields optional — sensible defaults applied.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct StartParams {
    /// PHY tier override (falls back to the server default).
    pub tier: Option<String>,
    /// Capture duration in seconds (also bounded by the tier's min-frame target).
    pub duration_s: u32,
    /// Optional room label, used in the persisted filename and status.
    pub room_id: Option<String>,
    /// Override the tier's minimum frame count (0 = use tier default).
    pub min_frames: u32,
}

impl Default for StartParams {
    fn default() -> Self {
        Self { tier: None, duration_s: 30, room_id: None, min_frames: 0 }
    }
}

/// Live per-session status snapshot returned by `GET /status`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    /// `recording` | `finalizing` | `complete` | `aborted`.
    pub state: String,
    pub room_id: String,
    pub tier: String,
    pub frames_recorded: usize,
    pub target_frames: usize,
    /// 0.0..=1.0 capture progress.
    pub progress: f32,
    pub z_median: f32,
    pub z_max: f32,
    pub motion_flagged: bool,
    pub elapsed_s: f32,
    pub eta_s: f32,
    /// Optional human-readable note (e.g. abort reason).
    pub note: Option<String>,
}

/// Summary of a finalized baseline, returned by `GET /result` and `POST /stop`.
#[derive(Debug, Clone, Serialize)]
pub struct ResultSummary {
    pub calibration_id: String,
    pub room_id: String,
    pub tier: String,
    pub frame_count: u64,
    pub subcarriers: usize,
    pub captured_at_unix_s: i64,
    pub amp_mean_avg: f32,
    pub amp_variance_avg: f32,
    pub phase_dispersion_avg: f32,
    pub output_path: String,
    pub saved_bytes: usize,
}

/// Shared status the HTTP handlers read.
#[derive(Default)]
struct SharedStatus {
    udp_port: u16,
    default_tier: String,
    output_dir: String,
    frames_seen: u64,
    last_frame_unix_ms: u64,
    session: Option<SessionStatus>,
    last_result: Option<ResultSummary>,
}

/// Commands sent from HTTP handlers to the ingest task.
enum CalCommand {
    Start { params: StartParams, reply: oneshot::Sender<Result<SessionStatus, String>> },
    Stop { reply: oneshot::Sender<Result<ResultSummary, String>> },
}

#[derive(Clone)]
struct ApiState {
    cmd_tx: mpsc::Sender<CalCommand>,
    status: Arc<RwLock<SharedStatus>>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the calibration HTTP API server (blocks until Ctrl-C).
pub async fn execute(args: CalibrateServeArgs) -> Result<()> {
    std::fs::create_dir_all(&args.output_dir)
        .map_err(|e| anyhow::anyhow!("cannot create output dir {}: {e}", args.output_dir))?;

    let udp_addr = format!("{}:{}", args.udp_bind, args.udp_port);
    let socket = UdpSocket::bind(&udp_addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind UDP socket on {udp_addr}: {e}"))?;
    eprintln!("[calibrate-serve] CSI ingest on udp://{udp_addr}");

    let status = Arc::new(RwLock::new(SharedStatus {
        udp_port: args.udp_port,
        default_tier: args.tier.clone(),
        output_dir: args.output_dir.clone(),
        ..Default::default()
    }));

    let (cmd_tx, cmd_rx) = mpsc::channel::<CalCommand>(8);

    // Background ingest task owns the socket + recorder.
    {
        let status = status.clone();
        let default_tier = args.tier.clone();
        let output_dir = args.output_dir.clone();
        tokio::spawn(async move {
            ingest_loop(socket, cmd_rx, status, default_tier, output_dir).await;
        });
    }

    let state = ApiState { cmd_tx, status };
    let app = Router::new()
        .route("/", get(descriptor))
        .route("/api/v1/calibration/health", get(health))
        .route("/api/v1/calibration/start", post(start))
        .route("/api/v1/calibration/status", get(status_handler))
        .route("/api/v1/calibration/stop", post(stop))
        .route("/api/v1/calibration/result", get(result))
        .route("/api/v1/calibration/baselines", get(baselines))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let http_addr = format!("{}:{}", args.http_bind, args.http_port);
    let listener = tokio::net::TcpListener::bind(&http_addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind HTTP listener on {http_addr}: {e}"))?;
    eprintln!("[calibrate-serve] HTTP API on http://{http_addr}  (GET / for the route list)");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ingest task — owns the UDP socket and the optional active recorder
// ---------------------------------------------------------------------------

struct ActiveSession {
    recorder: CalibrationRecorder,
    room_id: String,
    tier: String,
    started: Instant,
    deadline: Instant,
    target_frames: usize,
    z_median: f32,
    z_max: f32,
    motion_flagged: bool,
}

async fn ingest_loop(
    socket: UdpSocket,
    mut cmd_rx: mpsc::Receiver<CalCommand>,
    status: Arc<RwLock<SharedStatus>>,
    default_tier: String,
    output_dir: String,
) {
    let mut buf = vec![0u8; RECV_BUF];
    let mut active: Option<ActiveSession> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        tokio::select! {
            // --- incoming command ---
            Some(cmd) = cmd_rx.recv() => match cmd {
                CalCommand::Start { params, reply } => {
                    if active.is_some() {
                        let _ = reply.send(Err("a calibration session is already running".into()));
                        continue;
                    }
                    let tier = params.tier.unwrap_or_else(|| default_tier.clone());
                    if !["ht20", "ht40", "he20", "he40"].contains(&tier.to_ascii_lowercase().as_str()) {
                        let _ = reply.send(Err(format!("invalid tier {tier:?}")));
                        continue;
                    }
                    let mut config = tier_config(&tier);
                    if params.min_frames > 0 {
                        config.min_frames = params.min_frames;
                    }
                    let target_frames = config.min_frames as usize;
                    let dur = params.duration_s.max(1) as u64;
                    let room_id = params.room_id.unwrap_or_else(|| "default".into());
                    let sess = ActiveSession {
                        recorder: CalibrationRecorder::new(config),
                        room_id: room_id.clone(),
                        tier: tier.clone(),
                        started: Instant::now(),
                        deadline: Instant::now() + Duration::from_secs(dur),
                        target_frames,
                        z_median: 0.0,
                        z_max: 0.0,
                        motion_flagged: false,
                    };
                    let snap = session_snapshot(&sess, "recording", None);
                    active = Some(sess);
                    {
                        let mut s = status.write().await;
                        s.session = Some(snap.clone());
                        s.last_result = None;
                    }
                    eprintln!("[calibrate-serve] session start room={room_id} tier={tier} target={target_frames}");
                    let _ = reply.send(Ok(snap));
                }
                CalCommand::Stop { reply } => {
                    match active.take() {
                        Some(sess) => {
                            let res = finalize(sess, &output_dir, &status).await;
                            let _ = reply.send(res);
                        }
                        None => { let _ = reply.send(Err("no active calibration session".into())); }
                    }
                }
            },

            // --- incoming CSI frame ---
            Ok(n) = socket.recv(&mut buf) => {
                let now_ms = unix_ms();
                {
                    let mut s = status.write().await;
                    s.frames_seen += 1;
                    s.last_frame_unix_ms = now_ms;
                }
                if let Some(sess) = active.as_mut() {
                    let tier = sess.tier.clone();
                    if let Some(frame) = parse_csi_packet(&buf[..n], &tier) {
                        if let Ok(score) = sess.recorder.record(&frame) {
                            sess.z_median = score.amplitude_z_median;
                            sess.z_max = score.amplitude_z_max;
                            sess.motion_flagged = score.motion_flagged;
                        }
                        let frames = sess.recorder.frames_recorded() as usize;
                        let snap = session_snapshot(sess, "recording", None);
                        status.write().await.session = Some(snap);
                        if frames >= sess.target_frames {
                            if let Some(done) = active.take() {
                                let _ = finalize(done, &output_dir, &status).await;
                            }
                        }
                    }
                }
            },

            // --- periodic deadline check (fires even if frames stop arriving) ---
            _ = tick.tick() => {
                if let Some(sess) = active.as_ref() {
                    if Instant::now() >= sess.deadline {
                        let frames = sess.recorder.frames_recorded() as usize;
                        if frames >= 10 {
                            if let Some(done) = active.take() {
                                let _ = finalize(done, &output_dir, &status).await;
                            }
                        } else if let Some(mut done) = active.take() {
                            // not enough frames — abort honestly rather than emit a bad baseline
                            done.motion_flagged = false;
                            let note = format!(
                                "aborted: only {frames} frames in the time window (need >=10) — \
                                 is the ESP32 streaming to udp:{}? ",
                                status.read().await.udp_port
                            );
                            let snap = session_snapshot(&done, "aborted", Some(note.clone()));
                            status.write().await.session = Some(snap);
                            eprintln!("[calibrate-serve] {note}");
                        }
                    }
                }
            },
        }
    }
}

/// Finalize a session: persist the baseline and publish the result summary.
async fn finalize(
    sess: ActiveSession,
    output_dir: &str,
    status: &Arc<RwLock<SharedStatus>>,
) -> Result<ResultSummary, String> {
    let room_id = sess.room_id.clone();
    let tier = sess.tier.clone();
    // mark finalizing
    {
        let snap = session_snapshot(&sess, "finalizing", None);
        status.write().await.session = Some(snap);
    }

    let baseline: BaselineCalibration = sess
        .recorder
        .finalize()
        .map_err(|e| format!("finalize failed: {e}"))?;

    let (amp_mean_avg, amp_var_avg, disp_avg) = baseline_averages(&baseline);
    let uuid = baseline.calibration_uuid().to_string();
    let path = format!("{output_dir}/{room_id}-{uuid}.bin");
    let bytes = baseline.to_bytes();
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;

    let summary = ResultSummary {
        calibration_id: uuid,
        room_id: room_id.clone(),
        tier,
        frame_count: baseline.frame_count,
        subcarriers: baseline.subcarriers.len(),
        captured_at_unix_s: baseline.captured_at_unix_s,
        amp_mean_avg,
        amp_variance_avg: amp_var_avg,
        phase_dispersion_avg: disp_avg,
        output_path: path.clone(),
        saved_bytes: bytes.len(),
    };

    {
        let mut s = status.write().await;
        // reflect completion in the session snapshot, then store the result
        if let Some(sess_status) = s.session.as_mut() {
            sess_status.state = "complete".into();
            sess_status.progress = 1.0;
        }
        s.last_result = Some(summary.clone());
    }
    eprintln!(
        "[calibrate-serve] session complete room={room_id} frames={} -> {path} ({} bytes)",
        summary.frame_count, summary.saved_bytes
    );
    Ok(summary)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn descriptor() -> impl IntoResponse {
    Json(serde_json::json!({
        "service": "wifi-densepose calibration API",
        "adr": "ADR-135 (baseline) / ADR-151 (room calibration & training)",
        "endpoints": {
            "GET  /api/v1/calibration/health": "liveness + UDP ingest stats",
            "POST /api/v1/calibration/start": "{ tier?, duration_s?, room_id?, min_frames? }",
            "GET  /api/v1/calibration/status": "live session progress (poll for UI)",
            "POST /api/v1/calibration/stop": "finalize current session early",
            "GET  /api/v1/calibration/result": "last finalized baseline summary",
            "GET  /api/v1/calibration/baselines": "list persisted baseline files"
        }
    }))
}

async fn health(State(st): State<ApiState>) -> impl IntoResponse {
    let s = st.status.read().await;
    let age = if s.last_frame_unix_ms == 0 { None } else { Some(unix_ms().saturating_sub(s.last_frame_unix_ms)) };
    Json(serde_json::json!({
        "status": "ok",
        "udp_port": s.udp_port,
        "frames_seen": s.frames_seen,
        "last_frame_age_ms": age,
        "streaming": age.map(|a| a < 2000).unwrap_or(false),
        "default_tier": s.default_tier,
        "output_dir": s.output_dir,
        "session_active": s.session.as_ref().map(|x| x.state == "recording").unwrap_or(false),
    }))
}

async fn start(State(st): State<ApiState>, Json(params): Json<StartParams>) -> impl IntoResponse {
    let (tx, rx) = oneshot::channel();
    if st.cmd_tx.send(CalCommand::Start { params, reply: tx }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"ingest task unavailable"}))).into_response();
    }
    match rx.await {
        Ok(Ok(snap)) => (StatusCode::ACCEPTED, Json(serde_json::to_value(snap).unwrap())).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"no reply"}))).into_response(),
    }
}

async fn status_handler(State(st): State<ApiState>) -> impl IntoResponse {
    let s = st.status.read().await;
    match &s.session {
        Some(sess) => (StatusCode::OK, Json(serde_json::to_value(sess).unwrap())).into_response(),
        None => (StatusCode::OK, Json(serde_json::json!({"state":"idle"}))).into_response(),
    }
}

async fn stop(State(st): State<ApiState>) -> impl IntoResponse {
    let (tx, rx) = oneshot::channel();
    if st.cmd_tx.send(CalCommand::Stop { reply: tx }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"ingest task unavailable"}))).into_response();
    }
    match rx.await {
        Ok(Ok(summary)) => (StatusCode::OK, Json(serde_json::to_value(summary).unwrap())).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"no reply"}))).into_response(),
    }
}

async fn result(State(st): State<ApiState>) -> impl IntoResponse {
    let s = st.status.read().await;
    match &s.last_result {
        Some(r) => (StatusCode::OK, Json(serde_json::to_value(r).unwrap())).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error":"no finalized baseline yet"}))).into_response(),
    }
}

async fn baselines(State(st): State<ApiState>) -> impl IntoResponse {
    let dir = { st.status.read().await.output_dir.clone() };
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("bin") {
                let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(serde_json::json!({
                    "file": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    "path": path.to_string_lossy(),
                    "bytes": bytes,
                }));
            }
        }
    }
    Json(serde_json::json!({ "dir": dir, "baselines": out }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn session_snapshot(sess: &ActiveSession, state: &str, note: Option<String>) -> SessionStatus {
    let frames = sess.recorder.frames_recorded() as usize;
    let progress = if sess.target_frames == 0 {
        0.0
    } else {
        (frames as f32 / sess.target_frames as f32).clamp(0.0, 1.0)
    };
    let elapsed = sess.started.elapsed().as_secs_f32();
    let eta = if frames == 0 {
        sess.deadline.saturating_duration_since(Instant::now()).as_secs_f32()
    } else {
        let per = elapsed / frames as f32;
        (per * (sess.target_frames.saturating_sub(frames)) as f32).max(0.0)
    };
    SessionStatus {
        state: state.into(),
        room_id: sess.room_id.clone(),
        tier: sess.tier.clone(),
        frames_recorded: frames,
        target_frames: sess.target_frames,
        progress,
        z_median: sess.z_median,
        z_max: sess.z_max,
        motion_flagged: sess.motion_flagged,
        elapsed_s: elapsed,
        eta_s: eta,
        note,
    }
}

fn baseline_averages(b: &BaselineCalibration) -> (f32, f32, f32) {
    let n = b.subcarriers.len().max(1) as f32;
    let mut amp = 0.0f32;
    let mut var = 0.0f32;
    let mut disp = 0.0f32;
    for s in &b.subcarriers {
        amp += s.amp_mean;
        var += s.amp_variance;
        disp += s.phase_dispersion;
    }
    (amp / n, var / n, disp / n)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_params_defaults() {
        let p = StartParams::default();
        assert_eq!(p.duration_s, 30);
        assert_eq!(p.min_frames, 0);
        assert!(p.tier.is_none());
    }

    #[test]
    fn start_params_partial_json() {
        let p: StartParams = serde_json::from_str(r#"{"room_id":"living-room","tier":"he20"}"#).unwrap();
        assert_eq!(p.room_id.as_deref(), Some("living-room"));
        assert_eq!(p.tier.as_deref(), Some("he20"));
        assert_eq!(p.duration_s, 30); // default applied
    }

    #[test]
    fn args_defaults() {
        let a = CalibrateServeArgs {
            http_port: 8090,
            http_bind: "127.0.0.1".into(),
            udp_port: 5005,
            udp_bind: "0.0.0.0".into(),
            tier: "ht20".into(),
            output_dir: "./baselines".into(),
        };
        assert_eq!(a.http_port, 8090);
        assert_eq!(a.udp_port, 5005);
    }
}

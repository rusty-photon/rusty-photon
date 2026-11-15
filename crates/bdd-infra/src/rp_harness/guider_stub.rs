//! In-process double of the guider's HTTP contract.
//!
//! `rp` reaches the guider over the HTTP API of
//! `docs/services/guider.md` § HTTP API. This double answers that API
//! from a small state machine, so a scenario runs with no guider
//! process, camera or mount behind it. A scenario scripts it through
//! [`GuiderStub`]'s methods: the state it starts in, what it reports of
//! its devices, an error for one endpoint, a stop that times out, a run
//! that stops by itself, a star that is lost, a guider that stops
//! answering or restarts.
//!
//! Every request is recorded as it arrives, as method, path (with the
//! `/api/v1` prefix taken off) and JSON body. The camera-lease calls
//! are recorded a second time with their token and `ttl`, so that
//! `Then` steps assert on what was sent.
//!
//! The wire shapes are written out here and not taken from `rp-guider`,
//! the crate that holds rp's client: a double that shared the client's
//! types would agree with a mistake in them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::watch;

/// The `unique_id` the double reports for its camera until a scenario
/// names another.
pub const STUB_GUIDE_CAMERA_ID: &str = "stub-guide-camera";

/// The `unique_id` the double reports for its mount until a scenario
/// names another.
pub const STUB_GUIDE_MOUNT_ID: &str = "stub-guide-mount";

/// The guider's states (guider.md § States).
const STATES: [&str; 6] = [
    "idle",
    "looping",
    "calibrating",
    "guiding",
    "paused",
    "star_lost",
];

const API_PREFIX: &str = "/api/v1";

/// How often a request that waits to settle looks for a stop or an
/// abort.
const SETTLE_TICK: Duration = Duration::from_millis(20);

/// The `state` value of `name`, which must be one of the guider's.
///
/// # Panics
///
/// Panics on a name that is none of the guider's states, so that a
/// misspelt state in a feature file fails its scenario.
fn known_state(name: &str) -> &'static str {
    STATES
        .iter()
        .copied()
        .find(|state| *state == name)
        .unwrap_or_else(|| panic!("{name:?} is not a state of the guider: {STATES:?}"))
}

fn is_run_state(state: &str) -> bool {
    matches!(state, "calibrating" | "guiding" | "paused" | "star_lost")
}

/// Running totals the double reports on `guiding/stats`
/// (guider.md § Statistics).
///
/// They grow by the same amount on every stats read: read N reports N
/// times each per-read value, so any two consecutive reads differ by
/// exactly one read's growth. Every step carries both distances, so
/// each axis's sample count is the step count.
#[derive(Debug, Clone, Default)]
pub struct CannedTotals {
    pub guide_steps_per_read: u64,
    pub ra_sum_sq_px2_per_read: f64,
    pub dec_sum_sq_px2_per_read: f64,
    pub star_lost_per_read: u64,
    pub interruptions_per_read: u64,
    /// Report a new `epoch` on every read: a guider that restarted
    /// between any two reads.
    pub new_epoch_per_read: bool,
}

impl CannedTotals {
    /// The `totals` object of the `read`-th stats read (1-based).
    fn at_read(&self, read: u32) -> Value {
        let steps = self.guide_steps_per_read.saturating_mul(u64::from(read));
        let reads = f64::from(read);
        let epoch = if self.new_epoch_per_read {
            format!("stub-epoch-{read}")
        } else {
            "stub-epoch".to_string()
        };
        json!({
            "epoch": epoch,
            "guide_steps": steps,
            "ra_samples": steps,
            "ra_sum_sq_px2": self.ra_sum_sq_px2_per_read * reads,
            "dec_samples": steps,
            "dec_sum_sq_px2": self.dec_sum_sq_px2_per_read * reads,
            "star_lost": self.star_lost_per_read.saturating_mul(u64::from(read)),
            "interruptions": self.interruptions_per_read.saturating_mul(u64::from(read)),
        })
    }
}

/// One request the double received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubRequest {
    /// `GET`, `POST`, `PUT` or `DELETE`.
    pub method: String,
    /// The path with `/api/v1` taken off, e.g. `/guiding/start`.
    pub path: String,
    /// The JSON body; `{}` for a request that sent none.
    pub body: Value,
}

/// One call to a camera-lease endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseCall {
    /// `POST /camera/lease`, and whether the double granted it.
    Acquire {
        token: String,
        ttl: Duration,
        granted: bool,
    },
    /// `PUT /camera/lease/{token}`, and whether the token held the
    /// lease.
    Renew {
        token: String,
        ttl: Duration,
        found: bool,
    },
    /// `DELETE /camera/lease/{token}`, and whether the token held the
    /// lease.
    Release { token: String, released: bool },
}

/// The camera lease the double holds for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLease {
    pub token: String,
    pub ttl: Duration,
}

struct Inner {
    state: &'static str,
    /// The reason of the latest stop, as `last_stop` reports it.
    last_stop: Option<Value>,
    /// An error to answer a path with: path → (code, message).
    failures: HashMap<String, (String, String)>,
    camera_unique_id: Option<String>,
    mount_unique_id: Option<String>,
    settle_delay: Duration,
    lease_grant_never_answers: bool,
    lease: Option<HeldLease>,
    lease_calls: Vec<LeaseCall>,
    requests: Vec<StubRequest>,
    /// `None` is a star-lost frame.
    hfd_script: Vec<Option<f64>>,
    metrics_polls: u64,
    totals: Option<CannedTotals>,
    stats_reads: u32,
    /// Counts the restarts; names the `epoch`.
    epoch: u32,
    /// Counts the accepted `guiding/stop` requests.
    stops: u64,
    /// Answer an accepted `guiding/stop` with `stop_timeout`.
    stop_times_out: bool,
    /// Counts the `dither/abort` requests that found a dither waiting.
    dither_aborts: u64,
    dithers_in_flight: u32,
}

impl Inner {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            last_stop: None,
            failures: HashMap::new(),
            camera_unique_id: Some(STUB_GUIDE_CAMERA_ID.to_string()),
            mount_unique_id: Some(STUB_GUIDE_MOUNT_ID.to_string()),
            settle_delay: Duration::ZERO,
            lease_grant_never_answers: false,
            lease: None,
            lease_calls: Vec::new(),
            requests: Vec::new(),
            hfd_script: vec![Some(2.5)],
            metrics_polls: 0,
            totals: None,
            stats_reads: 0,
            epoch: 0,
            stops: 0,
            stop_times_out: false,
            dither_aborts: 0,
            dithers_in_flight: 0,
        }
    }

    fn epoch(&self) -> String {
        format!("stub-epoch-{}", self.epoch)
    }

    fn record_stop(&mut self, reason: &str) {
        self.last_stop = Some(json!({
            "reason": reason,
            "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "message": format!("the run stopped: {reason}"),
        }));
    }

    /// The answer of a request that settled (guider.md
    /// § `POST /api/v1/guiding/start`).
    fn settled(&self) -> Value {
        json!({
            "state": self.state,
            "rms_ra_px": 0.21,
            "rms_dec_px": 0.18,
            "total_rms_px": 0.28,
            "rms_ra_arcsec": 0.63,
            "rms_dec_arcsec": 0.54,
            "total_rms_arcsec": 0.83,
            "sample_count": 12,
        })
    }

    /// The error a scenario put on `path`, if any.
    fn injected(&self, path: &str) -> Result<(), Refusal> {
        self.failures
            .get(path)
            .map_or(Ok(()), |(code, message)| Err(Refusal::new(code, message)))
    }

    /// The error that ends a request's wait to settle: a `guiding/stop`
    /// since the request was accepted, when the count of stops was
    /// `stops`, or a `dither/abort` for the dither whose count of
    /// aborts at acceptance is `aborts`.
    fn interrupted(&self, stops: u64, aborts: Option<u64>) -> Option<Refusal> {
        if self.stops != stops {
            return Some(Refusal::new(
                "stopped",
                "a guiding/stop arrived while the request waited",
            ));
        }
        aborts
            .is_some_and(|aborts| self.dither_aborts != aborts)
            .then(|| {
                Refusal::new(
                    "dither_aborted",
                    "a dither/abort arrived while the dither waited to settle",
                )
            })
    }

    /// `guiding/start` up to its wait to settle: the count of stops at
    /// acceptance.
    fn accept_start(&mut self) -> Result<u64, Refusal> {
        self.injected("/guiding/start")?;
        if self.lease.is_some() {
            return Err(Refusal::new(
                "camera_leased",
                "a client holds the camera lease",
            ));
        }
        self.state = "guiding";
        Ok(self.stops)
    }

    fn stop(&mut self) -> Answer {
        self.injected("/guiding/stop")?;
        if self.state != "idle" {
            self.state = "idle";
            self.record_stop("requested");
        }
        self.stops = self.stops.saturating_add(1);
        if self.stop_times_out {
            return Err(Refusal::new(
                "stop_timeout",
                "the loop did not end within stop_timeout",
            ));
        }
        Ok(json!({ "state": "idle" }))
    }

    fn pause(&mut self) -> Answer {
        self.injected("/guiding/pause")?;
        match self.state {
            "guiding" | "star_lost" | "paused" => {
                self.state = "paused";
                Ok(json!({ "state": "paused" }))
            }
            _ => Err(Refusal::new("not_guiding", "no run is guiding")),
        }
    }

    fn resume(&mut self) -> Answer {
        self.injected("/guiding/resume")?;
        match self.state {
            "paused" => self.state = "guiding",
            "guiding" | "star_lost" => {}
            _ => return Err(Refusal::new("not_guiding", "no run is paused")),
        }
        Ok(json!({ "state": self.state }))
    }

    /// `dither` up to its wait to settle: the counts of stops and of
    /// aborts at acceptance. An accepted dither is in flight until its
    /// [`DitherInFlight`] drops.
    fn accept_dither(&mut self, body: &Value) -> Result<(u64, u64), Refusal> {
        self.injected("/dither")?;
        let amounts = ["amount_px", "amount_arcsec"]
            .iter()
            .filter(|field| body.get(**field).is_some_and(Value::is_number))
            .count();
        if amounts != 1 {
            return Err(Refusal::new(
                "invalid_request",
                "a dither names exactly one of amount_px and amount_arcsec",
            ));
        }
        if self.state != "guiding" {
            return Err(Refusal::new(
                "not_guiding",
                "a dither needs a run that guides",
            ));
        }
        self.dithers_in_flight = self.dithers_in_flight.saturating_add(1);
        Ok((self.stops, self.dither_aborts))
    }

    fn abort_dither(&mut self) -> Answer {
        self.injected("/dither/abort")?;
        let aborted = self.dithers_in_flight > 0;
        if aborted {
            self.dither_aborts = self.dither_aborts.saturating_add(1);
        }
        Ok(json!({ "state": self.state, "aborted": aborted }))
    }

    fn reselect(&self) -> Answer {
        self.injected("/star/reselect")?;
        if self.state == "idle" {
            return Err(Refusal::new("not_exposing", "no frames are taken"));
        }
        Ok(json!({ "state": self.state, "star_count": 5 }))
    }

    fn clear_calibration(&self) -> Answer {
        self.injected("/calibration/clear")?;
        if is_run_state(self.state) {
            return Err(Refusal::new("not_idle", "a run is in progress"));
        }
        Ok(json!({ "state": self.state }))
    }

    fn acquire_lease(&mut self, body: &Value) -> Answer {
        self.injected("/camera/lease")?;
        let token = body.get("token").and_then(Value::as_str).unwrap_or("");
        let (true, Some(ttl)) = (well_formed_token(token), lease_ttl(body)) else {
            return Err(Refusal::new(
                "invalid_request",
                "a lease needs a token of 32 lowercase hex digits and a ttl of 10s to 5m",
            ));
        };
        let refused = if self.state != "idle" {
            Some(Refusal::new(
                "not_idle",
                "a camera lease needs an idle guider",
            ))
        } else if self.lease.as_ref().is_some_and(|held| held.token != token) {
            Some(Refusal::new(
                "camera_leased",
                "another token holds the camera lease",
            ))
        } else {
            None
        };
        self.lease_calls.push(LeaseCall::Acquire {
            token: token.to_string(),
            ttl,
            granted: refused.is_none(),
        });
        if let Some(refused) = refused {
            return Err(refused);
        }
        let lease = HeldLease {
            token: token.to_string(),
            ttl,
        };
        let answer = lease_answer(&lease);
        self.lease = Some(lease);
        Ok(answer)
    }

    fn renew_lease(&mut self, token: String, body: &Value) -> Answer {
        let Some(ttl) = lease_ttl(body) else {
            return Err(Refusal::new(
                "invalid_request",
                "a lease needs a ttl of 10s to 5m",
            ));
        };
        let found = self.lease.as_ref().is_some_and(|held| held.token == token);
        self.lease_calls.push(LeaseCall::Renew {
            token: token.clone(),
            ttl,
            found,
        });
        if !found {
            return Err(Refusal::new(
                "lease_not_found",
                "the token holds no camera lease",
            ));
        }
        let lease = HeldLease { token, ttl };
        let answer = lease_answer(&lease);
        self.lease = Some(lease);
        Ok(answer)
    }

    fn release_lease(&mut self, token: String) -> Value {
        let released = self.lease.as_ref().is_some_and(|held| held.token == token);
        if released {
            self.lease = None;
        }
        self.lease_calls
            .push(LeaseCall::Release { token, released });
        json!({ "released": released })
    }

    fn stats(&mut self) -> Answer {
        self.injected("/guiding/stats")?;
        self.stats_reads = self.stats_reads.saturating_add(1);
        let totals = self.totals.as_ref().map_or_else(
            || {
                json!({
                    "epoch": self.epoch(),
                    "guide_steps": 0,
                    "ra_samples": 0,
                    "ra_sum_sq_px2": 0.0,
                    "dec_samples": 0,
                    "dec_sum_sq_px2": 0.0,
                    "star_lost": 0,
                    "interruptions": 0,
                })
            },
            |totals| totals.at_read(self.stats_reads),
        );
        // The RMS window holds a run's guide steps, and the star figures
        // are the latest measured frame's: an idle guider has neither.
        let run = is_run_state(self.state).then_some(());
        let exposing = (self.state != "idle").then_some(());
        Ok(json!({
            "state": self.state,
            "guiding": self.state == "guiding",
            "settling": false,
            "pixel_scale_arcsec": 3.0,
            "rms_ra_px": run.map(|()| 0.21),
            "rms_dec_px": run.map(|()| 0.18),
            "total_rms_px": run.map(|()| 0.28),
            "rms_ra_arcsec": run.map(|()| 0.63),
            "rms_dec_arcsec": run.map(|()| 0.54),
            "total_rms_arcsec": run.map(|()| 0.83),
            "snr": exposing.map(|()| 25.0),
            "star_mass": exposing.map(|()| 5432.0),
            "star_count": exposing.map(|()| 5),
            "sample_count": if run.is_some() { 12 } else { 0 },
            "last_stop": self.last_stop,
            "totals": totals,
        }))
    }

    fn metrics(&mut self) -> Answer {
        self.injected("/guiding/metrics")?;
        let frames: Vec<Value> = if self.state == "idle" {
            Vec::new()
        } else {
            let poll = self.metrics_polls;
            self.metrics_polls = poll.saturating_add(1);
            let index = usize::try_from(poll)
                .unwrap_or(usize::MAX)
                .min(self.hfd_script.len().saturating_sub(1));
            let hfd = self.hfd_script.get(index).copied().flatten();
            // A star-lost frame moves a run that guides to `star_lost`,
            // and a frame with the stars back moves it to `guiding`
            // (guider.md § States).
            match (hfd, self.state) {
                (None, "guiding") => self.state = "star_lost",
                (Some(_), "star_lost") => self.state = "guiding",
                _ => {}
            }
            let frame_end = poll.saturating_add(1).saturating_mul(5);
            let lost = hfd.is_none();
            (frame_end.saturating_sub(11).max(1)..=frame_end)
                .map(|frame| {
                    json!({
                        "frame": frame,
                        "hfd": hfd,
                        "snr": if lost { Value::Null } else { json!(25.0) },
                        "star_mass": if lost { Value::Null } else { json!(5432.0) },
                        "star_count": if lost { 0 } else { 5 },
                        "star_lost": lost,
                    })
                })
                .collect()
        };
        Ok(json!({
            "state": self.state,
            "guiding": self.state == "guiding",
            "epoch": self.epoch(),
            "frames": frames,
        }))
    }

    fn devices(&self) -> Answer {
        self.injected("/devices")?;
        Ok(json!({
            "camera": {
                "reachable": true,
                "connected": true,
                "unique_id": self.camera_unique_id,
                "leased": self.lease.is_some(),
                "sensor_type": "monochrome",
                "pixel_size_um": 3.76,
                "width": 1920,
                "height": 1080,
                "binning": 1,
                "can_abort_exposure": true,
            },
            "mount": {
                "reachable": true,
                "connected": true,
                "unique_id": self.mount_unique_id,
                "can_pulse_guide": true,
            },
            "rotator": null,
        }))
    }
}

/// An error answer: the `error` and the `message` of the envelope
/// (guider.md § Error envelope).
#[derive(Debug)]
struct Refusal {
    code: String,
    message: String,
}

impl Refusal {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            status_of(&self.code),
            Json(json!({ "error": self.code, "message": self.message })),
        )
            .into_response()
    }
}

/// What the state machine answers a request with.
type Answer = Result<Value, Refusal>;

/// What a handler answers a request with.
type Answered = Result<Json<Value>, Refusal>;

struct Shared {
    inner: Mutex<Inner>,
    /// `false` while the double is told not to answer.
    answering: watch::Sender<bool>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("guider stub lock poisoned")
    }

    /// Runs `f` on the state machine and lets go of it.
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.lock())
    }

    /// Runs `f` on the state machine, which it only reads.
    fn read<T>(&self, f: impl FnOnce(&Inner) -> T) -> T {
        f(&self.lock())
    }

    /// Records the request, then holds it while the double does not
    /// answer. Returns the parsed body.
    async fn arrive(&self, method: &str, path: &str, body: &Bytes) -> Value {
        let body = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        self.lock().requests.push(StubRequest {
            method: method.to_string(),
            path: path.to_string(),
            body: body.clone(),
        });
        let mut answering = self.answering.subscribe();
        let _ = answering.wait_for(|answering| *answering).await;
        body
    }

    /// Waits out the settle delay, and ends early with the error to
    /// answer when the wait is interrupted (see [`Inner::interrupted`]).
    async fn settle(&self, stops: u64, aborts: Option<u64>) -> Result<(), Refusal> {
        let delay = self.read(|inner| inner.settle_delay);
        let started = tokio::time::Instant::now();
        loop {
            if let Some(ended) = self.read(|inner| inner.interrupted(stops, aborts)) {
                return Err(ended);
            }
            let left = delay.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(());
            }
            tokio::time::sleep(left.min(SETTLE_TICK)).await;
        }
    }
}

/// Counts a dither as in flight from its acceptance until its request
/// is answered or dropped.
struct DitherInFlight(Arc<Shared>);

impl Drop for DitherInFlight {
    fn drop(&mut self) {
        self.0
            .with(|inner| inner.dithers_in_flight = inner.dithers_in_flight.saturating_sub(1));
    }
}

/// The HTTP status of an error code (guider.md § Error envelope).
fn status_of(code: &str) -> StatusCode {
    match code {
        "invalid_request" => StatusCode::BAD_REQUEST,
        "lease_not_found" | "no_frame" => StatusCode::NOT_FOUND,
        "not_configured"
        | "not_guiding"
        | "not_idle"
        | "not_exposing"
        | "camera_leased"
        | "device_not_connected"
        | "mount_not_ready"
        | "stopped"
        | "dither_aborted" => StatusCode::CONFLICT,
        "camera_unsupported" | "no_stars" | "calibration_failed" | "guide_failed" => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        "device_unreachable" => StatusCode::BAD_GATEWAY,
        "settle_timeout" | "stop_timeout" => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// A lease token as the contract wants it: 32 lowercase hexadecimal
/// digits.
fn well_formed_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `ttl` of a lease body: `"60s"` when left out, and between 10 s
/// and 5 min.
fn lease_ttl(body: &Value) -> Option<Duration> {
    let ttl = match body.get("ttl") {
        None | Some(Value::Null) => Duration::from_secs(60),
        Some(ttl) => humantime::parse_duration(ttl.as_str()?).ok()?,
    };
    (Duration::from_secs(10)..=Duration::from_secs(300))
        .contains(&ttl)
        .then_some(ttl)
}

/// The answer to an acquire and to a renewal.
fn lease_answer(lease: &HeldLease) -> Value {
    json!({
        "token": lease.token,
        "expires_in": humantime::format_duration(lease.ttl).to_string(),
    })
}

/// The double of the guider's HTTP contract. See the module docs.
pub struct GuiderStub {
    pub url: String,
    pub port: u16,
    shared: Arc<Shared>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl GuiderStub {
    /// Serve the guider's API on `127.0.0.1:0`, in the state `state`.
    ///
    /// # Panics
    ///
    /// Panics if `state` is none of the guider's states, or if the
    /// double cannot bind a loopback port or read back the address it
    /// bound.
    pub async fn start(state: &str) -> Self {
        let (answering, _) = watch::channel(true);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner::new(known_state(state))),
            answering,
        });

        let api = Router::new()
            .route("/guiding/start", post(start_handler))
            .route("/guiding/stop", post(stop_handler))
            .route("/guiding/pause", post(pause_handler))
            .route("/guiding/resume", post(resume_handler))
            .route("/dither", post(dither_handler))
            .route("/dither/abort", post(dither_abort_handler))
            .route("/star/reselect", post(reselect_handler))
            .route("/calibration/clear", post(calibration_clear_handler))
            .route("/camera/lease", post(lease_acquire_handler))
            .route(
                "/camera/lease/{token}",
                put(lease_renew_handler).delete(lease_release_handler),
            )
            .route("/guiding/stats", get(stats_handler))
            .route("/guiding/metrics", get(metrics_handler))
            .route("/devices", get(devices_handler));
        let app = Router::new()
            .nest(API_PREFIX, api)
            .route("/health", get(health_handler))
            .fallback(unknown_handler)
            .with_state(shared.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind guider stub");
        let port = listener
            .local_addr()
            .expect("guider stub has no local addr")
            .port();
        let url = format!("http://127.0.0.1:{port}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("guider stub failed");
        });

        Self {
            url,
            port,
            shared,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// The state the double is in.
    #[must_use]
    pub fn state(&self) -> String {
        self.shared.lock().state.to_string()
    }

    /// Put the double in `state`, as the guider's own loop would: a
    /// star lost (`star_lost`) or found again (`guiding`).
    pub fn set_state(&self, state: &str) {
        self.shared.lock().state = known_state(state);
    }

    /// Report a latest stop of `reason`, leaving the state alone.
    pub fn set_last_stop(&self, reason: &str) {
        self.shared.lock().record_stop(reason);
    }

    /// End the run as the guider does on its own: `idle`, with
    /// `reason` as the latest stop.
    pub fn stop_on_its_own(&self, reason: &str) {
        let mut inner = self.shared.lock();
        inner.state = "idle";
        inner.record_stop(reason);
    }

    /// End the run as another client's `guiding/stop` does, and leave
    /// the double in `state` (`idle`, or `looping` for a client that
    /// started the loop again).
    pub fn stopped_by_another_client(&self, state: &str) {
        let mut inner = self.shared.lock();
        inner.state = known_state(state);
        inner.record_stop("requested");
    }

    /// Restart: `idle`, no latest stop, no camera lease, a new epoch,
    /// and answering again. The record of requests stays.
    pub fn restart(&self) {
        {
            let mut inner = self.shared.lock();
            inner.state = "idle";
            inner.last_stop = None;
            inner.lease = None;
            inner.epoch = inner.epoch.saturating_add(1);
            inner.metrics_polls = 0;
        }
        self.shared.answering.send_replace(true);
    }

    /// Stop answering: every request from now on is recorded and then
    /// held, as by a guider that hangs.
    pub fn stop_answering(&self) {
        self.shared.answering.send_replace(false);
    }

    /// End the run on every `guiding/stop` as an answered stop does,
    /// and answer the stop with `stop_timeout`: a guider whose exposure
    /// outlasted its `stop_timeout`, which sends no further pulse in any
    /// case (guider.md § `POST /api/v1/guiding/stop`).
    pub fn stop_times_out(&self) {
        self.shared.lock().stop_times_out = true;
    }

    /// The reason of the latest stop `guiding/stats` reports, if any.
    #[must_use]
    pub fn last_stop_reason(&self) -> Option<String> {
        self.shared
            .lock()
            .last_stop
            .as_ref()
            .and_then(|stop| stop.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Answer every request to `path` (as recorded, e.g.
    /// `/guiding/start`) with the error `code` and `message`.
    pub fn fail(&self, path: &str, code: &str, message: &str) {
        self.shared
            .lock()
            .failures
            .insert(path.to_string(), (code.to_string(), message.to_string()));
    }

    /// The `unique_id` `devices` reports for the camera: `None` for
    /// `null`, as before the guider's first device poll.
    pub fn set_camera_unique_id(&self, unique_id: Option<&str>) {
        self.shared.lock().camera_unique_id = unique_id.map(str::to_string);
    }

    /// The `unique_id` `devices` reports for the mount: `None` for
    /// `null`.
    pub fn set_mount_unique_id(&self, unique_id: Option<&str>) {
        self.shared.lock().mount_unique_id = unique_id.map(str::to_string);
    }

    /// How long `guiding/start` and `dither` wait before they answer.
    pub fn set_settle_delay(&self, delay: Duration) {
        self.shared.lock().settle_delay = delay;
    }

    /// Grant every camera-lease acquire and never answer it: a grant
    /// whose answer is lost.
    pub fn grant_lease_and_never_answer(&self) {
        self.shared.lock().lease_grant_never_answers = true;
    }

    /// The HFD `guiding/metrics` serves, one entry per request: request
    /// N serves `script[min(N-1, len-1)]` for every frame of its
    /// answer, so the last value repeats. Each request moves the frame
    /// counter on by 5 and answers the trailing 12 frames, so a poller
    /// sees new frame numbers every time. `idle` serves no frame and
    /// takes no entry.
    ///
    /// # Panics
    ///
    /// Panics on an empty script.
    pub fn set_hfd_script(&self, script: Vec<f64>) {
        self.set_hfd_script_with_lost(script.into_iter().map(Some).collect());
    }

    /// As [`Self::set_hfd_script`], where `None` serves star-lost
    /// frames: no HFD, no star, `star_lost: true`. Such an answer moves
    /// a run in `guiding` to `star_lost`, and the next answer with an
    /// HFD moves it back.
    ///
    /// # Panics
    ///
    /// Panics on an empty script.
    pub fn set_hfd_script_with_lost(&self, script: Vec<Option<f64>>) {
        assert!(!script.is_empty(), "the HFD script must not be empty");
        self.shared.lock().hfd_script = script;
    }

    /// The running totals `guiding/stats` reports. Without this they
    /// stand still at zero.
    pub fn set_totals(&self, totals: CannedTotals) {
        self.shared.lock().totals = Some(totals);
    }

    /// Every request received, in order of arrival.
    #[must_use]
    pub fn requests(&self) -> Vec<StubRequest> {
        self.shared.lock().requests.clone()
    }

    /// The bodies of the requests to exactly `path`, in order of
    /// arrival. `/camera/lease` is the acquire alone: a renewal and a
    /// release carry their token in the path.
    #[must_use]
    pub fn requests_to(&self, path: &str) -> Vec<Value> {
        self.shared
            .lock()
            .requests
            .iter()
            .filter(|request| request.path == path)
            .map(|request| request.body.clone())
            .collect()
    }

    /// The camera lease held, if any.
    #[must_use]
    pub fn lease(&self) -> Option<HeldLease> {
        self.shared.lock().lease.clone()
    }

    /// Every call to a camera-lease endpoint, in order of arrival.
    #[must_use]
    pub fn lease_calls(&self) -> Vec<LeaseCall> {
        self.shared.lock().lease_calls.clone()
    }

    /// The tokens of the acquires the double granted, in order.
    #[must_use]
    pub fn granted_tokens(&self) -> Vec<String> {
        self.lease_calls()
            .into_iter()
            .filter_map(|call| match call {
                LeaseCall::Acquire {
                    token,
                    granted: true,
                    ..
                } => Some(token),
                _ => None,
            })
            .collect()
    }

    /// The tokens of the releases received, in order.
    #[must_use]
    pub fn released_tokens(&self) -> Vec<String> {
        self.lease_calls()
            .into_iter()
            .filter_map(|call| match call {
                LeaseCall::Release { token, .. } => Some(token),
                _ => None,
            })
            .collect()
    }

    /// Stop the server.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for GuiderStub {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn start_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/guiding/start", &body).await;
    let stops = shared.with(Inner::accept_start)?;
    shared.settle(stops, None).await?;
    Ok(Json(shared.read(Inner::settled)))
}

async fn stop_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/guiding/stop", &body).await;
    shared.with(Inner::stop).map(Json)
}

async fn pause_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/guiding/pause", &body).await;
    shared.with(Inner::pause).map(Json)
}

async fn resume_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/guiding/resume", &body).await;
    shared.with(Inner::resume).map(Json)
}

async fn dither_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    let body = shared.arrive("POST", "/dither", &body).await;
    let (stops, aborts) = shared.with(|inner| inner.accept_dither(&body))?;
    let _in_flight = DitherInFlight(shared.clone());
    shared.settle(stops, Some(aborts)).await?;
    Ok(Json(shared.read(Inner::settled)))
}

async fn dither_abort_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/dither/abort", &body).await;
    shared.with(Inner::abort_dither).map(Json)
}

async fn reselect_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/star/reselect", &body).await;
    shared.read(Inner::reselect).map(Json)
}

async fn calibration_clear_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("POST", "/calibration/clear", &body).await;
    shared.read(Inner::clear_calibration).map(Json)
}

async fn lease_acquire_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    let body = shared.arrive("POST", "/camera/lease", &body).await;
    let answer = shared.with(|inner| inner.acquire_lease(&body))?;
    if shared.read(|inner| inner.lease_grant_never_answers) {
        std::future::pending::<()>().await;
    }
    Ok(Json(answer))
}

async fn lease_renew_handler(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    body: Bytes,
) -> Answered {
    let body = shared
        .arrive("PUT", &format!("/camera/lease/{token}"), &body)
        .await;
    shared
        .with(|inner| inner.renew_lease(token, &body))
        .map(Json)
}

async fn lease_release_handler(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    body: Bytes,
) -> Json<Value> {
    shared
        .arrive("DELETE", &format!("/camera/lease/{token}"), &body)
        .await;
    Json(shared.with(|inner| inner.release_lease(token)))
}

async fn stats_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("GET", "/guiding/stats", &body).await;
    shared.with(Inner::stats).map(Json)
}

async fn metrics_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("GET", "/guiding/metrics", &body).await;
    shared.with(Inner::metrics).map(Json)
}

async fn devices_handler(State(shared): State<Arc<Shared>>, body: Bytes) -> Answered {
    shared.arrive("GET", "/devices", &body).await;
    shared.read(Inner::devices).map(Json)
}

async fn health_handler() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

/// A request to a path the contract does not have: recorded, so that a
/// scenario sees it, and answered `404`.
async fn unknown_handler(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let path = uri.path();
    let path = path.strip_prefix(API_PREFIX).unwrap_or(path);
    shared.arrive(method.as_str(), path, &body).await;
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "message": format!("no route {path}") })),
    )
        .into_response()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    const TOKEN: &str = "5f2b8c1e9a4d47c3b0e6a1d2c3f4e5a6";
    const OTHER_TOKEN: &str = "00000000000000000000000000000001";

    async fn send(
        stub: &GuiderStub,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let request = reqwest::Client::new().request(method, format!("{}/api/v1{path}", stub.url));
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    async fn post(stub: &GuiderStub, path: &str, body: Value) -> (u16, Value) {
        send(stub, reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(stub: &GuiderStub, path: &str) -> (u16, Value) {
        send(stub, reqwest::Method::GET, path, None).await
    }

    #[test]
    #[should_panic(expected = "is not a state of the guider")]
    fn an_unknown_state_panics() {
        known_state("stopped");
    }

    #[tokio::test]
    async fn a_start_from_idle_guides_and_answers_the_settled_figures() {
        let stub = GuiderStub::start("idle").await;

        let (status, body) = post(&stub, "/guiding/start", json!({ "recalibrate": true })).await;

        assert_eq!(status, 200);
        assert_eq!(body["state"], "guiding");
        assert_eq!(body["rms_ra_px"], 0.21);
        assert_eq!(body["rms_dec_px"], 0.18);
        assert_eq!(body["total_rms_px"], 0.28);
        assert_eq!(body["sample_count"], 12);
        assert_eq!(stub.state(), "guiding");
    }

    #[tokio::test]
    async fn requests_are_recorded_with_method_path_and_body() {
        let stub = GuiderStub::start("guiding").await;

        post(&stub, "/guiding/pause", json!({ "full": false })).await;
        reqwest::Client::new()
            .post(format!("{}/api/v1/guiding/stop", stub.url))
            .send()
            .await
            .unwrap();
        get(&stub, "/guiding/stats").await;

        assert_eq!(
            stub.requests(),
            vec![
                StubRequest {
                    method: "POST".to_string(),
                    path: "/guiding/pause".to_string(),
                    body: json!({ "full": false }),
                },
                StubRequest {
                    method: "POST".to_string(),
                    path: "/guiding/stop".to_string(),
                    body: json!({}),
                },
                StubRequest {
                    method: "GET".to_string(),
                    path: "/guiding/stats".to_string(),
                    body: json!({}),
                },
            ]
        );
        assert_eq!(
            stub.requests_to("/guiding/pause"),
            vec![json!({ "full": false })]
        );
    }

    #[tokio::test]
    async fn a_stop_idles_the_run_and_reports_it_as_requested() {
        let stub = GuiderStub::start("guiding").await;

        let (status, body) = post(&stub, "/guiding/stop", json!({})).await;

        assert_eq!(status, 200);
        assert_eq!(body, json!({ "state": "idle" }));
        let (_, stats) = get(&stub, "/guiding/stats").await;
        assert_eq!(stats["state"], "idle");
        assert_eq!(stats["last_stop"]["reason"], "requested");
    }

    #[tokio::test]
    async fn a_stop_that_times_out_ends_the_run_and_answers_stop_timeout() {
        let stub = GuiderStub::start("guiding").await;
        stub.stop_times_out();

        let (status, body) = post(&stub, "/guiding/stop", json!({})).await;

        assert_eq!(status, 504);
        assert_eq!(body["error"], "stop_timeout");
        assert_eq!(stub.state(), "idle");
        assert_eq!(stub.last_stop_reason().as_deref(), Some("requested"));
    }

    #[tokio::test]
    async fn a_stop_of_an_idle_guider_records_no_stop() {
        let stub = GuiderStub::start("idle").await;

        post(&stub, "/guiding/stop", json!({})).await;

        let (_, stats) = get(&stub, "/guiding/stats").await;
        assert_eq!(stats["last_stop"], Value::Null);
    }

    #[tokio::test]
    async fn pause_and_resume_follow_the_state() {
        let stub = GuiderStub::start("guiding").await;

        let (_, paused) = post(&stub, "/guiding/pause", json!({ "full": true })).await;
        assert_eq!(paused, json!({ "state": "paused" }));
        let (_, resumed) = post(&stub, "/guiding/resume", json!({})).await;
        assert_eq!(resumed, json!({ "state": "guiding" }));

        stub.set_state("star_lost");
        let (_, unchanged) = post(&stub, "/guiding/resume", json!({})).await;
        assert_eq!(unchanged, json!({ "state": "star_lost" }));
    }

    #[tokio::test]
    async fn pause_and_resume_refuse_a_guider_in_no_run() {
        let stub = GuiderStub::start("idle").await;

        let (status, body) = post(&stub, "/guiding/pause", json!({})).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_guiding");
        let (status, body) = post(&stub, "/guiding/resume", json!({})).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_guiding");
    }

    #[tokio::test]
    async fn an_injected_error_answers_its_path_alone_with_the_codes_status() {
        let stub = GuiderStub::start("guiding").await;
        stub.fail("/guiding/stop", "stop_timeout", "too slow");

        let (status, body) = post(&stub, "/guiding/stop", json!({})).await;

        assert_eq!(status, 504);
        assert_eq!(
            body,
            json!({ "error": "stop_timeout", "message": "too slow" })
        );
        assert_eq!(stub.state(), "guiding");
        let (status, _) = post(&stub, "/guiding/pause", json!({})).await;
        assert_eq!(status, 200);
    }

    #[test]
    fn every_code_of_the_contract_has_its_status() {
        for (code, status) in [
            ("invalid_request", 400),
            ("lease_not_found", 404),
            ("no_frame", 404),
            ("not_configured", 409),
            ("not_guiding", 409),
            ("not_idle", 409),
            ("not_exposing", 409),
            ("camera_leased", 409),
            ("device_not_connected", 409),
            ("mount_not_ready", 409),
            ("stopped", 409),
            ("dither_aborted", 409),
            ("camera_unsupported", 422),
            ("no_stars", 422),
            ("calibration_failed", 422),
            ("guide_failed", 422),
            ("device_unreachable", 502),
            ("settle_timeout", 504),
            ("stop_timeout", 504),
            ("internal", 500),
        ] {
            assert_eq!(status_of(code).as_u16(), status, "{code}");
        }
    }

    #[tokio::test]
    async fn a_dither_needs_a_guiding_run_and_exactly_one_amount() {
        let stub = GuiderStub::start("guiding").await;

        let (status, body) = post(&stub, "/dither", json!({ "amount_px": 5.0 })).await;
        assert_eq!(status, 200);
        assert_eq!(body["state"], "guiding");

        let (status, body) = post(
            &stub,
            "/dither",
            json!({ "amount_px": 5.0, "amount_arcsec": 5.0 }),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_request");

        let (status, _) = post(&stub, "/dither", json!({ "ra_only": true })).await;
        assert_eq!(status, 400);

        stub.set_state("paused");
        let (status, body) = post(&stub, "/dither", json!({ "amount_arcsec": 5.0 })).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_guiding");
    }

    #[tokio::test]
    async fn an_abort_ends_the_dither_that_waits_to_settle() {
        let stub = GuiderStub::start("guiding").await;
        stub.set_settle_delay(Duration::from_secs(30));
        let url = stub.url.clone();
        let dither = tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(format!("{url}/api/v1/dither"))
                .json(&json!({ "amount_px": 5.0 }))
                .send()
                .await
                .unwrap();
            (
                response.status().as_u16(),
                response.json::<Value>().await.unwrap(),
            )
        });
        while stub.requests_to("/dither").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The dither is recorded on arrival and counted in flight a
        // moment later; an abort that found none answers `false`.
        let aborted = loop {
            let (_, body) = post(&stub, "/dither/abort", json!({})).await;
            if body["aborted"] == true {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };

        assert_eq!(aborted, json!({ "state": "guiding", "aborted": true }));
        let (status, body) = dither.await.unwrap();
        assert_eq!(status, 409);
        assert_eq!(body["error"], "dither_aborted");
    }

    #[tokio::test]
    async fn an_abort_with_no_dither_in_flight_answers_false() {
        let stub = GuiderStub::start("guiding").await;

        let (status, body) = post(&stub, "/dither/abort", json!({})).await;

        assert_eq!(status, 200);
        assert_eq!(body, json!({ "state": "guiding", "aborted": false }));
    }

    #[tokio::test]
    async fn a_stop_answers_the_start_that_waits_to_settle_with_stopped() {
        let stub = GuiderStub::start("idle").await;
        stub.set_settle_delay(Duration::from_secs(30));
        let url = stub.url.clone();
        let start = tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(format!("{url}/api/v1/guiding/start"))
                .send()
                .await
                .unwrap();
            (
                response.status().as_u16(),
                response.json::<Value>().await.unwrap(),
            )
        });
        while stub.state() != "guiding" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        post(&stub, "/guiding/stop", json!({})).await;

        let (status, body) = start.await.unwrap();
        assert_eq!(status, 409);
        assert_eq!(body["error"], "stopped");
        assert_eq!(stub.state(), "idle");
    }

    #[tokio::test]
    async fn reselect_and_calibration_clear_follow_the_state() {
        let stub = GuiderStub::start("idle").await;

        let (status, body) = post(&stub, "/star/reselect", json!({})).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_exposing");
        let (status, body) = post(&stub, "/calibration/clear", json!({})).await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "state": "idle" }));

        stub.set_state("guiding");
        let (status, body) = post(&stub, "/star/reselect", json!({})).await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "state": "guiding", "star_count": 5 }));
        let (status, body) = post(&stub, "/calibration/clear", json!({})).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_idle");
    }

    #[tokio::test]
    async fn a_camera_lease_is_granted_renewed_and_released_by_its_token() {
        let stub = GuiderStub::start("idle").await;

        let (status, body) = post(
            &stub,
            "/camera/lease",
            json!({ "token": TOKEN, "ttl": "1m" }),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["token"], TOKEN);
        assert_eq!(
            stub.lease(),
            Some(HeldLease {
                token: TOKEN.to_string(),
                ttl: Duration::from_secs(60),
            })
        );

        let (status, _) = send(
            &stub,
            reqwest::Method::PUT,
            &format!("/camera/lease/{TOKEN}"),
            Some(json!({ "ttl": "30s" })),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(stub.lease().unwrap().ttl, Duration::from_secs(30));

        let (status, body) = send(
            &stub,
            reqwest::Method::DELETE,
            &format!("/camera/lease/{TOKEN}"),
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "released": true }));
        assert_eq!(stub.lease(), None);
        assert_eq!(
            stub.lease_calls(),
            vec![
                LeaseCall::Acquire {
                    token: TOKEN.to_string(),
                    ttl: Duration::from_secs(60),
                    granted: true,
                },
                LeaseCall::Renew {
                    token: TOKEN.to_string(),
                    ttl: Duration::from_secs(30),
                    found: true,
                },
                LeaseCall::Release {
                    token: TOKEN.to_string(),
                    released: true,
                },
            ]
        );
        assert_eq!(stub.granted_tokens(), vec![TOKEN.to_string()]);
        assert_eq!(stub.released_tokens(), vec![TOKEN.to_string()]);
        assert_eq!(stub.requests_to("/camera/lease").len(), 1);
    }

    #[tokio::test]
    async fn a_camera_lease_is_refused_outside_idle_and_to_a_second_token() {
        let stub = GuiderStub::start("guiding").await;

        let (status, body) = post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "not_idle");
        assert_eq!(stub.lease(), None);

        stub.set_state("idle");
        post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;
        let (status, body) = post(&stub, "/camera/lease", json!({ "token": OTHER_TOKEN })).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "camera_leased");
        let (status, _) = post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;
        assert_eq!(status, 200);
        assert_eq!(stub.granted_tokens().len(), 2);
    }

    #[tokio::test]
    async fn a_malformed_token_or_a_ttl_out_of_range_is_an_invalid_request() {
        let stub = GuiderStub::start("idle").await;

        for body in [
            json!({ "token": "ABCDEF" }),
            json!({ "token": TOKEN.to_uppercase() }),
            json!({ "token": TOKEN, "ttl": "5s" }),
            json!({ "token": TOKEN, "ttl": "10m" }),
            json!({ "token": TOKEN, "ttl": 60 }),
        ] {
            let (status, answer) = post(&stub, "/camera/lease", body.clone()).await;
            assert_eq!(status, 400, "{body}");
            assert_eq!(answer["error"], "invalid_request", "{body}");
        }
        assert_eq!(stub.lease(), None);
    }

    #[tokio::test]
    async fn a_renewal_of_no_lease_is_not_found_and_a_release_of_none_is_false() {
        let stub = GuiderStub::start("idle").await;

        let (status, body) = send(
            &stub,
            reqwest::Method::PUT,
            &format!("/camera/lease/{TOKEN}"),
            Some(json!({ "ttl": "60s" })),
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "lease_not_found");

        let (status, body) = send(
            &stub,
            reqwest::Method::DELETE,
            &format!("/camera/lease/{TOKEN}"),
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "released": false }));
    }

    #[tokio::test]
    async fn a_held_camera_lease_refuses_a_start() {
        let stub = GuiderStub::start("idle").await;
        post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;

        let (status, body) = post(&stub, "/guiding/start", json!({})).await;

        assert_eq!(status, 409);
        assert_eq!(body["error"], "camera_leased");
        assert_eq!(stub.state(), "idle");
    }

    #[tokio::test]
    async fn a_grant_that_never_answers_still_holds_the_lease() {
        let stub = GuiderStub::start("idle").await;
        stub.grant_lease_and_never_answer();

        let outcome = reqwest::Client::new()
            .post(format!("{}/api/v1/camera/lease", stub.url))
            .json(&json!({ "token": TOKEN }))
            .timeout(Duration::from_millis(300))
            .send()
            .await;

        assert!(outcome.unwrap_err().is_timeout());
        assert_eq!(stub.lease().unwrap().token, TOKEN);
    }

    #[tokio::test]
    async fn stats_follow_the_state() {
        let stub = GuiderStub::start("idle").await;

        let (_, idle) = get(&stub, "/guiding/stats").await;
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["guiding"], false);
        assert_eq!(idle["total_rms_px"], Value::Null);
        assert_eq!(idle["sample_count"], 0);
        assert_eq!(idle["snr"], Value::Null);
        assert_eq!(idle["last_stop"], Value::Null);
        assert_eq!(idle["totals"]["epoch"], "stub-epoch-0");

        stub.set_state("guiding");
        stub.set_last_stop("mount_slewing");
        let (_, guiding) = get(&stub, "/guiding/stats").await;
        assert_eq!(guiding["guiding"], true);
        assert_eq!(guiding["settling"], false);
        assert_eq!(guiding["pixel_scale_arcsec"], 3.0);
        assert_eq!(guiding["total_rms_px"], 0.28);
        assert_eq!(guiding["sample_count"], 12);
        assert_eq!(guiding["snr"], 25.0);
        assert_eq!(guiding["star_mass"], 5432.0);
        assert_eq!(guiding["star_count"], 5);
        assert_eq!(guiding["last_stop"]["reason"], "mount_slewing");

        stub.set_state("paused");
        let (_, paused) = get(&stub, "/guiding/stats").await;
        assert_eq!(paused["guiding"], false);
        assert_eq!(paused["total_rms_px"], 0.28);
    }

    #[tokio::test]
    async fn canned_totals_grow_with_every_stats_read() {
        let stub = GuiderStub::start("guiding").await;
        stub.set_totals(CannedTotals {
            guide_steps_per_read: 10,
            ra_sum_sq_px2_per_read: 2.5,
            dec_sum_sq_px2_per_read: 4.0,
            star_lost_per_read: 1,
            interruptions_per_read: 0,
            new_epoch_per_read: false,
        });

        let (_, first) = get(&stub, "/guiding/stats").await;
        let (_, second) = get(&stub, "/guiding/stats").await;

        assert_eq!(first["totals"]["guide_steps"], 10);
        assert_eq!(second["totals"]["guide_steps"], 20);
        assert_eq!(second["totals"]["ra_samples"], 20);
        assert_eq!(second["totals"]["ra_sum_sq_px2"], 5.0);
        assert_eq!(second["totals"]["dec_sum_sq_px2"], 8.0);
        assert_eq!(second["totals"]["star_lost"], 2);
        assert_eq!(first["totals"]["epoch"], second["totals"]["epoch"]);
    }

    #[tokio::test]
    async fn canned_totals_can_change_epoch_with_every_read() {
        let stub = GuiderStub::start("guiding").await;
        stub.set_totals(CannedTotals {
            new_epoch_per_read: true,
            ..CannedTotals::default()
        });

        let (_, first) = get(&stub, "/guiding/stats").await;
        let (_, second) = get(&stub, "/guiding/stats").await;

        assert_ne!(first["totals"]["epoch"], second["totals"]["epoch"]);
    }

    #[tokio::test]
    async fn metrics_serve_the_script_to_a_guider_that_exposes() {
        let stub = GuiderStub::start("idle").await;
        stub.set_hfd_script(vec![2.0, 3.0]);

        let (_, idle) = get(&stub, "/guiding/metrics").await;
        assert_eq!(idle["frames"], json!([]));
        assert_eq!(idle["guiding"], false);

        stub.set_state("guiding");
        let (_, first) = get(&stub, "/guiding/metrics").await;
        let (_, second) = get(&stub, "/guiding/metrics").await;
        let (_, third) = get(&stub, "/guiding/metrics").await;

        assert_eq!(first["guiding"], true);
        assert_eq!(first["epoch"], "stub-epoch-0");
        let frames = first["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0]["frame"], 1);
        assert_eq!(frames[4]["frame"], 5);
        assert_eq!(frames[0]["hfd"], 2.0);
        assert_eq!(frames[0]["star_count"], 5);
        assert_eq!(frames[0]["star_lost"], false);
        let frames = second["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 10);
        assert_eq!(frames[9]["frame"], 10);
        assert_eq!(frames[9]["hfd"], 3.0);
        let frames = third["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 12);
        assert_eq!(frames[0]["frame"], 4);
        assert_eq!(frames[11]["frame"], 15);
        assert_eq!(frames[11]["hfd"], 3.0);
    }

    #[tokio::test]
    async fn a_star_lost_frame_moves_a_guiding_run_to_star_lost_and_back() {
        let stub = GuiderStub::start("guiding").await;
        stub.set_hfd_script_with_lost(vec![None, Some(2.0)]);

        let (_, lost) = get(&stub, "/guiding/metrics").await;
        assert_eq!(lost["state"], "star_lost");
        assert_eq!(lost["guiding"], false);
        let frame = &lost["frames"][0];
        assert_eq!(frame["star_lost"], true);
        assert_eq!(frame["hfd"], Value::Null);
        assert_eq!(frame["star_count"], 0);
        assert_eq!(stub.state(), "star_lost");

        let (_, back) = get(&stub, "/guiding/metrics").await;
        assert_eq!(back["state"], "guiding");
        assert_eq!(back["guiding"], true);
        assert_eq!(back["frames"][9]["star_lost"], false);
        assert_eq!(back["frames"][9]["hfd"], 2.0);
    }

    #[tokio::test]
    async fn devices_report_the_scripted_unique_ids() {
        let stub = GuiderStub::start("idle").await;

        let (_, defaults) = get(&stub, "/devices").await;
        assert_eq!(defaults["camera"]["unique_id"], STUB_GUIDE_CAMERA_ID);
        assert_eq!(defaults["mount"]["unique_id"], STUB_GUIDE_MOUNT_ID);
        assert_eq!(defaults["camera"]["leased"], false);
        assert_eq!(defaults["rotator"], Value::Null);

        stub.set_camera_unique_id(None);
        stub.set_mount_unique_id(Some(""));
        post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;
        let (_, scripted) = get(&stub, "/devices").await;
        assert_eq!(scripted["camera"]["unique_id"], Value::Null);
        assert_eq!(scripted["mount"]["unique_id"], "");
        assert_eq!(scripted["camera"]["leased"], true);
    }

    #[tokio::test]
    async fn a_stub_that_stops_answering_records_the_request_and_holds_it() {
        let stub = GuiderStub::start("guiding").await;
        stub.stop_answering();

        let outcome = reqwest::Client::new()
            .get(format!("{}/api/v1/guiding/stats", stub.url))
            .timeout(Duration::from_millis(300))
            .send()
            .await;

        assert!(outcome.unwrap_err().is_timeout());
        assert_eq!(stub.requests_to("/guiding/stats").len(), 1);
    }

    #[tokio::test]
    async fn a_restart_idles_the_stub_and_drops_what_it_held() {
        let stub = GuiderStub::start("idle").await;
        post(&stub, "/camera/lease", json!({ "token": TOKEN })).await;
        stub.set_state("guiding");
        stub.set_last_stop("mount_slewing");
        stub.stop_answering();

        stub.restart();

        let (_, stats) = get(&stub, "/guiding/stats").await;
        assert_eq!(stats["state"], "idle");
        assert_eq!(stats["last_stop"], Value::Null);
        assert_eq!(stats["totals"]["epoch"], "stub-epoch-1");
        assert_eq!(stub.lease(), None);
        assert_eq!(stub.requests_to("/camera/lease").len(), 1);
    }

    #[tokio::test]
    async fn a_run_ended_from_outside_reports_its_reason() {
        let stub = GuiderStub::start("guiding").await;

        stub.stop_on_its_own("camera_error");
        let (_, stats) = get(&stub, "/guiding/stats").await;
        assert_eq!(stats["state"], "idle");
        assert_eq!(stats["last_stop"]["reason"], "camera_error");

        stub.set_state("guiding");
        stub.stopped_by_another_client("looping");
        let (_, stats) = get(&stub, "/guiding/stats").await;
        assert_eq!(stats["state"], "looping");
        assert_eq!(stats["last_stop"]["reason"], "requested");
    }

    #[tokio::test]
    async fn a_path_outside_the_contract_is_recorded_and_not_found() {
        let stub = GuiderStub::start("idle").await;

        let (status, _) = get(&stub, "/equipment").await;

        assert_eq!(status, 404);
        assert_eq!(stub.requests()[0].path, "/equipment");
    }

    #[tokio::test]
    async fn health_answers_ok() {
        let stub = GuiderStub::start("idle").await;

        let body: Value = reqwest::get(format!("{}/health", stub.url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(body, json!({ "status": "ok" }));
    }
}

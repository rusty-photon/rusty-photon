use ascom_alpaca::api::camera::{CameraState, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use ndarray::Array2;
use parking_lot::Mutex;
use std::num::{NonZeroU32, NonZeroU8};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

use crate::config::Config;
use crate::config_actions::SkySurveyCameraDriver;
use crate::fits::parse_primary_hdu;
use crate::pointing::{PointingSource, PointingState, SharedPointing};
use crate::survey::{try_cache_load, try_cache_store, SurveyClient, SurveyError, SurveyRequest};
use rusty_photon_driver::ConfigActionCtx;

/// 0x500 — ASCOM "unspecified" / driver-specific catch-all. The
/// Behavioral Contracts in `docs/services/sky-survey-camera.md` use
/// "`UNSPECIFIED_ERROR`" for everything that isn't covered by a more
/// precise standard code.
const UNSPECIFIED_ERROR: ASCOMErrorCode = ASCOMErrorCode::new_for_driver(0);

const MAX_BIN: u8 = 4;
const EXPOSURE_MIN: Duration = Duration::from_micros(1);
const EXPOSURE_MAX: Duration = Duration::from_hours(1);

/// Builds the v0 cutout `SurveyRequest` for a snapshot of camera
/// state: the cutout is always sized to the binned full sensor (the
/// design doc crops sub-frames out client-side after the FITS comes
/// back).
#[must_use]
pub fn build_full_sensor_request(
    config: &Config,
    pointing: PointingState,
    bin_x: u8,
    bin_y: u8,
) -> SurveyRequest {
    let arcsec_per_pixel_x =
        206.265 * config.optics.pixel_size_x_um / config.optics.focal_length_mm;
    let arcsec_per_pixel_y =
        206.265 * config.optics.pixel_size_y_um / config.optics.focal_length_mm;
    // `new(..).unwrap_or(MIN)` is `.max(1)` shaped as a `NonZero`, so
    // the divisions below cannot hit zero.
    let bx = NonZeroU32::from(NonZeroU8::new(bin_x).unwrap_or(NonZeroU8::MIN));
    let by = NonZeroU32::from(NonZeroU8::new(bin_y).unwrap_or(NonZeroU8::MIN));
    SurveyRequest {
        survey: config.survey.name.clone(),
        ra_deg: pointing.ra_deg,
        dec_deg: pointing.dec_deg,
        rotation_deg: pointing.rotation_deg,
        pixels_x: config.optics.sensor_width_px / bx,
        pixels_y: config.optics.sensor_height_px / by,
        size_x_deg: arcsec_per_pixel_x * f64::from(config.optics.sensor_width_px) / 3600.0,
        size_y_deg: arcsec_per_pixel_y * f64::from(config.optics.sensor_height_px) / 3600.0,
    }
}

/// Outcome of the spawned exposure task — stashed on the device state
/// so subsequent ASCOM calls can map it to `ImageArray` vs ASCOM error.
#[derive(Debug)]
pub struct ExposureOutcome {
    /// `usize` because they describe `data`: the only things done with
    /// them are shaping that buffer and sizing it. The ASCOM `NumX`/
    /// `NumY` they came from stay fixed-width on the request side.
    pub width: usize,
    pub height: usize,
    pub data: Vec<i32>,
}

/// Shared state held by the [`SkySurveyCamera`] device and the custom
/// `/sky-survey/*` HTTP routes.
///
/// Cloning a [`SkySurveyCamera`] only clones the `Arc` — both views
/// observe the same connection and pointing state.
///
/// `exposure_generation` is bumped on every `start_exposure`, every
/// `abort_exposure` / `stop_exposure`, and every `set_connected(false)`
/// — the spawned exposure task captures the value at start and only
/// commits its result if the captured value still matches when it
/// finishes, so a late-completing task can never resurrect an image
/// after Abort/Stop/disconnect.
#[derive(Debug)]
pub struct DeviceState {
    pub config: Config,
    pub connected: AtomicBool,
    /// Snapshot source. `Static` reads `last_snapshot` directly;
    /// `Telescope` reads the configured ASCOM mount and writes the
    /// result into `last_snapshot` for `GET /sky-survey/position`.
    pub pointing_source: PointingSource,
    /// Most-recently-snapshotted pointing. Both modes use this as the
    /// source for `GET /sky-survey/position` (F6 — "most recently
    /// snapshotted") and the in-flight cutout request. In `Static`
    /// mode the inner `Arc` is shared with `pointing_source`, so
    /// `POST` writes are visible immediately; in `Telescope` mode the
    /// exposure pipeline writes back after a successful mount read.
    pub last_snapshot: Arc<SharedPointing>,
    /// Optional one-shot pointing override armed by
    /// `POST /sky-survey/position` while in follow mode. The next
    /// `StartExposure` (light) takes the value, uses it as the
    /// snapshot, and clears the field. Subsequent exposures resume
    /// follow-mode reads from the mount. Used by BDD scenarios to
    /// inject "the camera saw something different from where the
    /// mount thinks it is" on a single capture (F7); the standard
    /// follow-mode read path is otherwise unchanged.
    pub next_pointing_override: Mutex<Option<PointingState>>,
    pub bin_x: AtomicU8,
    pub bin_y: AtomicU8,
    pub num_x: AtomicU32,
    pub num_y: AtomicU32,
    pub start_x: AtomicU32,
    pub start_y: AtomicU32,
    pub exposure_in_flight: AtomicBool,
    pub image_ready: AtomicBool,
    pub last_image: Mutex<Option<ExposureOutcome>>,
    pub last_error: Mutex<Option<String>>,
    pub last_exposure_start: Mutex<Option<SystemTime>>,
    pub last_exposure_duration: Mutex<Option<Duration>>,
    pub exposure_generation: AtomicU64,
    pub survey_client: Arc<dyn SurveyClient>,
}

#[derive(Clone, derive_more::Debug)]
pub struct SkySurveyCamera {
    state: Arc<DeviceState>,
    /// `Some` when built through the reload loop with a config source; `None`
    /// for focused unit-test devices that don't exercise config actions.
    ///
    /// Skipped from `Debug`: the shared `ConfigActionCtx<D>` has no `Debug` impl.
    /// (The follow-mode credentials its effective `Config` carries are redacted
    /// at the leaf regardless — see [`rp_auth::config::ClientAuthConfig`].)
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<SkySurveyCameraDriver>>,
}

impl SkySurveyCamera {
    /// Construct in static-pointing mode. Asserts that
    /// `config.pointing.telescope` is `None` — follow-mode setup
    /// requires fallible network construction (see
    /// [`mount::AlpacaMountReader::from_config`]) and must go through
    /// `lib::build_device`, which returns `Result`.
    /// Tests that want to exercise follow mode against a mock
    /// `MountReader` should call [`Self::from_parts`] directly.
    pub fn new_static(config: Config, survey_client: Arc<dyn SurveyClient>) -> Self {
        debug_assert!(
            config.pointing.telescope.is_none(),
            "SkySurveyCamera::new_static called with pointing.telescope set; \
             use build_device (in src/lib.rs) for follow mode (it returns Result)"
        );
        let last_snapshot = Arc::new(SharedPointing::new(PointingState::new(
            config.pointing.initial_ra_deg,
            config.pointing.initial_dec_deg,
            config.pointing.initial_rotation_deg,
        )));
        let pointing_source = PointingSource::Static(Arc::clone(&last_snapshot));
        Self::from_parts(config, survey_client, pointing_source, last_snapshot)
    }

    /// Construct from a fully-prepared [`PointingSource`]. Used by
    /// `lib.rs::run_with_client` when follow mode is configured, and
    /// by tests that want to inject a mock `MountReader`.
    pub fn from_parts(
        config: Config,
        survey_client: Arc<dyn SurveyClient>,
        pointing_source: PointingSource,
        last_snapshot: Arc<SharedPointing>,
    ) -> Self {
        let sensor_w = config.optics.sensor_width_px;
        let sensor_h = config.optics.sensor_height_px;
        let state = DeviceState {
            config,
            connected: AtomicBool::new(false),
            pointing_source,
            last_snapshot,
            bin_x: AtomicU8::new(1),
            bin_y: AtomicU8::new(1),
            num_x: AtomicU32::new(sensor_w),
            num_y: AtomicU32::new(sensor_h),
            start_x: AtomicU32::new(0),
            start_y: AtomicU32::new(0),
            exposure_in_flight: AtomicBool::new(false),
            image_ready: AtomicBool::new(false),
            last_image: Mutex::new(None),
            last_error: Mutex::new(None),
            last_exposure_start: Mutex::new(None),
            last_exposure_duration: Mutex::new(None),
            exposure_generation: AtomicU64::new(0),
            survey_client,
            next_pointing_override: Mutex::new(None),
        };
        Self {
            state: Arc::new(state),
            config_ctx: None,
        }
    }

    /// Attach the config-action context, enabling `config.get` / `config.apply`
    /// / `config.schema` on this device.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<SkySurveyCameraDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    #[must_use]
    pub fn shared_state(&self) -> Arc<DeviceState> {
        Arc::clone(&self.state)
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.state.connected.load(Ordering::Acquire)
    }
}

/// The body of the spawned exposure task. Performs the cache hit /
/// fetch / parse / sub-frame crop; on any failure stores the message
/// in `state.last_error` and clears `in_flight` so subsequent
/// `image_array` calls can surface `UNSPECIFIED_ERROR`.
///
/// `gen` is the value of `exposure_generation` at the moment
/// `start_exposure` spawned this task. Abort, Stop, and disconnect
/// bump that counter, so a late-completing task whose generation
/// no longer matches must NOT publish its outcome — that would
/// resurrect a cancelled exposure.
async fn run_exposure(
    state: Arc<DeviceState>,
    light: bool,
    gen: u64,
    pointing_override: Option<PointingState>,
) {
    let result = run_exposure_inner(&state, light, pointing_override).await;
    if state.exposure_generation.load(Ordering::Acquire) != gen {
        debug!(
            ?gen,
            "exposure cancelled before completion; discarding outcome"
        );
        return;
    }
    match result {
        Ok(outcome) => {
            *state.last_image.lock() = Some(outcome);
            *state.last_error.lock() = None;
            state.image_ready.store(true, Ordering::Release);
        }
        Err(err) => {
            warn!(error = %err, "exposure failed");
            *state.last_error.lock() = Some(err);
            // image_ready stays false
        }
    }
    state.exposure_in_flight.store(false, Ordering::Release);
}

async fn run_exposure_inner(
    state: &Arc<DeviceState>,
    light: bool,
    pointing_override: Option<PointingState>,
) -> Result<ExposureOutcome, String> {
    let bx = state.bin_x.load(Ordering::Acquire);
    let by = state.bin_y.load(Ordering::Acquire);
    let nx = state.num_x.load(Ordering::Acquire);
    let ny = state.num_y.load(Ordering::Acquire);
    let sx = state.start_x.load(Ordering::Acquire);
    let sy = state.start_y.load(Ordering::Acquire);
    // `nx`/`ny` stay fixed-width for `crop_subframe`, which takes the
    // subframe as the ASCOM device state it is. The outcome carries the
    // same numbers as the geometry of the buffer it holds, so convert
    // once here. Fallible rather than saturating because the dark-frame
    // path below *allocates* `out_w * out_h` elements — a saturated
    // length would abort the process instead of reporting anything.
    // `StartExposure` has already bounded both against the binned sensor,
    // so this reports a configuration no camera could have.
    let (Ok(out_w), Ok(out_h)) = (usize::try_from(nx), usize::try_from(ny)) else {
        return Err(format!("subframe {nx}x{ny} is too large to address"));
    };
    let Some(out_pixels) = out_w.checked_mul(out_h) else {
        return Err(format!("subframe {nx}x{ny} is too large to address"));
    };

    // Hold `Exposing` state for the requested duration so clients
    // (incl. ConformU) can observe the camera mid-exposure. Capped at
    // 5s so survey/cache fetches don't add their latency on top of an
    // already-long simulated exposure.
    let exposure_sleep = state
        .last_exposure_duration
        .lock()
        .map(|d| std::cmp::min(d, Duration::from_secs(5)));
    if let Some(d) = exposure_sleep {
        tokio::time::sleep(d).await;
    }

    if !light {
        // S2: zero-filled NumX × NumY frame, no fetch.
        return Ok(ExposureOutcome {
            width: out_w,
            height: out_h,
            data: vec![0i32; out_pixels],
        });
    }

    // F1/F4: in follow mode, read the mount fresh on every exposure;
    // in static mode this is the cached `SharedPointing`. After a
    // successful follow-mode read, write back to `last_snapshot` so
    // `GET /sky-survey/position` reflects what the camera last saw.
    //
    // F7: when a one-shot override was armed by `POST
    // /sky-survey/position` in follow mode, `start_exposure` already
    // consumed it (before spawning this task — see comment there);
    // the captured value is passed in via `pointing_override` so a
    // POST that arrives while THIS exposure is still simulating its
    // duration only affects the *next* exposure (P7).
    let pointing = match pointing_override {
        Some(p) => p,
        None => state
            .pointing_source
            .snapshot()
            .await
            .map_err(|e| format!("pointing read failed: {e}"))?,
    };
    if state.pointing_source.is_follow_mode() {
        state.last_snapshot.store(pointing).await;
    }
    let request = build_full_sensor_request(&state.config, pointing, bx, by);
    let cache_dir = state.config.survey.cache_dir.clone();
    let cache_key = request.cache_key();
    let (bytes, from_cache) =
        if let Some(b) = try_cache_load(cache_dir.clone(), cache_key.clone()).await {
            (b, true)
        } else {
            match state.survey_client.fetch(&request).await {
                Ok(b) => (b, false),
                Err(SurveyError::Timeout) => return Err("survey request timed out".into()),
                Err(SurveyError::NonSuccess(code)) => {
                    return Err(format!("survey returned status {code}"))
                }
                Err(SurveyError::Http(msg)) => return Err(format!("survey HTTP error: {msg}")),
            }
        };

    let img = parse_primary_hdu(&bytes).map_err(|e| format!("FITS parse error: {e}"))?;
    let cropped = crop_subframe(&img.data, img.width, img.height, sx, sy, nx, ny)?;
    // S6: only commit a network response to the cache after a
    // successful FITS parse. Otherwise a malformed body could poison
    // the cache and re-fail forever. We move `bytes` into the cache
    // store here — `parse_primary_hdu`'s output owns its own data and
    // `crop_subframe` already ran above, so the original FITS bytes
    // aren't needed downstream.
    if !from_cache {
        try_cache_store(cache_dir, cache_key, bytes).await;
    }
    Ok(ExposureOutcome {
        width: out_w,
        height: out_h,
        data: cropped,
    })
}

/// `src_w`/`src_h` describe the decoded buffer, so they are lengths.
/// The subframe is ASCOM device state (`StartX`/`NumX`) and arrives
/// fixed-width; it becomes a buffer offset here.
pub(crate) fn crop_subframe(
    src: &[i32],
    src_w: usize,
    src_h: usize,
    sx: u32,
    sy: u32,
    nx: u32,
    ny: u32,
) -> Result<Vec<i32>, String> {
    // This walks `src` a row at a time, so the geometry has to describe
    // it. `rp_fits` establishes that on decode, but a survey response is
    // remote input and this function must not slice on a guarantee made
    // somewhere else.
    if src_w.checked_mul(src_h) != Some(src.len()) {
        return Err(format!(
            "source geometry ({src_w},{src_h}) does not match {} pixels",
            src.len()
        ));
    }
    let out_of_bounds =
        || format!("subframe ({sx}+{nx},{sy}+{ny}) exceeds source ({src_w},{src_h})");
    // A subframe too large to be a `usize` cannot fit the source
    // either, so it belongs in the bounds error rather than one of
    // its own.
    let (Ok(sx), Ok(sy), Ok(nx), Ok(ny)) = (
        usize::try_from(sx),
        usize::try_from(sy),
        usize::try_from(nx),
        usize::try_from(ny),
    ) else {
        return Err(out_of_bounds());
    };
    let (Some(x_end), Some(y_end)) = (sx.checked_add(nx), sy.checked_add(ny)) else {
        return Err(out_of_bounds());
    };
    if x_end > src_w || y_end > src_h {
        return Err(out_of_bounds());
    }
    if sx == 0 && sy == 0 && nx == src_w && ny == src_h {
        return Ok(src.to_vec());
    }
    if nx == 0 || ny == 0 {
        return Ok(Vec::new());
    }
    // `nx >= 1` past this point, so the bounds check above pins
    // `src_w >= x_end >= 1` and `chunks_exact` cannot be handed a zero
    // chunk size. `nx * ny <= src.len()` for the same reason, so the
    // saturation never actually engages.
    let mut out = Vec::with_capacity(nx.saturating_mul(ny));
    for row in src.chunks_exact(src_w).skip(sy).take(ny) {
        let Some(cols) = row.get(sx..x_end) else {
            return Err(out_of_bounds());
        };
        out.extend_from_slice(cols);
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Device for SkySurveyCamera {
    fn static_name(&self) -> &str {
        &self.state.config.device.name
    }

    fn unique_id(&self) -> &str {
        &self.state.config.device.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.state.connected.load(Ordering::Acquire))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        if connected {
            // C2: cache_dir must be creatable AND writable. `create_
            // dir_all` succeeds on an existing read-only directory,
            // so we follow it with a probe write/delete.
            let cache_dir = &self.state.config.survey.cache_dir;
            if let Err(e) = std::fs::create_dir_all(cache_dir) {
                debug!(?cache_dir, error = %e, "cache_dir create failed");
                return Err(ASCOMError::new(
                    UNSPECIFIED_ERROR,
                    format!("cache_dir is not writable: {e}"),
                ));
            }
            let probe = cache_dir.join(".sky-survey-camera.write-probe");
            if let Err(e) = std::fs::write(&probe, b"").and_then(|()| std::fs::remove_file(&probe))
            {
                debug!(?cache_dir, error = %e, "cache_dir write probe failed");
                return Err(ASCOMError::new(
                    UNSPECIFIED_ERROR,
                    format!("cache_dir is not writable: {e}"),
                ));
            }
            // C3: probe the survey endpoint with a short capped HEAD.
            // A failed probe is logged at warn! but does NOT block
            // Connect — tying ASCOM Connect latency to NASA's TLS
            // handshake makes the simulator flaky on slow links and
            // in CI. The first real exposure will still surface a
            // hard error via S4 if the endpoint is genuinely down.
            if let Err(e) = self.state.survey_client.health_check().await {
                warn!(
                    endpoint = %self.state.config.survey.endpoint,
                    error = %e,
                    "survey endpoint health check failed; continuing anyway",
                );
            }
        }
        self.state.connected.store(connected, Ordering::Release);
        if !connected {
            // C4: disconnect cancels any in-flight exposure. Bumping
            // the generation makes the spawned task discard its
            // outcome on completion. Per ASCOM convention the
            // LastExposure* properties are reset too — they describe
            // exposures since the *current* connect, so a Connect →
            // Disconnect → Connect cycle should make them error
            // again until a fresh exposure runs.
            self.state
                .exposure_generation
                .fetch_add(1, Ordering::AcqRel);
            self.state
                .exposure_in_flight
                .store(false, Ordering::Release);
            self.state.image_ready.store(false, Ordering::Release);
            *self.state.last_image.lock() = None;
            *self.state.last_error.lock() = None;
            *self.state.last_exposure_start.lock() = None;
            *self.state.last_exposure_duration.lock() = None;
        }
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.state.config.device.description.clone())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon sky-survey-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<SkySurveyCameraDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait::async_trait]
impl Camera for SkySurveyCamera {
    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        Ok(self.state.config.optics.sensor_width_px)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        Ok(self.state.config.optics.sensor_height_px)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        Ok(self.state.config.optics.pixel_size_x_um)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        Ok(self.state.config.optics.pixel_size_y_um)
    }

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        Ok(EXPOSURE_MIN)
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        Ok(EXPOSURE_MAX)
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        Ok(Duration::from_micros(1))
    }

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        Ok(65535)
    }

    async fn max_bin_x(&self) -> ASCOMResult<u8> {
        Ok(MAX_BIN)
    }

    async fn max_bin_y(&self) -> ASCOMResult<u8> {
        Ok(MAX_BIN)
    }

    async fn bin_x(&self) -> ASCOMResult<u8> {
        Ok(self.state.bin_x.load(Ordering::Acquire))
    }

    async fn set_bin_x(&self, bin_x: u8) -> ASCOMResult<()> {
        if !(1..=MAX_BIN).contains(&bin_x) {
            return Err(ASCOMError::invalid_value(format!(
                "BinX {bin_x} outside [1, {MAX_BIN}]"
            )));
        }
        self.state.bin_x.store(bin_x, Ordering::Release);
        Ok(())
    }

    async fn bin_y(&self) -> ASCOMResult<u8> {
        Ok(self.state.bin_y.load(Ordering::Acquire))
    }

    async fn set_bin_y(&self, bin_y: u8) -> ASCOMResult<()> {
        if !(1..=MAX_BIN).contains(&bin_y) {
            return Err(ASCOMError::invalid_value(format!(
                "BinY {bin_y} outside [1, {MAX_BIN}]"
            )));
        }
        self.state.bin_y.store(bin_y, Ordering::Release);
        Ok(())
    }

    async fn num_x(&self) -> ASCOMResult<u32> {
        Ok(self.state.num_x.load(Ordering::Acquire))
    }

    async fn set_num_x(&self, num_x: u32) -> ASCOMResult<()> {
        // ASCOM convention: sub-frame property setters accept any
        // value; geometry validation runs at StartExposure (E4/E5).
        // ConformU exercises this by setting one-past-the-edge then
        // calling StartExposure and expecting INVALID_VALUE there.
        self.state.num_x.store(num_x, Ordering::Release);
        Ok(())
    }

    async fn num_y(&self) -> ASCOMResult<u32> {
        Ok(self.state.num_y.load(Ordering::Acquire))
    }

    async fn set_num_y(&self, num_y: u32) -> ASCOMResult<()> {
        self.state.num_y.store(num_y, Ordering::Release);
        Ok(())
    }

    async fn start_x(&self) -> ASCOMResult<u32> {
        Ok(self.state.start_x.load(Ordering::Acquire))
    }

    async fn set_start_x(&self, start_x: u32) -> ASCOMResult<()> {
        self.state.start_x.store(start_x, Ordering::Release);
        Ok(())
    }

    async fn start_y(&self) -> ASCOMResult<u32> {
        Ok(self.state.start_y.load(Ordering::Acquire))
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.state.start_y.store(start_y, Ordering::Release);
        Ok(())
    }

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        if !self.state.connected.load(Ordering::Acquire) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::NOT_CONNECTED,
                "camera is not connected",
            ));
        }
        if !(EXPOSURE_MIN..=EXPOSURE_MAX).contains(&duration) {
            return Err(ASCOMError::invalid_value(format!(
                "Duration {duration:?} outside [{EXPOSURE_MIN:?}, {EXPOSURE_MAX:?}]"
            )));
        }
        let bx = u32::from(self.state.bin_x.load(Ordering::Acquire));
        let by = u32::from(self.state.bin_y.load(Ordering::Acquire));
        let nx = self.state.num_x.load(Ordering::Acquire);
        let ny = self.state.num_y.load(Ordering::Acquire);
        let sx = self.state.start_x.load(Ordering::Acquire);
        let sy = self.state.start_y.load(Ordering::Acquire);
        let binned_sensor_width = self.state.config.optics.sensor_width_px / bx.max(1);
        let binned_sensor_height = self.state.config.optics.sensor_height_px / by.max(1);
        // E4: NumX/NumY must be > 0. The setters now accept any u32
        // per ASCOM convention; we enforce E4/E5 here at the moment
        // the geometry is actually used.
        if nx == 0 || ny == 0 {
            return Err(ASCOMError::invalid_value(format!(
                "NumX/NumY must be > 0 (got NumX={nx} NumY={ny})"
            )));
        }
        // E5: subframe must fit within the binned sensor. `u32` wrap
        // on `sx + nx` is impossible in practice (sensor sizes are
        // < 2^31) but we still check before the comparison.
        let end_x = sx.saturating_add(nx);
        let end_y = sy.saturating_add(ny);
        if end_x > binned_sensor_width || end_y > binned_sensor_height {
            return Err(ASCOMError::invalid_value(format!(
                "subframe ({sx}+{nx},{sy}+{ny}) exceeds binned sensor ({binned_sensor_width},{binned_sensor_height})"
            )));
        }
        // E2: reject if another exposure is already in flight.
        if self
            .state
            .exposure_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ASCOMError::invalid_operation("exposure already in flight"));
        }
        // Reset readout state for the new exposure.
        self.state.image_ready.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        *self.state.last_image.lock() = None;
        *self.state.last_exposure_start.lock() = Some(SystemTime::now());
        *self.state.last_exposure_duration.lock() = Some(duration);

        // Bump the generation so any *previous* spawned task that
        // races to completion is ignored, and capture the new
        // generation for *this* task to honour at finish time.
        let gen = self
            .state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        // Consume the F7 one-shot pointing override here, *before*
        // spawning the exposure task — not inside the task — so that
        // a `POST /sky-survey/position` issued while this exposure is
        // mid-flight (i.e. during the simulated exposure sleep) only
        // affects the *next* `StartExposure`, matching P7. The
        // captured value is threaded into the spawned task as an
        // explicit parameter.
        let override_for_exposure = if light {
            self.state.next_pointing_override.lock().take()
        } else {
            None
        };
        debug!(?duration, light, gen, "exposure started");
        let state = Arc::clone(&self.state);
        tokio::spawn(run_exposure(state, light, gen, override_for_exposure));
        Ok(())
    }

    async fn abort_exposure(&self) -> ASCOMResult<()> {
        if !self
            .state
            .exposure_in_flight
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok_and(|prev| prev)
        {
            return Err(ASCOMError::invalid_operation(
                "no exposure in progress to abort",
            ));
        }
        // Bump the generation so the in-flight task discards its
        // outcome. The actual fetch task can't always be cancelled at
        // the OS level (e.g. a Hold stub keeps the connection open
        // until process exit) but it can no longer publish results.
        // A1 ("ImageReady is false") holds.
        self.state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel);
        self.state.image_ready.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        *self.state.last_image.lock() = None;
        Ok(())
    }

    async fn stop_exposure(&self) -> ASCOMResult<()> {
        if !self
            .state
            .exposure_in_flight
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok_and(|prev| prev)
        {
            return Err(ASCOMError::invalid_operation(
                "no exposure in progress to stop",
            ));
        }
        self.state
            .exposure_generation
            .fetch_add(1, Ordering::AcqRel);
        self.state.image_ready.store(false, Ordering::Release);
        *self.state.last_error.lock() = None;
        *self.state.last_image.lock() = None;
        Ok(())
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        Ok(self.state.image_ready.load(Ordering::Acquire))
    }

    async fn last_exposure_start_time(&self) -> ASCOMResult<SystemTime> {
        self.state
            .last_exposure_start
            .lock()
            .ok_or_else(|| ASCOMError::invalid_operation("no exposure has started yet"))
    }

    async fn last_exposure_duration(&self) -> ASCOMResult<Duration> {
        self.state
            .last_exposure_duration
            .lock()
            .ok_or_else(|| ASCOMError::invalid_operation("no exposure has started yet"))
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        // S4-S6: a stored fetch error becomes ASCOM UNSPECIFIED_ERROR.
        let last_error = self.state.last_error.lock().clone();
        if let Some(msg) = last_error {
            return Err(ASCOMError::new(UNSPECIFIED_ERROR, msg));
        }
        if !self.state.image_ready.load(Ordering::Acquire) {
            return Err(ASCOMError::invalid_operation("no image is ready"));
        }
        let (height, width, data) = self
            .state
            .last_image
            .lock()
            .as_ref()
            .map(|outcome| (outcome.height, outcome.width, outcome.data.clone()))
            .ok_or_else(|| ASCOMError::invalid_operation("image_ready=true but no stored image"))?;
        // ASCOM ImageArray indexing is `[x][y]` (column-major from a
        // ndarray perspective), so the first axis must be NumX and the
        // second NumY. The FITS data is laid out row-major (rows of
        // width columns), so we build a row-major (height, width)
        // array first and then `.reversed_axes()` swaps the strides
        // in-place — no element copy.
        let array = Array2::from_shape_vec((height, width), data)
            .map_err(|e| ASCOMError::new(UNSPECIFIED_ERROR, format!("ndarray shape: {e}")))?
            .reversed_axes();
        Ok(ImageArray::from(array))
    }

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        if self.state.last_error.lock().is_some() {
            return Ok(CameraState::Error);
        }
        if self.state.exposure_in_flight.load(Ordering::Acquire) {
            return Ok(CameraState::Exposing);
        }
        Ok(CameraState::Idle)
    }

    async fn percent_completed(&self) -> ASCOMResult<u8> {
        // The fetch-or-cache pipeline is atomic from the client's
        // perspective — there's no meaningful intermediate progress
        // to report — so percent is binary.
        if self.state.image_ready.load(Ordering::Acquire) {
            Ok(100)
        } else {
            Ok(0)
        }
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        Ok("SkyView Virtual Sensor".to_string())
    }

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        Ok(SensorType::Monochrome)
    }

    async fn readout_modes(&self) -> ASCOMResult<Vec<String>> {
        Ok(vec!["Default".to_string()])
    }

    async fn readout_mode(&self) -> ASCOMResult<usize> {
        Ok(0)
    }

    async fn set_readout_mode(&self, readout_mode: usize) -> ASCOMResult<()> {
        if readout_mode != 0 {
            return Err(ASCOMError::invalid_value(format!(
                "ReadoutMode {readout_mode} not supported (only index 0)"
            )));
        }
        Ok(())
    }

    async fn electrons_per_adu(&self) -> ASCOMResult<f64> {
        Ok(1.0)
    }

    async fn full_well_capacity(&self) -> ASCOMResult<f64> {
        Ok(65535.0)
    }

    async fn gain(&self) -> ASCOMResult<i32> {
        Ok(0)
    }

    async fn set_gain(&self, gain: i32) -> ASCOMResult<()> {
        if gain != 0 {
            return Err(ASCOMError::invalid_value(format!(
                "Gain {gain} not supported (single fixed value 0)"
            )));
        }
        Ok(())
    }

    async fn gain_min(&self) -> ASCOMResult<i32> {
        Ok(0)
    }

    async fn gain_max(&self) -> ASCOMResult<i32> {
        Ok(0)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{
        AlpacaServerConfig, DeviceConfig, OpticsConfig, PointingConfig, SurveyConfig,
    };

    fn fake_config() -> Config {
        Config {
            device: DeviceConfig {
                name: "Test".into(),
                unique_id: "uid-001".into(),
                description: "test".into(),
            },
            optics: OpticsConfig {
                focal_length_mm: 1000.0,
                pixel_size_x_um: 3.76,
                pixel_size_y_um: 3.76,
                sensor_width_px: 640,
                sensor_height_px: 480,
            },
            pointing: PointingConfig {
                initial_ra_deg: 0.0,
                initial_dec_deg: 0.0,
                initial_rotation_deg: 0.0,
                telescope: None,
                rotator: None,
            },
            survey: SurveyConfig {
                name: "DSS2 Red".into(),
                request_timeout: Duration::from_secs(5),
                cache_dir: std::env::temp_dir().join("sky-survey-camera-tests"),
                endpoint: "http://placeholder/".into(),
            },
            server: AlpacaServerConfig::new(0),
        }
    }

    #[test]
    fn build_full_sensor_request_uses_full_sensor_fov() {
        let cfg = fake_config();
        let pointing = PointingState::new(10.0, 20.0, 0.0);
        let req = build_full_sensor_request(&cfg, pointing, 1, 1);
        assert_eq!(req.pixels_x, 640);
        assert_eq!(req.pixels_y, 480);
        assert!(req.size_x_deg > 0.1 && req.size_x_deg < 1.0);
    }

    #[test]
    fn build_full_sensor_request_halves_pixels_when_binned() {
        let cfg = fake_config();
        let pointing = PointingState::new(0.0, 0.0, 0.0);
        let req = build_full_sensor_request(&cfg, pointing, 2, 2);
        assert_eq!(req.pixels_x, 320);
        assert_eq!(req.pixels_y, 240);
    }

    #[test]
    fn crop_subframe_full_frame_is_passthrough() {
        let src: Vec<i32> = (0..12).collect();
        let out = crop_subframe(&src, 4, 3, 0, 0, 4, 3).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn crop_subframe_central_window() {
        // 4x3 source = [[0,1,2,3],[4,5,6,7],[8,9,10,11]]
        // Crop StartX=1, StartY=1, NumX=2, NumY=2 → [[5,6],[9,10]]
        let src: Vec<i32> = (0..12).collect();
        let out = crop_subframe(&src, 4, 3, 1, 1, 2, 2).unwrap();
        assert_eq!(out, vec![5, 6, 9, 10]);
    }

    /// A zero-area subframe is in bounds wherever it starts, and crops
    /// to zero pixels rather than an error.
    #[test]
    fn crop_subframe_zero_area_is_empty() {
        let src: Vec<i32> = (0..12).collect();
        assert_eq!(
            crop_subframe(&src, 4, 3, 2, 1, 0, 2).unwrap(),
            Vec::<i32>::new()
        );
        assert_eq!(
            crop_subframe(&src, 4, 3, 2, 1, 2, 0).unwrap(),
            Vec::<i32>::new()
        );
    }

    #[test]
    fn crop_subframe_rejects_out_of_bounds() {
        let src: Vec<i32> = vec![0; 12];
        crop_subframe(&src, 4, 3, 3, 0, 2, 1).unwrap_err();
        crop_subframe(&src, 4, 3, 0, 2, 1, 2).unwrap_err();
    }

    /// A survey response short of the geometry its header declares must
    /// not be sliced by row — the subframe bounds check alone compares
    /// against the declared size, not the buffer.
    #[test]
    fn crop_subframe_rejects_geometry_that_does_not_describe_the_buffer() {
        let short: Vec<i32> = vec![0; 8]; // 4×3 declared, 8 pixels present
        let err = crop_subframe(&short, 4, 3, 0, 0, 4, 3).unwrap_err();
        assert!(
            err.contains("does not match 8 pixels"),
            "unexpected error: {err}"
        );
        // The full-frame passthrough shortcut must not slip past it either.
        crop_subframe(&short, 4, 3, 1, 1, 2, 2).unwrap_err();
    }

    /// Trait stub usable from `cfg(test)` without enabling the
    /// public `mock` feature. Fetch is unused — tests that exercise
    /// the survey path go through BDD with a stub HTTP server.
    #[derive(Debug)]
    struct StubSurveyClient;

    #[async_trait::async_trait]
    impl SurveyClient for StubSurveyClient {
        async fn health_check(&self) -> Result<(), SurveyError> {
            Ok(())
        }
        async fn fetch(&self, _request: &SurveyRequest) -> Result<Vec<u8>, SurveyError> {
            Err(SurveyError::Http("stub: fetch not implemented".into()))
        }
    }

    fn fake_camera() -> SkySurveyCamera {
        let cfg = fake_config();
        let client: Arc<dyn SurveyClient> = Arc::new(StubSurveyClient);
        SkySurveyCamera::new_static(cfg, client)
    }

    #[test]
    fn is_connected_starts_false() {
        let cam = fake_camera();
        assert!(!cam.is_connected());
        cam.state.connected.store(true, Ordering::Release);
        assert!(cam.is_connected());
    }

    #[tokio::test]
    async fn camera_trait_static_metadata_matches_config() {
        let cam = fake_camera();
        // Device trait
        assert_eq!(cam.static_name(), "Test");
        assert_eq!(cam.unique_id(), "uid-001");
        assert_eq!(cam.description().await.unwrap(), "test");
        assert_eq!(
            cam.driver_info().await.unwrap(),
            "rusty-photon sky-survey-camera"
        );
        assert_eq!(
            cam.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        assert!(!cam.connected().await.unwrap());
        // Camera trait
        assert_eq!(cam.camera_x_size().await.unwrap(), 640);
        assert_eq!(cam.camera_y_size().await.unwrap(), 480);
        assert!((cam.pixel_size_x().await.unwrap() - 3.76).abs() < 1e-9);
        assert!((cam.pixel_size_y().await.unwrap() - 3.76).abs() < 1e-9);
        assert_eq!(cam.exposure_min().await.unwrap(), Duration::from_micros(1));
        assert_eq!(cam.exposure_max().await.unwrap(), Duration::from_hours(1));
        assert_eq!(
            cam.exposure_resolution().await.unwrap(),
            Duration::from_micros(1)
        );
        assert!(!cam.has_shutter().await.unwrap());
        assert_eq!(cam.max_adu().await.unwrap(), 65535);
        assert_eq!(cam.max_bin_x().await.unwrap(), 4);
        assert_eq!(cam.max_bin_y().await.unwrap(), 4);
    }

    #[tokio::test]
    async fn bin_num_start_round_trip() {
        let cam = fake_camera();
        assert_eq!(cam.bin_x().await.unwrap(), 1);
        assert_eq!(cam.bin_y().await.unwrap(), 1);
        cam.set_bin_x(2).await.unwrap();
        cam.set_bin_y(2).await.unwrap();
        assert_eq!(cam.bin_x().await.unwrap(), 2);
        assert_eq!(cam.bin_y().await.unwrap(), 2);
        cam.set_num_x(100).await.unwrap();
        cam.set_num_y(50).await.unwrap();
        assert_eq!(cam.num_x().await.unwrap(), 100);
        assert_eq!(cam.num_y().await.unwrap(), 50);
        cam.set_start_x(10).await.unwrap();
        cam.set_start_y(5).await.unwrap();
        assert_eq!(cam.start_x().await.unwrap(), 10);
        assert_eq!(cam.start_y().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn last_exposure_methods_pre_first_exposure() {
        let cam = fake_camera();
        let err = cam.last_exposure_start_time().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        let err = cam.last_exposure_duration().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    }

    #[tokio::test]
    async fn last_exposure_methods_after_set() {
        let cam = fake_camera();
        let when = SystemTime::now();
        let duration = Duration::from_millis(500);
        *cam.state.last_exposure_start.lock() = Some(when);
        *cam.state.last_exposure_duration.lock() = Some(duration);
        let returned_when = cam.last_exposure_start_time().await.unwrap();
        let returned_duration = cam.last_exposure_duration().await.unwrap();
        assert_eq!(returned_when, when);
        assert_eq!(returned_duration, duration);
    }

    #[tokio::test]
    async fn image_ready_initially_false() {
        let cam = fake_camera();
        assert!(!cam.image_ready().await.unwrap());
    }

    #[tokio::test]
    async fn image_array_returns_invalid_operation_when_empty() {
        let cam = fake_camera();
        let err = cam.image_array().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    }

    #[tokio::test]
    async fn image_array_surfaces_stored_error_as_unspecified() {
        let cam = fake_camera();
        *cam.state.last_error.lock() = Some("survey returned status 500".into());
        let err = cam.image_array().await.unwrap_err();
        assert_eq!(err.code, UNSPECIFIED_ERROR);
    }

    #[tokio::test]
    async fn image_array_returns_stored_image_when_ready() {
        let cam = fake_camera();
        *cam.state.last_image.lock() = Some(ExposureOutcome {
            width: 4,
            height: 3,
            data: (0..12).collect(),
        });
        cam.state.image_ready.store(true, Ordering::Release);
        let array = cam.image_array().await.unwrap();
        // ImageArray flattens to 3D with rank 2 reported; for our 2D
        // input the underlying data view has 12 elements.
        let total: i32 = array.iter().copied().sum();
        assert_eq!(total, (0..12i32).sum::<i32>());
    }

    #[tokio::test]
    async fn abort_stop_when_idle_return_invalid_operation() {
        let cam = fake_camera();
        let err = cam.abort_exposure().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        let err = cam.stop_exposure().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    }

    #[tokio::test]
    async fn run_exposure_discards_outcome_when_generation_changed() {
        let cam = fake_camera();
        // Bump generation so the captured `gen=0` no longer matches.
        cam.state.exposure_generation.fetch_add(1, Ordering::AcqRel);
        cam.state.exposure_in_flight.store(true, Ordering::Release);
        // Light=false synthesises a zero frame without network I/O.
        run_exposure(Arc::clone(&cam.state), false, 0, None).await;
        // image_ready stays false because the generation check
        // triggered an early return.
        assert!(!cam.state.image_ready.load(Ordering::Acquire));
        assert!(cam.state.last_image.lock().is_none());
        // exposure_in_flight is left untouched on cancellation
        // (Abort/Stop already cleared it from the caller side).
        assert!(cam.state.exposure_in_flight.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn camera_state_reflects_in_flight_and_error() {
        let cam = fake_camera();
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Idle);
        cam.state.exposure_in_flight.store(true, Ordering::Release);
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Exposing);
        cam.state.exposure_in_flight.store(false, Ordering::Release);
        *cam.state.last_error.lock() = Some("boom".into());
        assert_eq!(cam.camera_state().await.unwrap(), CameraState::Error);
    }

    #[tokio::test]
    async fn percent_completed_is_binary() {
        let cam = fake_camera();
        assert_eq!(cam.percent_completed().await.unwrap(), 0);
        cam.state.image_ready.store(true, Ordering::Release);
        assert_eq!(cam.percent_completed().await.unwrap(), 100);
    }

    #[tokio::test]
    async fn readout_mode_only_accepts_zero() {
        let cam = fake_camera();
        assert_eq!(cam.readout_mode().await.unwrap(), 0);
        assert_eq!(cam.readout_modes().await.unwrap(), vec!["Default"]);
        cam.set_readout_mode(0).await.unwrap();
        let err = cam.set_readout_mode(1).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn sensor_name_and_type_are_populated() {
        let cam = fake_camera();
        assert_eq!(cam.sensor_name().await.unwrap(), "SkyView Virtual Sensor");
        assert_eq!(cam.sensor_type().await.unwrap(), SensorType::Monochrome);
    }

    #[tokio::test]
    async fn gain_reports_single_fixed_value() {
        let cam = fake_camera();
        assert_eq!(cam.gain().await.unwrap(), 0);
        assert_eq!(cam.gain_min().await.unwrap(), 0);
        assert_eq!(cam.gain_max().await.unwrap(), 0);
        cam.set_gain(0).await.unwrap();
        let err = cam.set_gain(5).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn full_well_and_electrons_per_adu() {
        let cam = fake_camera();
        assert!((cam.electrons_per_adu().await.unwrap() - 1.0).abs() < 1e-12);
        assert!((cam.full_well_capacity().await.unwrap() - 65535.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn setters_accept_out_of_range_values() {
        // ASCOM convention: NumX/NumY/StartX/StartY setters always
        // accept; geometry validation happens at StartExposure.
        let cam = fake_camera();
        cam.set_num_x(99_999).await.unwrap();
        cam.set_num_y(99_999).await.unwrap();
        cam.set_start_x(99_999).await.unwrap();
        cam.set_start_y(99_999).await.unwrap();
        assert_eq!(cam.num_x().await.unwrap(), 99_999);
        assert_eq!(cam.start_x().await.unwrap(), 99_999);
    }

    #[tokio::test]
    async fn start_exposure_rejects_zero_num() {
        let cam = fake_camera();
        cam.state.connected.store(true, Ordering::Release);
        cam.set_num_x(0).await.unwrap();
        let err = cam
            .start_exposure(Duration::from_millis(1), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn start_exposure_rejects_oversized_subframe() {
        let cam = fake_camera();
        cam.state.connected.store(true, Ordering::Release);
        // Sensor is 640×480; 700×100 overflows X.
        cam.set_num_x(700).await.unwrap();
        cam.set_num_y(100).await.unwrap();
        let err = cam
            .start_exposure(Duration::from_millis(1), true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn run_exposure_publishes_zero_frame_on_light_false_when_uncancelled() {
        let cam = fake_camera();
        cam.state.exposure_in_flight.store(true, Ordering::Release);
        let gen = cam.state.exposure_generation.load(Ordering::Acquire);
        run_exposure(Arc::clone(&cam.state), false, gen, None).await;
        assert!(cam.state.image_ready.load(Ordering::Acquire));
        let img = cam.state.last_image.lock();
        let outcome = img.as_ref().unwrap();
        // Default num_x/num_y match the sensor dimensions.
        assert_eq!(outcome.width, 640);
        assert_eq!(outcome.height, 480);
        assert!(outcome.data.iter().all(|v| *v == 0));
        assert!(!cam.state.exposure_in_flight.load(Ordering::Acquire));
    }
}

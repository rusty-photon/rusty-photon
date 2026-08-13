//! Harness for spawning `sky-survey-camera` from BDD scenarios.
//!
//! The closed-loop centering scenarios in
//! `services/rp/tests/features/` need a camera that follows the
//! `OmniSim` Telescope (so a `slew` on the mount changes what the
//! camera renders). This module provides:
//!
//! - [`SkyViewStub`] — an in-process axum server that emulates NASA
//!   `SkyView`. It parses `Position` / `Pixels` / `Size` query params
//!   from the camera's request and returns a minimal FITS that
//!   advertises matching `CRVAL1` / `CRVAL2`. The rp closed-loop
//!   centering scenarios don't actually consume those CRVAL records
//!   — `rp`'s persistence layer writes its own FITS from the
//!   camera's `ImageArray` and strips the camera-side WCS, so the
//!   scenarios fake solve outcomes via
//!   [`crate::rp_harness::StubBehavior::Sequence`] instead. The
//!   CRVAL-bearing FITS the stub mints is still useful as a
//!   general-purpose primitive for tests that hand a synthetic
//!   `imagebytes` payload directly to a downstream consumer that
//!   *does* honour camera-side WCS (e.g. unit tests that pair the
//!   stub with [`crate::rp_harness::StubBehavior::EchoFitsCenter`]).
//! - [`SkySurveyCameraConfig`] / [`SkySurveyCameraConfigBuilder`] —
//!   builds the JSON config the production binary expects.
//! - [`start_sky_survey_camera`] — spawns the binary via
//!   [`crate::ServiceHandle`], parses the bound port from stdout.
//!
//! The harness intentionally does not depend on the
//! `sky-survey-camera` crate: the `SkyView` wire shape is the contract
//! and the duplication is the point (mirroring the
//! `plate_solver_stub` posture).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::extract::RawQuery;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use serde_json::Value;

use crate::rp_harness::write_temp_config_file;
use crate::ServiceHandle;

/// Handle for the in-process `SkyView` stub. The stub serves the same
/// HEAD / GET surface NASA `SkyView` exposes; the camera's
/// `SkyViewClient` connects to it via the `survey.endpoint` config
/// override.
#[derive(Debug)]
pub struct SkyViewStub {
    pub url: String,
    pub port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SkyViewStub {
    /// Bind a `SkyView` stub on `127.0.0.1:0` and return its public URL.
    pub async fn start() -> Self {
        let app = Router::new().fallback(any(handle_skyview));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("SkyViewStub bind");
        let addr: SocketAddr = listener.local_addr().expect("SkyViewStub local_addr");
        let url = format!("http://{addr}/");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Self {
            url,
            port: addr.port(),
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Stop the stub. Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for SkyViewStub {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hard cap on the rendered cutout. Production cameras top out at
/// roughly 60 megapixels (Fujifilm GFX series, etc.); 8192 × 8192 ≈
/// 67 megapixels is a comfortable upper bound that still fits in
/// ~128 MB at u16 + a few hundred MB at i32 — enough headroom for
/// any plausible test scenario without letting a malformed
/// `Pixels=` query allocate a runaway buffer that OOMs the test
/// process.
const MAX_PIXELS_PER_AXIS: u32 = 8192;

async fn handle_skyview(method: Method, RawQuery(query): RawQuery) -> axum::response::Response {
    if method == Method::HEAD {
        return (StatusCode::OK, Vec::<u8>::new()).into_response();
    }
    let params = parse_query(query.as_deref().unwrap_or(""));
    let (ra_deg, dec_deg) =
        parse_pair(params.get("Position").map(String::as_str)).unwrap_or((0.0, 0.0));
    let (w, h) = parse_pixels(params.get("Pixels").map(String::as_str)).unwrap_or((64, 48));
    let (size_x_deg, _size_y_deg) =
        parse_pair(params.get("Size").map(String::as_str)).unwrap_or((0.1, 0.1));
    // Defensive bound on pixel dimensions: a malformed `Pixels=`
    // query (or one with values larger than any real camera would
    // request) would otherwise drive a `vec![0u16; w*h]` allocation
    // that can overflow `usize::checked_mul` or OOM the test runner.
    if w == 0 || h == 0 || w > MAX_PIXELS_PER_AXIS || h > MAX_PIXELS_PER_AXIS {
        return (
            StatusCode::BAD_REQUEST,
            format!("Pixels out of range: requested {w}x{h}, cap {MAX_PIXELS_PER_AXIS} per axis"),
        )
            .into_response();
    }
    let pixel_scale_arcsec = (size_x_deg * 3600.0) / f64::from(w).max(1.0);
    // Reject non-finite scale (e.g. caller passed a finite but huge
    // `Size=` that overflows the multiply). Otherwise the `Float`
    // keyword construction in `synth_fits_with_wcs` would panic via
    // `rp_fits::writer::Keyword::new`, which rejects non-finite
    // values.
    if !pixel_scale_arcsec.is_finite() {
        return (
            StatusCode::BAD_REQUEST,
            format!("derived pixel_scale_arcsec is not finite: size_x_deg={size_x_deg}"),
        )
            .into_response();
    }
    let bytes = synth_fits_with_wcs(w, h, ra_deg, dec_deg, pixel_scale_arcsec);
    (StatusCode::OK, bytes).into_response()
}

fn parse_query(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    // Minimal `+`-as-space + `%XX` decoder. SkyView accepts both
    // forms. Invalid escapes pass through verbatim — the caller's
    // numeric parsers will reject them.
    let mut out = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let decoded = match tail {
                    [hi, lo, ..] => char::from(*hi)
                        .to_digit(16)
                        .zip(char::from(*lo).to_digit(16))
                        .map(|(h, l)| h.saturating_mul(16).saturating_add(l)),
                    _ => None,
                };
                match decoded.and_then(|v| u8::try_from(v).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        rest = rest.get(2..).unwrap_or_default();
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn parse_pair(s: Option<&str>) -> Option<(f64, f64)> {
    let s = s?;
    let mut parts = s.split(',');
    let a: f64 = parts.next()?.trim().parse().ok()?;
    let b: f64 = parts.next()?.trim().parse().ok()?;
    // Reject NaN/Inf — they flow into `synth_fits_with_wcs`, which
    // routes them into `rp_fits::writer::Keyword::new` for `CRVAL*` /
    // `CDELT*` records, and that rejects non-finite floats. Returning
    // `None` here lets the caller fall back to its default values
    // instead of crashing the stub server on a malformed query.
    if !a.is_finite() || !b.is_finite() {
        return None;
    }
    Some((a, b))
}

fn parse_pixels(s: Option<&str>) -> Option<(u32, u32)> {
    let s = s?;
    let mut parts = s.split(',');
    let w: u32 = parts.next()?.trim().parse().ok()?;
    let h: u32 = parts.next()?.trim().parse().ok()?;
    Some((w, h))
}

/// Build a minimal `BITPIX=16` FITS that advertises a TAN WCS at
/// `(ra_center_deg, dec_center_deg)` with the given plate scale.
/// Pixel data is zero-filled — the centering loop's plate-solver stub
/// reads only header records.
///
/// `width` and `height` must satisfy
/// `width * height <= isize::MAX as usize` (Rust's `Vec` ceiling);
/// callers are expected to apply [`MAX_PIXELS_PER_AXIS`] (the value
/// `handle_skyview` enforces) so the multiplication can't overflow.
/// The internal `checked_mul` is belt-and-suspenders for direct
/// callers (unit tests / future helpers) that bypass `handle_skyview`.
fn synth_fits_with_wcs(
    width: u32,
    height: u32,
    ra_center_deg: f64,
    dec_center_deg: f64,
    pixel_scale_arcsec: f64,
) -> Vec<u8> {
    use rp_fits::writer::{write_u16_image, Keyword, KeywordValue};

    let crpix1 = f64::from(width) / 2.0 + 0.5;
    let crpix2 = f64::from(height) / 2.0 + 0.5;
    let cdelt = pixel_scale_arcsec / 3600.0;

    let extras = vec![
        Keyword::new("CTYPE1", KeywordValue::Str("RA---TAN".into())).unwrap(),
        Keyword::new("CTYPE2", KeywordValue::Str("DEC--TAN".into())).unwrap(),
        Keyword::new("CRVAL1", KeywordValue::Float(ra_center_deg)).unwrap(),
        Keyword::new("CRVAL2", KeywordValue::Float(dec_center_deg)).unwrap(),
        Keyword::new("CRPIX1", KeywordValue::Float(crpix1)).unwrap(),
        Keyword::new("CRPIX2", KeywordValue::Float(crpix2)).unwrap(),
        Keyword::new("CDELT1", KeywordValue::Float(-cdelt)).unwrap(),
        Keyword::new("CDELT2", KeywordValue::Float(cdelt)).unwrap(),
    ];

    // The dimensions arrive as a `Pixels=` query field and end up as
    // both a buffer length and `f64` WCS header cards, so this is the
    // boundary: `f64::from` wants the wire type, the allocation and the
    // writer want the length.
    let w = usize::try_from(width).expect("width fits usize");
    let h = usize::try_from(height).expect("height fits usize");
    let pixel_count = w
        .checked_mul(h)
        .expect("width * height overflows usize (callers must cap dimensions)");
    let pixels = vec![0u16; pixel_count];
    let mut out = Vec::new();
    write_u16_image(&mut out, &pixels, w, h, &extras).expect("write_u16_image");
    out
}

/// Configuration for one `sky-survey-camera` instance.
///
/// Use [`SkySurveyCameraConfigBuilder`] to construct. Defaults are
/// chosen so the plate scale is the same as `sky-survey-camera`'s
/// default config (1000 mm / 3.76 µm ≈ 0.776 "/px) and the sensor is
/// small enough to keep test wall-clock low.
#[derive(Debug, Clone)]
pub struct SkySurveyCameraConfig {
    pub device_name: String,
    pub unique_id: String,
    pub focal_length_mm: f64,
    pub pixel_size_um: f64,
    pub sensor_width_px: u32,
    pub sensor_height_px: u32,
    pub initial_ra_deg: f64,
    pub initial_dec_deg: f64,
    pub initial_rotation_deg: f64,
    pub follow: Option<TelescopeFollow>,
    pub survey_endpoint: String,
    pub survey_name: String,
    pub cache_dir: PathBuf,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct TelescopeFollow {
    pub alpaca_url: String,
    pub device_number: u32,
    pub offset_ra_arcsec: f64,
    pub offset_dec_arcsec: f64,
    pub request_timeout: Duration,
}

/// Fluent builder for [`SkySurveyCameraConfig`].
#[derive(Debug, Clone)]
pub struct SkySurveyCameraConfigBuilder {
    inner: SkySurveyCameraConfig,
}

impl SkySurveyCameraConfigBuilder {
    pub fn new(survey_endpoint: impl Into<String>) -> Self {
        Self {
            inner: SkySurveyCameraConfig {
                device_name: "Sky Survey Camera".into(),
                unique_id: "sky-survey-camera-bdd".into(),
                focal_length_mm: 1000.0,
                pixel_size_um: 3.76,
                sensor_width_px: 64,
                sensor_height_px: 48,
                initial_ra_deg: 83.8221,
                initial_dec_deg: -5.3911,
                initial_rotation_deg: 0.0,
                follow: None,
                survey_endpoint: survey_endpoint.into(),
                survey_name: "DSS2 Red".into(),
                // Populated by `start_sky_survey_camera` from a
                // freshly-created `TempDir` so the cache directory is
                // RAII-cleaned at scenario teardown — see the function
                // doc for why this lives at the launcher rather than
                // the builder.
                cache_dir: PathBuf::new(),
                port: 0,
            },
        }
    }

    #[must_use]
    pub fn with_follow(mut self, follow: TelescopeFollow) -> Self {
        self.inner.follow = Some(follow);
        self
    }

    #[must_use]
    pub const fn with_sensor(mut self, width_px: u32, height_px: u32) -> Self {
        self.inner.sensor_width_px = width_px;
        self.inner.sensor_height_px = height_px;
        self
    }

    #[must_use]
    pub const fn with_initial_pointing(mut self, ra_deg: f64, dec_deg: f64) -> Self {
        self.inner.initial_ra_deg = ra_deg;
        self.inner.initial_dec_deg = dec_deg;
        self
    }

    #[must_use]
    pub fn build(self) -> SkySurveyCameraConfig {
        self.inner
    }
}

impl SkySurveyCameraConfig {
    /// Serialize into the JSON shape `sky-survey-camera`'s config
    /// loader expects.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut pointing = serde_json::json!({
            "initial_ra_deg": self.initial_ra_deg,
            "initial_dec_deg": self.initial_dec_deg,
            "initial_rotation_deg": self.initial_rotation_deg,
            "telescope": Value::Null,
        });
        if let Some(f) = &self.follow {
            // `pointing` is a `json!({...})` object literal, so
            // `as_object_mut` always succeeds.
            if let Some(map) = pointing.as_object_mut() {
                map.insert(
                    "telescope".to_owned(),
                    serde_json::json!({
                        "alpaca_url": f.alpaca_url,
                        "device_number": f.device_number,
                        "offset_ra_arcsec": f.offset_ra_arcsec,
                        "offset_dec_arcsec": f.offset_dec_arcsec,
                        "request_timeout": format!("{}ms", f.request_timeout.as_millis()),
                        "auth": Value::Null,
                    }),
                );
            }
        }
        serde_json::json!({
            "device": {
                "name": self.device_name,
                "unique_id": self.unique_id,
                "description": "BDD-managed sky-survey-camera",
            },
            "optics": {
                "focal_length_mm": self.focal_length_mm,
                "pixel_size_x_um": self.pixel_size_um,
                "pixel_size_y_um": self.pixel_size_um,
                "sensor_width_px": self.sensor_width_px,
                "sensor_height_px": self.sensor_height_px,
            },
            "pointing": pointing,
            "survey": {
                "name": self.survey_name,
                "request_timeout": "30s",
                "cache_dir": self.cache_dir.to_string_lossy(),
                "endpoint": self.survey_endpoint,
            },
            "server": { "port": self.port },
        })
    }
}

/// Spawn `sky-survey-camera` with the given config and wait for it to
/// announce its bound port.
///
/// Returns a `(ServiceHandle, TempDir)` pair. The `ServiceHandle`'s
/// `base_url` is suitable for an `add_camera` entry in
/// [`crate::rp_harness::RpConfigBuilder`]; the `TempDir` is the
/// camera's cache directory and **must be kept alive for the
/// camera's lifetime** — its `Drop` impl removes the directory, so
/// dropping it before stopping the camera would yank the cache out
/// from under an in-flight exposure. Callers that store both on a
/// scenario-scoped world struct should declare the `ServiceHandle`
/// field *before* the `TempDir` field so Rust's struct-drop order
/// (top-down) tears down the camera process first, then removes the
/// cache directory.
///
/// The launcher owns cache-directory creation rather than the
/// builder so the cleanup `TempDir` guard can be returned alongside
/// the spawned process — there's no clean way to thread that out of
/// a chained builder API.
pub async fn start_sky_survey_camera(
    config: &SkySurveyCameraConfig,
) -> (ServiceHandle, tempfile::TempDir) {
    let cache = tempfile::Builder::new()
        .prefix("sky-survey-cache-")
        .tempdir()
        .expect("failed to create sky-survey-camera cache TempDir");
    let mut config = config.clone();
    config.cache_dir = cache.path().to_path_buf();
    let path = write_temp_config_file("sky-survey-camera-bdd-config", &config.to_json()).await;
    let handle = ServiceHandle::start("sky-survey-camera", &path).await;
    (handle, cache)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skyview_stub_returns_fits_with_position_crval() {
        let stub = SkyViewStub::start().await;
        let url = format!("{}?Position=10.5,-30.0&Pixels=16,16&Size=0.1,0.1", stub.url);
        let bytes = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(bytes.starts_with(b"SIMPLE"), "expected FITS preamble");

        use rp_fits::reader::read_primary_keyword;
        use rp_fits::writer::KeywordValue;
        let crval1 = read_primary_keyword(std::io::Cursor::new(&bytes), "CRVAL1")
            .unwrap()
            .unwrap();
        let crval2 = read_primary_keyword(std::io::Cursor::new(&bytes), "CRVAL2")
            .unwrap()
            .unwrap();
        match (crval1, crval2) {
            (KeywordValue::Float(ra), KeywordValue::Float(dec)) => {
                assert!((ra - 10.5).abs() < 1e-6);
                assert!((dec - -30.0).abs() < 1e-6);
            }
            other => panic!("expected float CRVAL1/2, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skyview_stub_responds_to_head() {
        let stub = SkyViewStub::start().await;
        let resp = reqwest::Client::new().head(&stub.url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[test]
    fn config_builder_emits_telescope_block_only_when_set() {
        let cfg_static = SkySurveyCameraConfigBuilder::new("http://stub/").build();
        let json_static = cfg_static.to_json();
        assert!(json_static["pointing"]["telescope"].is_null());

        let cfg_follow = SkySurveyCameraConfigBuilder::new("http://stub/")
            .with_follow(TelescopeFollow {
                alpaca_url: "http://127.0.0.1:32323".into(),
                device_number: 0,
                offset_ra_arcsec: 60.0,
                offset_dec_arcsec: -45.0,
                request_timeout: Duration::from_secs(2),
            })
            .build();
        let json_follow = cfg_follow.to_json();
        assert_eq!(
            json_follow["pointing"]["telescope"]["alpaca_url"],
            "http://127.0.0.1:32323"
        );
        assert_eq!(
            json_follow["pointing"]["telescope"]["offset_ra_arcsec"],
            60.0
        );
        assert_eq!(
            json_follow["pointing"]["telescope"]["offset_dec_arcsec"],
            -45.0
        );
        assert_eq!(
            json_follow["pointing"]["telescope"]["request_timeout"],
            "2000ms"
        );
    }
}

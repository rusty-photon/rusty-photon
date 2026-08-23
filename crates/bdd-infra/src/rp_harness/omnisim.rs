//! `OmniSim` (ASCOM Alpaca simulator) process management for BDD tests.
//!
//! A single `OmniSim` process is shared across all scenarios in a test binary
//! via a [`tokio::sync::OnceCell`]. Each test process spawns its **own**
//! instance on a **dynamically chosen port**, passing `--multi-instance` —
//! the flag added to our `OmniSim` fork (`ivonnyssen/ASCOM.Alpaca.Simulators`,
//! release `v0.5.0-467.1`) that skips upstream's machine-global
//! single-instance guard (a named Mutex keyed on a fixed GUID, backed by a
//! file under `/tmp/.dotnet/shm/` on Unix). Combined with a per-instance
//! settings dir (see [`OmniSimProcess::prepare_settings_dir`]), any number
//! of BDD test processes — parallel Bazel targets, shards of one target, or a
//! stray dev instance on the default port — can run concurrently without
//! contending for one simulator. This is what lets Bazel run the
//! OmniSim-backed suites in parallel and shard `rp:bdd` (issue #467).
//!
//! The settings dir is passed via `OMNISIM_SETTINGS_DIR` (fork release
//! `v0.5.0-467.2`, the version floor), NOT `XDG_CONFIG_HOME`: .NET honors
//! XDG only on non-macOS Unix, so on macOS `OmniSim`'s profile store defaults
//! to the shared `~/Library/Application Support` (and on Windows to
//! `%USERPROFILE%\.ASCOM`), neither redirectable by any environment
//! variable. Concurrent instances sharing one profile store race their
//! startup write-backs and leak persisted *settings* across suites — on
//! macOS CI, session-runner's computed telescope site leaked into rp's
//! shards through per-scenario `restart` (which reloads from the profile)
//! and rp refused to start on mount-site validation. The fork's env var
//! bypasses the platform lookup entirely, so isolation is deterministic on
//! every OS and the Bazel `omnisim` pool runs parallel everywhere.
//!
//! Two more spawn-time defences keep a host's environment from turning
//! into a wall of downstream assertion failures: the .NET culture is
//! pinned (`DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1`) so the seeded
//! profile parses the same on a comma-decimal host as on an `en_US`
//! runner, and a freshly healthy instance is gated on the device roster
//! it advertises — see [`OmniSimProcess::command`] and
//! [`OmniSimProcess::check_device_roster`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::sync::OnceCell;

/// Attempts to spawn `OmniSim` before giving up. Each attempt picks a fresh
/// ephemeral port, so a lost bind race (another process grabbed the port
/// between our probe and `OmniSim`'s bind) just costs one retry.
const SPAWN_ATTEMPTS: u32 = 3;

/// Shared `OmniSim` info returned to each scenario.
#[derive(Debug, Clone)]
pub struct OmniSimHandle {
    pub base_url: String,
    pub port: u16,
}

/// Singleton that owns the `OmniSim` child process for the entire test run.
#[derive(Debug)]
struct OmniSimProcess {
    _child: std::process::Child,
    base_url: String,
    port: u16,
}

/// Global singleton — one `OmniSim` process shared by all scenarios.
static OMNISIM: OnceCell<OmniSimProcess> = OnceCell::const_new();

/// Process-wide serialization of `/restart` PUTs. `OmniSim`'s restart
/// handler (`DriverManager.Load{Class}(n)`) mutates unsynchronised
/// process-wide static state, so concurrent restarts race inside the
/// simulator. `reset_all_devices` already issues its per-device PUTs
/// sequentially (#171), but that only serialises *within* one hook —
/// cucumber runs untagged scenarios concurrently, and every
/// concurrently-drawn scenario runs its own before-hook. In the
/// pi-nightly failure behind #431 the 11 non-`@serial` rp scenarios
/// were all drawn at once after the `@serial` queue drained, their 11
/// hooks issued ~11 concurrent restarts per device class, and `OmniSim`
/// deadlocked mid-wave (log torn then silent, no stderr, subsequent
/// PUTs timing out) — failing the remaining hooks loud. Holding this
/// mutex across each PUT caps in-flight restarts at one per test
/// process. A process-wide mutex is also *sufficient* now: every test
/// process owns a private `OmniSim` instance (`--multi-instance` +
/// dynamic port), so no other process can send restarts to ours — the
/// old cross-process caveat about two Bazel actions sharing one
/// `OmniSim` on port 32323 no longer applies.
static RESTART_SERIALIZER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl OmniSimHandle {
    /// Get or start this process's private `OmniSim`. Returns a lightweight
    /// handle.
    ///
    /// The first call spawns a fresh instance with `--multi-instance` on a
    /// dynamically chosen `127.0.0.1` port (with `PR_SET_PDEATHSIG` on Linux
    /// so the kernel kills it when the test process exits); subsequent calls
    /// share it. A pre-existing `OmniSim` — a dev instance on the default port,
    /// or another test process's instance — is never reused: private
    /// instances are what allow OmniSim-backed suites and shards to run
    /// concurrently, and what stopped cross-session dev instances from
    /// contending with test runs.
    ///
    /// Binary discovery order:
    /// 1. `OMNISIM_PATH` env var — full path to the binary
    /// 2. `OMNISIM_DIR` env var — directory containing the binary
    /// 3. `ascom.alpaca.simulators` on `PATH`
    ///
    /// The binary must support `--multi-instance` and `OMNISIM_SETTINGS_DIR`
    /// (fork release `v0.5.0-467.2` or newer). An older binary either exits
    /// immediately when another instance is running (pre-467.1, surfaces
    /// here as a spawn failure naming the flag) or silently ignores the
    /// settings-dir override and shares the platform-default profile store
    /// with every other instance (467.1 on macOS/Windows).
    pub async fn start() -> Self {
        let process = OMNISIM
            .get_or_init(|| async { OmniSimProcess::get_or_spawn().await })
            .await;
        Self {
            base_url: process.base_url.clone(),
            port: process.port,
        }
    }

    /// Reset the telescope simulator device 0 to its `OmniSim` default state.
    /// See [`Self::restart_device`] for the underlying mechanism.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_telescope() -> Result<(), String> {
        Self::restart_device("telescope", 0).await
    }

    /// Reset the camera simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_camera() -> Result<(), String> {
        Self::restart_device("camera", 0).await
    }

    /// Reset the filter-wheel simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_filter_wheel() -> Result<(), String> {
        Self::restart_device("filterwheel", 0).await
    }

    /// Reset the focuser simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_focuser() -> Result<(), String> {
        Self::restart_device("focuser", 0).await
    }

    /// Reset the cover-calibrator simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_cover_calibrator() -> Result<(), String> {
        Self::restart_device("covercalibrator", 0).await
    }

    /// Reset the safety-monitor simulator device 0 to its `OmniSim` default
    /// state. `restart` reloads the device from its persisted profile, and
    /// [`Self::set_safety_monitor_is_safe`] writes only the in-memory
    /// setting — so this restores the profile's `IsSafe` (true in our
    /// seeded config) after a safety scenario flipped it.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_safety_monitor() -> Result<(), String> {
        Self::restart_device("safetymonitor", 0).await
    }

    /// Reset the switch simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_switch() -> Result<(), String> {
        Self::restart_device("switch", 0).await
    }

    /// Reset the rotator simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_rotator() -> Result<(), String> {
        Self::restart_device("rotator", 0).await
    }

    /// Reset the observing-conditions simulator device 0 to its `OmniSim`
    /// default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_observing_conditions() -> Result<(), String> {
        Self::restart_device("observingconditions", 0).await
    }

    /// Reset the dome simulator device 0 to its `OmniSim` default state.
    ///
    /// # Errors
    ///
    /// Returns the failure message from [`Self::restart_device`].
    pub async fn reset_dome() -> Result<(), String> {
        Self::restart_device("dome", 0).await
    }

    /// Reset every device class our BDD suites currently exercise
    /// (telescope, camera, filter wheel, focuser, cover calibrator,
    /// safety monitor, switch, rotator, observing conditions, dome) to
    /// `OmniSim` defaults. Issued **sequentially** — one PUT at a time.
    ///
    /// Why not parallel? `OmniSim`'s `DriverManager.Load{Class}(n)`
    /// mutates a process-wide `static List<AlpacaConfiguredDevice>
    /// AlpacaDevices` via unsynchronised `List.Remove(...)` +
    /// `List.Add(...)`. When two of our PUTs landed on different
    /// Kestrel threads they raced inside that list, leaving a `null`
    /// entry that the management endpoint then serialised verbatim
    /// into `configureddevices` responses. rp's deserialiser hit
    /// `invalid type: null, expected struct ConfiguredDevice` and
    /// silently registered the device as disconnected — which is the
    /// camera/calibrator/focuser "not connected" cascade in #171.
    /// Sequential PUTs eliminate that race *within* one hook; the
    /// process-wide [`RESTART_SERIALIZER`] taken inside each PUT
    /// eliminates it *across* concurrently-running hooks too — the
    /// end-of-run burst of non-`@serial` scenarios deadlocked `OmniSim`
    /// on the Pi nightly (#431).
    ///
    /// The wall-time cost is small: 6 localhost round-trips serialised
    /// is ~10-30 ms per scenario depending on runner.
    ///
    /// When the shared `OMNISIM` singleton has not been initialised yet
    /// (no scenario has gone through `OmniSimHandle::start()`), this is
    /// a no-op: there is no instance to reset, and the one `start()`
    /// will eventually spawn is fresh by construction. (The pre-#467
    /// behaviour of firing best-effort PUTs at the default port to
    /// scrub a reusable dev instance is gone along with reuse itself.)
    /// Once the suite has called `OmniSimHandle::start()`, every reset
    /// failure is fatal — that's the loud-reset behaviour from #172
    /// that catches state leakage between scenarios.
    ///
    /// # Errors
    ///
    /// Returns every failed reset's message, in device order, when at
    /// least one restart fails; the remaining resets still run.
    pub async fn reset_all_devices() -> Result<(), Vec<String>> {
        if OMNISIM.get().is_none() {
            return Ok(());
        }
        let mut errors: Vec<String> = Vec::new();
        let results = [
            Self::reset_telescope().await,
            Self::reset_camera().await,
            Self::reset_filter_wheel().await,
            Self::reset_focuser().await,
            Self::reset_cover_calibrator().await,
            Self::reset_safety_monitor().await,
            Self::reset_switch().await,
            Self::reset_rotator().await,
            Self::reset_observing_conditions().await,
            Self::reset_dome().await,
        ];
        for result in results {
            if let Err(e) = result {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Reset a single `OmniSim` device by class and instance number to
    /// its default state without restarting the simulator process.
    ///
    /// Posts to `OmniSim`'s private `PUT /simulator/v1/{class}/{n}/restart`
    /// endpoint, which calls `DriverManager.Load{Class}(n)` server-side.
    /// The result is equivalent to `OmniSim` having just started for that
    /// device — e.g. for telescope: `AtPark` false, Tracking false,
    /// position at the configured startup alt/az (default ≈ alt 38.9°
    /// az 165° — above horizon).
    ///
    /// `class` must match one of `OmniSim`'s device class slugs:
    /// `telescope`, `camera`, `covercalibrator`, `dome`, `filterwheel`,
    /// `focuser`, `observingconditions`, `rotator`, `safetymonitor`,
    /// `switch`.
    ///
    /// # Errors
    ///
    /// Returns a message, suitable for inclusion in a panic message, if
    /// the PUT cannot be built or sent or answers a non-success HTTP
    /// status. The endpoint is OmniSim-only (not part of standard
    /// Alpaca), so older or alternative simulators may 404 — those are
    /// surfaced as errors; we run only against `OmniSim` and want to know
    /// if that ever changes. They used to be silently swallowed here,
    /// which masked intermittent macOS failures.
    pub async fn restart_device(class: &str, n: u32) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        Self::restart_device_at(&base_url, class, n).await
    }

    /// Set what the safety-monitor simulator device 0 reports for
    /// `IsSafe`, via `OmniSim`'s private
    /// `PUT /simulator/v1/safetymonitor/{n}/issafesetting` endpoint.
    ///
    /// This writes the device's in-memory setting only (`OmniSim` persists
    /// it to the profile on its own save path, which this endpoint does
    /// not trigger), so [`Self::reset_safety_monitor`] — or the next
    /// process restart — restores the profile default (safe). Safety
    /// scenarios still set `true` explicitly during setup so they never
    /// depend on reset ordering.
    ///
    /// # Errors
    ///
    /// Returns a message if the PUT cannot be built or sent or answers a
    /// non-success HTTP status.
    pub async fn set_safety_monitor_is_safe(is_safe: bool) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        Self::set_safety_monitor_is_safe_at(&base_url, 0, is_safe).await
    }

    /// Drive the cover-calibrator simulator's cover fully open or
    /// closed via the standard Alpaca device API (`opencover` /
    /// `closecover`, then `coverstate` polling; `CoverState` 1 = Closed,
    /// 3 = Open). Connects the device first — the scenario's `Given`
    /// runs before rp does. `OmniSim`'s cover sweep takes a few seconds;
    /// polls up to 30 s.
    ///
    /// # Errors
    ///
    /// Returns a message if the connect or cover PUT cannot be built or
    /// sent, answers a non-success HTTP status or an unparseable body, or
    /// reports a non-zero ASCOM `ErrorNumber`; if a `coverstate` poll
    /// fails (see [`Self::cover_state`]); or if the cover has not reached
    /// the target state after 30 s.
    pub async fn set_cover_closed(closed: bool) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;

        for (verb, form) in [
            ("connected", ("Connected", "true")),
            (
                if closed { "closecover" } else { "opencover" },
                ("ClientID", "77"),
            ),
        ] {
            let url = format!("{base_url}/api/v1/covercalibrator/0/{verb}");
            let resp = client
                .put(&url)
                .form(&[form])
                .send()
                .await
                .map_err(|e| format!("PUT {url} failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("PUT {url} returned HTTP {}", resp.status()));
            }
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("PUT {url}: unparseable body: {e}"))?;
            let error_number = body
                .get("ErrorNumber")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            if error_number != 0 {
                return Err(format!(
                    "PUT {url}: ASCOM error {error_number}: {}",
                    body.get("ErrorMessage").unwrap_or(&serde_json::Value::Null)
                ));
            }
        }

        let target = if closed { 1 } else { 3 };
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if Self::cover_state().await? == target {
                return Ok(());
            }
        }
        Err(format!(
            "cover did not reach state {target} within 30s of {}",
            if closed { "closecover" } else { "opencover" }
        ))
    }

    /// The cover-calibrator simulator's Alpaca `CoverState` (1 =
    /// Closed, 2 = Moving, 3 = Open).
    ///
    /// # Errors
    ///
    /// Returns a message if the GET fails in transport, its body is not
    /// JSON, or the body has no integer `Value`.
    pub async fn cover_state() -> Result<i64, String> {
        let base_url = Self::singleton_base_url().await;
        let url = format!(
            "{base_url}/api/v1/covercalibrator/0/coverstate?ClientID=77&ClientTransactionID=1"
        );
        let body: serde_json::Value = reqwest::get(&url)
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("GET {url}: unparseable body: {e}"))?;
        body.get("Value")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| format!("GET {url}: no integer Value in {body}"))
    }

    /// `set_safety_monitor_is_safe` extracted to take an explicit
    /// `base_url` and device number so unit tests can drive the HTTP
    /// path against an axum stub without touching the global `OMNISIM`
    /// singleton.
    async fn set_safety_monitor_is_safe_at(
        base_url: &str,
        n: u32,
        is_safe: bool,
    ) -> Result<(), String> {
        let url = format!("{base_url}/simulator/v1/safetymonitor/{n}/issafesetting");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        let resp = client
            .put(&url)
            .form(&[("IsSafeSetting", if is_safe { "true" } else { "false" })])
            .send()
            .await
            .map_err(|e| format!("PUT {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("PUT {url} returned HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Set the telescope simulator's observer site (`SiteLatitude` /
    /// `SiteLongitude`, standard Alpaca telescope properties). rp
    /// hard-errors on mount connect when its configured `site` differs
    /// from the mount's reported site by more than 0.01° (rp.md § Site
    /// Validation Against the ASCOM Mount), so scenarios that compute a
    /// site at runtime must teach the simulated mount the same one
    /// before rp starts.
    ///
    /// **The write outlives the scenario.** `OmniSim` treats the site as
    /// a profile *setting*, not runtime state: the per-scenario
    /// `restart` does NOT restore the default (unlike tracking or the
    /// mount position), and on platforms without `PR_SET_PDEATHSIG`
    /// (macOS, Windows) the `OmniSim` process itself outlives the test
    /// binary, so a leaked site poisons the *next* suite that reuses
    /// the instance — rp's planner scenarios pin their config to
    /// `OmniSim`'s default site and fail mount-site validation against a
    /// leftover computed one. Scenarios that call this must capture
    /// the prior site via [`Self::get_telescope_site`] and restore it
    /// when they finish.
    ///
    /// # Errors
    ///
    /// Returns a message if either property PUT cannot be built or sent,
    /// answers a non-success HTTP status or a body that is not an Alpaca
    /// response, or reports a non-zero Alpaca `ErrorNumber`.
    pub async fn set_telescope_site(
        latitude_degrees: f64,
        longitude_degrees: f64,
    ) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        Self::put_telescope_form_at(
            &base_url,
            0,
            "sitelatitude",
            &[("SiteLatitude", format!("{latitude_degrees}"))],
        )
        .await?;
        Self::put_telescope_form_at(
            &base_url,
            0,
            "sitelongitude",
            &[("SiteLongitude", format!("{longitude_degrees}"))],
        )
        .await
    }

    /// Read the telescope simulator's observer site — the capture half
    /// of the capture/restore contract on [`Self::set_telescope_site`].
    ///
    /// # Errors
    ///
    /// Returns a message if either property GET cannot be built or sent,
    /// answers a non-success HTTP status or a body that is not an Alpaca
    /// response, reports a non-zero Alpaca `ErrorNumber`, or carries no
    /// numeric `Value`.
    pub async fn get_telescope_site() -> Result<(f64, f64), String> {
        let base_url = Self::singleton_base_url().await;
        let lat = Self::get_telescope_number_at(&base_url, 0, "sitelatitude").await?;
        let lon = Self::get_telescope_number_at(&base_url, 0, "sitelongitude").await?;
        Ok((lat, lon))
    }

    /// One GET against the standard Alpaca telescope API, returning the
    /// numeric `Value` and checking both the HTTP status and the Alpaca
    /// `ErrorNumber`.
    async fn get_telescope_number_at(
        base_url: &str,
        n: u32,
        property: &str,
    ) -> Result<f64, String> {
        let url = format!("{base_url}/api/v1/telescope/{n}/{property}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url} returned HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("GET {url} returned a non-JSON body: {e}"))?;
        // A response without a numeric ErrorNumber is not an Alpaca
        // response at all (wrong port, proxy error page, …) — reject
        // it rather than treating it as success.
        let error_number = body
            .get("ErrorNumber")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                format!("GET {url} returned a body without a numeric ErrorNumber: {body}")
            })?;
        if error_number != 0 {
            let message = body
                .get("ErrorMessage")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(format!(
                "GET {url} returned Alpaca error {error_number}: {message}"
            ));
        }
        body.get("Value")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| format!("GET {url} returned no numeric Value: {body}"))
    }

    /// Enable or disable the telescope simulator's sidereal tracking
    /// (`Tracking`, standard Alpaca). `OmniSim` requires tracking to be
    /// on before `SyncToCoordinates` — call this before
    /// [`Self::sync_telescope_to`].
    ///
    /// # Errors
    ///
    /// Returns a message if the PUT cannot be built or sent, answers a
    /// non-success HTTP status or a body that is not an Alpaca response,
    /// or reports a non-zero Alpaca `ErrorNumber`.
    pub async fn set_telescope_tracking(enabled: bool) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        Self::put_telescope_form_at(
            &base_url,
            0,
            "tracking",
            &[(
                "Tracking",
                if enabled { "true" } else { "false" }.to_string(),
            )],
        )
        .await
    }

    /// Sync the telescope simulator to equatorial coordinates
    /// (`SyncToCoordinates`, standard Alpaca): teleports the mount's
    /// coordinate frame without physical motion, so a scenario can
    /// start a session with the mount already "pointing" near its
    /// target and every document slew stays sub-degree (`OmniSim` slews
    /// at real-mount speed — a tens-of-degrees slew costs minutes).
    /// Requires tracking on (OmniSim-imposed; see
    /// [`Self::set_telescope_tracking`]).
    ///
    /// # Errors
    ///
    /// Returns a message if the PUT cannot be built or sent, answers a
    /// non-success HTTP status or a body that is not an Alpaca response,
    /// or reports a non-zero Alpaca `ErrorNumber` — which is how
    /// `OmniSim` refuses a sync with tracking off.
    pub async fn sync_telescope_to(ra_hours: f64, dec_degrees: f64) -> Result<(), String> {
        let base_url = Self::singleton_base_url().await;
        Self::put_telescope_form_at(
            &base_url,
            0,
            "synctocoordinates",
            &[
                ("RightAscension", format!("{ra_hours}")),
                ("Declination", format!("{dec_degrees}")),
            ],
        )
        .await
    }

    /// The shared singleton's base URL, starting this process's `OmniSim`
    /// first if no scenario has done so yet. There is no fixed fallback
    /// port anymore — with per-process instances on dynamic ports, "the"
    /// `OmniSim` is always the one this process owns, so the state-arranging
    /// helpers (`restart_device`, the telescope-site/tracking/sync setters,
    /// the safety-monitor override) simply ensure it exists.
    async fn singleton_base_url() -> String {
        Self::start().await.base_url
    }

    /// One form-encoded PUT against the standard Alpaca telescope API
    /// (`/api/v1/telescope/{n}/{property}`), checking both the HTTP
    /// status and the Alpaca `ErrorNumber` in the response body — an
    /// Alpaca-level refusal (e.g. syncing with tracking off) arrives
    /// as HTTP 200 with a non-zero `ErrorNumber`.
    async fn put_telescope_form_at(
        base_url: &str,
        n: u32,
        property: &str,
        form: &[(&str, String)],
    ) -> Result<(), String> {
        let url = format!("{base_url}/api/v1/telescope/{n}/{property}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        let resp = client
            .put(&url)
            .form(form)
            .send()
            .await
            .map_err(|e| format!("PUT {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("PUT {url} returned HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("PUT {url} returned a non-JSON body: {e}"))?;
        // A response without a numeric ErrorNumber is not an Alpaca
        // response at all (wrong port, proxy error page, …) — reject
        // it rather than treating it as success.
        let error_number = body
            .get("ErrorNumber")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| {
                format!("PUT {url} returned a body without a numeric ErrorNumber: {body}")
            })?;
        if error_number != 0 {
            let message = body
                .get("ErrorMessage")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(format!(
                "PUT {url} returned Alpaca error {error_number}: {message}"
            ));
        }
        Ok(())
    }

    /// `restart_device` extracted to take an explicit `base_url` so unit
    /// tests can drive the HTTP path against an axum stub without
    /// touching the global `OMNISIM` singleton. See the `tests` module
    /// at the bottom of this file.
    ///
    /// The PUT is issued under [`RESTART_SERIALIZER`], so at most one
    /// restart is in flight per test process no matter how many
    /// scenario hooks run concurrently — see the mutex docs for the
    /// `OmniSim` deadlock (#431) this prevents.
    async fn restart_device_at(base_url: &str, class: &str, n: u32) -> Result<(), String> {
        let url = format!("{base_url}/simulator/v1/{class}/{n}/restart");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        // Lock only around the request itself — client construction and
        // URL formatting don't touch OmniSim and would just lengthen the
        // critical section when many hooks queue here.
        let _serialized = RESTART_SERIALIZER.lock().await;
        let resp = client
            .put(&url)
            .send()
            .await
            .map_err(|e| format!("PUT {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("PUT {url} returned HTTP {}", resp.status()));
        }
        Ok(())
    }
}

/// Why one [`OmniSimProcess::spawn_on_port`] attempt failed — decides
/// whether [`OmniSimProcess::get_or_spawn`] tries again on a fresh port
/// or gives up on the spot.
#[derive(Debug)]
enum SpawnFailure {
    /// The child never came up (lost the port-bind race, pre-fork
    /// binary, unhealthy past the window), or a probe of it failed in
    /// transit. A fresh port may fix it.
    Retry(String),
    /// The child came up but its device roster is unusable (see
    /// [`OmniSimProcess::check_device_roster`]). That is deterministic
    /// — no port changes it — so retrying would only bury the
    /// diagnostic under two more copies of the same failure.
    Fatal(String),
}

impl OmniSimProcess {
    async fn get_or_spawn() -> Self {
        let binary = Self::find_binary();
        let mut last_failure = String::new();
        for _ in 0..SPAWN_ATTEMPTS {
            let port = Self::pick_free_port();
            match Self::spawn_on_port(&binary, port).await {
                Ok(process) => return process,
                Err(SpawnFailure::Retry(failure)) => last_failure = failure,
                Err(SpawnFailure::Fatal(diagnostic)) => panic!("{diagnostic}"),
            }
        }
        panic!(
            "failed to start OmniSim binary '{binary}' after {SPAWN_ATTEMPTS} attempts: {last_failure}. \
             Note: bdd-infra spawns OmniSim with --multi-instance and \
             OMNISIM_SETTINGS_DIR, which need the patched fork \
             (ivonnyssen/ASCOM.Alpaca.Simulators release v0.5.0-467.2 or \
             newer) — an older binary exits at startup when any other \
             OmniSim instance is running on the host."
        );
    }

    /// One spawn attempt: launch `OmniSim` on `port`, wait for it to become
    /// healthy, then check that the device roster it advertises is one rp
    /// can actually consume. `Retry` when the child exits early (lost the
    /// port-bind race, or the binary predates `--multi-instance`) or never
    /// turns healthy — the caller retries on a fresh port; `Fatal` when it
    /// came up with an unusable roster.
    async fn spawn_on_port(binary: &str, port: u16) -> Result<Self, SpawnFailure> {
        let base_url = format!("http://127.0.0.1:{port}");

        // Capture OmniSim's stdout/stderr to per-run log files under the
        // cargo target tree. The previous `Stdio::null()` dropped every
        // line OmniSim emitted, which left CI failures with no insight
        // into what the simulator was doing — see #171 for the
        // diagnostic gap. Failures here fall back to `Stdio::null` so a
        // log-write problem can't stop the test suite from running.
        let (stdout_target, stderr_target) = Self::open_log_files(port);

        let mut cmd = Self::command(binary, &base_url, Self::prepare_settings_dir());
        cmd.stdout(stdout_target).stderr(stderr_target);

        let mut child = cmd
            .spawn()
            .map_err(|e| SpawnFailure::Retry(format!("spawn failed: {e}")))?;

        let outcome = match Self::wait_healthy(&mut child, &base_url).await {
            Ok(()) => Self::check_device_roster(&base_url, port).await,
            Err(e) => Err(SpawnFailure::Retry(e)),
        };
        match outcome {
            Ok(()) => Ok(Self {
                _child: child,
                base_url,
                port,
            }),
            Err(failure) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(failure)
            }
        }
    }

    /// Build the `OmniSim` command line and environment: everything about
    /// the spawn except where its stdout/stderr go.
    ///
    /// `--multi-instance` (our fork's flag) skips `OmniSim`'s machine-global
    /// single-instance guard; `--urls` pins the Kestrel listener to the
    /// port we probed as free. The sanitizer-related env vars are cleared
    /// so the .NET runtime isn't broken by `LD_PRELOAD` injection from
    /// ASAN/LSAN.
    ///
    /// `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1` pins the .NET runtime to
    /// the invariant culture. `OmniSim` parses its profile values with the
    /// *current* culture, so on a host whose region uses a comma decimal
    /// separator a seed value like `0.6` fails to parse, the device's
    /// constructor throws, and `OmniSim` advertises the device **without a
    /// `UniqueID`** — which rp's discovery requires on every entry, so
    /// nothing on that instance connects. The runtime knob is the one pin
    /// that holds on every platform: .NET on Windows reads the user's
    /// regional settings and ignores `LANG`/`LC_ALL`, and on macOS ICU can
    /// fall back to the system region. (Invariant mode also stops `OmniSim`
    /// depending on `libicu` being installed.)
    ///
    /// `settings_dir` becomes the per-instance `OMNISIM_SETTINGS_DIR`:
    /// concurrent `OmniSim`s must not share a writable profile store (see
    /// [`Self::prepare_settings_dir`], which panics rather than degrade to
    /// the shared platform default). The fork's `OMNISIM_SETTINGS_DIR`
    /// (467.2) re-roots the profile store on every platform —
    /// `XDG_CONFIG_HOME` would cover Linux only (.NET ignores it on macOS,
    /// and Windows never honored it).
    fn command(binary: &str, base_url: &str, settings_dir: PathBuf) -> std::process::Command {
        let mut cmd = std::process::Command::new(binary);
        cmd.arg("--multi-instance")
            .arg(format!("--urls={base_url}"))
            .env_remove("LD_PRELOAD")
            .env_remove("ASAN_OPTIONS")
            .env_remove("LSAN_OPTIONS")
            .env("DOTNET_SYSTEM_GLOBALIZATION_INVARIANT", "1")
            .env("OMNISIM_SETTINGS_DIR", settings_dir);

        // On Linux, set PR_SET_PDEATHSIG so the kernel will SIGKILL this
        // child when the test process exits (normal, panic, or SIGKILL).
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
        }
        cmd
    }

    /// Gate a freshly healthy `OmniSim` on the device roster it advertises:
    /// every entry of `GET /management/v1/configureddevices` must be a
    /// device object carrying a non-empty `UniqueID`.
    ///
    /// rp discovers devices through `ascom-alpaca`'s `get_devices`, which
    /// deserialises the whole roster at once and requires `UniqueID` on
    /// every entry — so **one** bad entry fails discovery for **every**
    /// device on the server, and the suites then fail on symptoms several
    /// layers downstream ("mount not connected", "expected 4
    /// `exposure_complete` events", a 900 s suite timeout) that name
    /// neither the entry nor the reason `OmniSim` dropped its ID.
    /// `OmniSim` drops the ID when a device's constructor throws at
    /// startup, and it can serialise a `null` entry when its device list
    /// is mutated concurrently. Failing here instead names the entry and
    /// quotes the startup exception from `OmniSim`'s log.
    ///
    /// A transport or JSON failure on the probe itself is `Retry` — the
    /// health probe just answered, so that is a fresh anomaly rather than
    /// a verdict on the roster.
    async fn check_device_roster(base_url: &str, port: u16) -> Result<(), SpawnFailure> {
        let roster = Self::fetch_device_roster(base_url)
            .await
            .map_err(SpawnFailure::Retry)?;
        let offenders = Self::devices_without_unique_id(&roster);
        if offenders.is_empty() {
            return Ok(());
        }
        Err(SpawnFailure::Fatal(Self::roster_diagnostic(
            base_url, &offenders, port,
        )))
    }

    /// `GET /management/v1/configureddevices` on `base_url`, returned as the
    /// raw JSON body so a malformed entry can be reported rather than
    /// rejected by a typed deserialiser.
    async fn fetch_device_roster(base_url: &str) -> Result<serde_json::Value, String> {
        let url = format!("{base_url}/management/v1/configureddevices");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url} returned HTTP {}", resp.status()));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| format!("GET {url} returned a non-JSON body: {e}"))
    }

    /// Name every entry of a `configureddevices` body that rp's discovery
    /// would choke on: entries that are not device objects (`OmniSim` can
    /// emit `null` there) and devices whose `UniqueID` is missing, not a
    /// string, or empty. Empty when the roster is fully usable. A body
    /// without a `Value` array is reported as one offender, since rp
    /// could not have used it either — quoting a bounded excerpt of the
    /// body, so an unexpectedly large reply can't swamp the panic text.
    fn devices_without_unique_id(body: &serde_json::Value) -> Vec<String> {
        const BODY_EXCERPT_CHARS: usize = 200;
        let Some(entries) = body.get("Value").and_then(serde_json::Value::as_array) else {
            let rendered = body.to_string();
            let excerpt: String = rendered.chars().take(BODY_EXCERPT_CHARS).collect();
            let ellipsis = if rendered.chars().count() > BODY_EXCERPT_CHARS {
                "…"
            } else {
                ""
            };
            return vec![format!("reply has no `Value` array: {excerpt}{ellipsis}")];
        };
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let Some(device) = entry.as_object() else {
                    return Some(format!("entry #{index} is {entry}, not a device object"));
                };
                let has_id = device
                    .get("UniqueID")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| !id.is_empty());
                if has_id {
                    return None;
                }
                let device_type = device
                    .get("DeviceType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<unknown DeviceType>");
                let device_number = device
                    .get("DeviceNumber")
                    .and_then(serde_json::Value::as_u64)
                    .map_or_else(|| "?".to_string(), |n| n.to_string());
                Some(format!("{device_type} {device_number}"))
            })
            .collect()
    }

    /// What `OmniSim`'s captured startup log has to say about the failure.
    fn startup_log_evidence(port: u16) -> String {
        let Some(log) = Self::stdout_log_path(port) else {
            return "OmniSim's startup log was not captured (no writable log dir).".to_string();
        };
        let Some(lines) = Self::exception_lines_from_log(&log) else {
            return format!(
                "OmniSim's startup log ({}) has no `Exception` line — read it in full.",
                log.display()
            );
        };
        format!("OmniSim's startup log says:\n{lines}")
    }

    /// The panic text for an unusable roster: the offending entries, why
    /// that would sink every device on the instance, the usual cause, and
    /// whatever `Exception` lines `OmniSim` logged at startup.
    fn roster_diagnostic(base_url: &str, offenders: &[String], port: u16) -> String {
        let evidence = Self::startup_log_evidence(port);
        format!(
            "OmniSim at {base_url} came up advertising device(s) rp cannot use: {}. \
             rp's discovery needs a UniqueID on every configureddevices entry, so this \
             instance would fail to connect EVERY device on it (a `missing field UniqueID` \
             cascade), not just the ones named. OmniSim drops a device's UniqueID when its \
             constructor throws at startup — typically because a seed profile value under \
             crates/bdd-infra/omnisim-config/ did not parse (a decimal read under a \
             comma-decimal culture is the known case; the spawn pins \
             DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1 against exactly that). {evidence}",
            offenders.join(", ")
        )
    }

    /// Every line of `log` that mentions `Exception`, each with the line
    /// that follows it (`OmniSim` prints the exception message on its own
    /// line), capped so a chatty log can't swamp the panic. `None` when
    /// the file is unreadable or has no such line.
    fn exception_lines_from_log(log: &Path) -> Option<String> {
        const MAX_LINES: usize = 12;
        let contents = std::fs::read_to_string(log).ok()?;
        let lines: Vec<&str> = contents.lines().collect();
        let mut picked: Vec<&str> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains("Exception") {
                continue;
            }
            picked.push(line);
            if let Some(next) = lines.get(i.saturating_add(1)) {
                picked.push(next);
            }
            if picked.len() >= MAX_LINES {
                break;
            }
        }
        if picked.is_empty() {
            None
        } else {
            Some(picked.join("\n"))
        }
    }

    /// Probe the OS for a free `127.0.0.1` port by binding an ephemeral
    /// listener and immediately dropping it. Another process can grab the
    /// port in the window before `OmniSim` binds it — that lost race surfaces
    /// as an early child exit in [`Self::wait_healthy`] and costs one retry
    /// in [`Self::get_or_spawn`].
    fn pick_free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .map_or_else(
                |e| panic!("failed to probe a free port for OmniSim: {e}"),
                |addr| addr.port(),
            )
    }

    /// Seed a per-instance `OMNISIM_SETTINGS_DIR` for `OmniSim` and return
    /// its path. The seed is a recursive copy of the checked-in
    /// `crates/bdd-infra/omnisim-config/` tree, whose layout mirrors what
    /// the fork puts under the settings root
    /// (`ascom-alpaca-simulator/<device>/v1/instance-0.xml`; the lowercase
    /// names also satisfy Windows' case-insensitive lookups of its
    /// platform-cased paths). `OmniSim` writes back to this directory on
    /// startup (e.g. emitting missing `UniqueIDs`, persisting full default
    /// profiles), so we MUST copy the source into a scratch location and
    /// never let `OmniSim` see the repository copy directly.
    ///
    /// The destination is suffixed with the test process's PID plus a
    /// process-wide spawn counter (`bdd-infra-omnisim-<pid>-<n>/`) under
    /// [`Self::state_root`]: with parallel suites and shards each
    /// spawning a private `OmniSim`, instances must not share a writable
    /// profile dir either — a shared dir would race the startup
    /// write-backs and leak profile *settings* (e.g. the telescope site,
    /// which `restart` does not reset) between concurrently running
    /// suites. (`XDG_CONFIG_HOME` cannot provide this isolation: .NET
    /// ignores it on macOS and Windows.) We fully reseed on every spawn
    /// so a write-back from a prior run can't leak into this one.
    ///
    /// The PID alone distinguishes *processes*, not *spawns*: this
    /// crate's own unit tests run several spawns concurrently on one
    /// PID, and a PID-only name would make them wipe and re-create one
    /// shared path. On Windows that races `remove_dir_all` against a
    /// sibling's `create_dir_all` — a directory stays in a
    /// delete-pending state until its last handle closes, so the
    /// re-create intermittently fails with `ERROR_ACCESS_DENIED`. The
    /// counter gives every spawn its own path, which also keeps
    /// concurrent same-process instances from sharing a live profile
    /// dir in the first place.
    ///
    /// Panics when the destination dir can't be created: spawning without
    /// the override would silently fall back to the shared platform-default
    /// profile store — reintroducing exactly the cross-suite leakage this
    /// isolation exists to prevent — so it must fail loudly instead. A
    /// missing seed *source* stays non-fatal: the instance still gets a
    /// private, initially empty config dir and runs on upstream defaults.
    fn prepare_settings_dir() -> PathBuf {
        static SPAWN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SPAWN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest =
            Self::state_root().join(format!("bdd-infra-omnisim-{}-{seq}", std::process::id()));
        // Wipe whatever a previous run that recycled this PID (and
        // reached this sequence number) left behind so an OmniSim
        // write-back from then can't survive into this run's profile.
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap_or_else(|e| {
            panic!(
                "bdd-infra: failed to create the per-instance OmniSim settings dir {}: {e} — \
                 proceeding without OMNISIM_SETTINGS_DIR would make concurrent OmniSim \
                 instances share the platform-default profile store",
                dest.display()
            )
        });
        if let Some(src) = Self::seed_config_source() {
            // Best-effort: a partial copy still leaves a private dir.
            let _ = Self::copy_dir_recursive(&src, &dest);
        }
        dest
    }

    /// Locate the checked-in `omnisim-config` seed tree.
    ///
    /// 1. `env!("CARGO_MANIFEST_DIR")/omnisim-config` — resolves under
    ///    cargo. Under Bazel `CARGO_MANIFEST_DIR` is a compile-time
    ///    sandbox path that doesn't exist at test runtime.
    /// 2. Walking up from the cwd looking for
    ///    `crates/bdd-infra/omnisim-config` — resolves in the Bazel
    ///    runfiles tree (after the `bdd_main!` chdir the cwd is
    ///    `<runfiles>/_main/<package>`; the seed tree rides along as
    ///    `data` on the `bdd-infra_rp_harness` target).
    ///
    /// Returns `None` when neither resolves. Note that before #467 the
    /// Bazel path never resolved (branch 1 was dead and the tree wasn't in
    /// the runfiles), so Bazel-run suites always used upstream defaults;
    /// branch 2 closes that gap and brings the tuned timings (shorter
    /// cover-calibrator open/close) to Bazel runs too.
    fn seed_config_source() -> Option<PathBuf> {
        Self::seed_config_source_from(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("omnisim-config"),
            std::env::current_dir().ok(),
        )
    }

    /// `seed_config_source` extracted over explicit candidates so the
    /// cwd-ancestors walk — the branch that resolves the seed inside the
    /// Bazel runfiles tree, where the compile-time path is dead — is
    /// unit-testable without depending on the build environment. See the
    /// `tests` module at the bottom of this file.
    fn seed_config_source_from(compile_time: PathBuf, cwd: Option<PathBuf>) -> Option<PathBuf> {
        if compile_time.is_dir() {
            return Some(compile_time);
        }
        let cwd = cwd?;
        for ancestor in cwd.ancestors() {
            let candidate = ancestor
                .join("crates")
                .join("bdd-infra")
                .join("omnisim-config");
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        None
    }

    /// Root for per-instance scratch state: Bazel's per-action
    /// `TEST_TMPDIR` when present (cleaned up by Bazel), else the cargo
    /// target tree (reached by `cargo clean`).
    fn state_root() -> PathBuf {
        if let Some(tmp) = std::env::var_os("TEST_TMPDIR") {
            return PathBuf::from(tmp);
        }
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(|p| p.parent())
                    .map_or_else(
                        || PathBuf::from("target"),
                        |workspace| workspace.join("target"),
                    )
            },
            PathBuf::from,
        )
    }

    /// Resolve the log directory for `OmniSim`'s captured stdout/stderr.
    /// Lives at `<CARGO_TARGET_DIR>/bdd-infra-omnisim-logs/` (or
    /// `<workspace>/target/bdd-infra-omnisim-logs/` if unset). Kept
    /// outside the seeded settings dir so `prepare_settings_dir`'s
    /// `remove_dir_all` can't sweep the previous run's logs.
    ///
    /// Returns `None` (caller falls back to `Stdio::null`) only if the
    /// directory can't be created.
    fn log_dir() -> Option<PathBuf> {
        // Under Bazel there is no cargo target tree and `CARGO_MANIFEST_DIR` is a
        // compile-time sandbox path, so the cargo branch below resolves to a
        // directory that can't be created at test runtime — OmniSim's logs would
        // silently go to `Stdio::null` and a CI crash would leave no trace (the
        // #171 diagnostic gap, recurring under Bazel). Bazel sets
        // `TEST_UNDECLARED_OUTPUTS_DIR` for test actions; files written there are
        // collected under `bazel-testlogs/.../test.outputs`. Prefer it.
        if let Some(undeclared) = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR") {
            let dest = PathBuf::from(undeclared).join("omnisim-logs");
            if std::fs::create_dir_all(&dest).is_ok() {
                return Some(dest);
            }
        }
        let target_dir = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(|p| p.parent())
                    .map_or_else(
                        || PathBuf::from("target"),
                        |workspace| workspace.join("target"),
                    )
            },
            PathBuf::from,
        );
        let dest = target_dir.join("bdd-infra-omnisim-logs");
        std::fs::create_dir_all(&dest).ok()?;
        Some(dest)
    }

    /// Where this process files `OmniSim`'s stdout for the instance on
    /// `port` — `omnisim.<pid>.<port>.stdout.log` under [`Self::log_dir`].
    /// `OmniSim` logs its startup device-construction exceptions there,
    /// which is why [`Self::roster_diagnostic`] reads it back. `None`
    /// when there is no writable log dir.
    fn stdout_log_path(port: u16) -> Option<PathBuf> {
        Self::log_dir().map(|dir| Self::stdout_log_path_in(&dir, port))
    }

    /// The stdout log file name for `port` inside an already-resolved
    /// log `dir` — the one place the `omnisim.<pid>.<port>.stdout.log`
    /// shape is spelled, so the spawn's writer and the diagnostic's
    /// reader can't drift apart.
    fn stdout_log_path_in(dir: &Path, port: u16) -> PathBuf {
        let pid = std::process::id();
        dir.join(format!("omnisim.{pid}.{port}.stdout.log"))
    }

    /// Open fresh (truncating) log files for `OmniSim`'s stdout and
    /// stderr, returning `Stdio` handles ready to attach to the
    /// `Command`. Falls back to `Stdio::null()` for either stream
    /// individually if its file can't be opened.
    ///
    /// File names embed the BDD test binary's PID so concurrent runs
    /// (e.g. `cargo test --workspace --test bdd`, where each package's
    /// BDD binary is a separate process sharing one `CARGO_TARGET_DIR`)
    /// don't truncate each other's logs. On Windows, file-locking on a
    /// shared name would also fail one of the spawns outright; the PID
    /// suffix avoids that. The port distinguishes retried spawn attempts
    /// within one process, so a failed attempt's log (the bind-race /
    /// old-binary evidence) survives the retry.
    fn open_log_files(port: u16) -> (Stdio, Stdio) {
        let dir = Self::log_dir();
        let pid = std::process::id();
        let stdout = dir
            .as_ref()
            .and_then(|d| std::fs::File::create(Self::stdout_log_path_in(d, port)).ok())
            .map_or_else(Stdio::null, Stdio::from);
        // Under Bazel, inherit OmniSim's stderr into the test process so a
        // crash / unhandled exception (the cause of the rp:bdd / calibrator-flats
        // OmniSim cascades) shows up in the failed test output (`--test_output=errors`)
        // in the CI job log — the TEST_UNDECLARED_OUTPUTS_DIR files aren't uploaded
        // by the bazel workflow today, and the flake doesn't reproduce locally.
        // stdout stays filed: OmniSim's per-request logging is too chatty to inherit.
        let stderr = if std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR").is_some() {
            Stdio::inherit()
        } else {
            dir.as_ref()
                .and_then(|d| {
                    std::fs::File::create(d.join(format!("omnisim.{pid}.{port}.stderr.log"))).ok()
                })
                .map_or_else(Stdio::null, Stdio::from)
        };
        (stdout, stderr)
    }

    fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dest.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                Self::copy_dir_recursive(&from, &to)?;
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    fn find_binary() -> String {
        if let Ok(path) = std::env::var("OMNISIM_PATH") {
            return path;
        }

        let binary_name = if cfg!(target_os = "windows") {
            "ascom.alpaca.simulators.exe"
        } else {
            "ascom.alpaca.simulators"
        };

        if let Ok(dir) = std::env::var("OMNISIM_DIR") {
            let path = std::path::Path::new(&dir).join(binary_name);
            return path.to_string_lossy().to_string();
        }

        binary_name.to_string()
    }

    async fn is_healthy(base_url: &str) -> bool {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("failed to build reqwest client");
        let url = format!("{base_url}/api/v1/camera/0/connected");
        matches!(client.get(&url).send().await, Ok(resp) if resp.status().is_success())
    }

    /// Poll `base_url` until `OmniSim` answers, watching the child so an
    /// early exit (lost port-bind race; a pre-`--multi-instance` binary
    /// deferring to another running instance) fails fast with its exit
    /// status instead of burning the full 30-second health window.
    async fn wait_healthy(child: &mut std::process::Child, base_url: &str) -> Result<(), String> {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "OmniSim exited during startup ({status}) — lost the port-bind \
                     race, or the binary does not support --multi-instance"
                ));
            }
            if Self::is_healthy(base_url).await {
                return Ok(());
            }
        }
        Err(format!(
            "OmniSim did not become healthy at {base_url} within 30 seconds"
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    use axum::http::StatusCode;
    use axum::routing::{get, put};
    use axum::Router;

    async fn spawn_stub(status: StatusCode) -> (String, tokio::sync::oneshot::Sender<()>) {
        let app = Router::new().route(
            "/api/v1/camera/0/connected",
            get(move || async move { status }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://127.0.0.1:{port}"), tx)
    }

    /// Stub server that responds to `PUT /simulator/v1/{class}/{n}/restart`
    /// with the given `status`. The route is registered at the exact
    /// `class`/`n` the test will hit, so a request to a different
    /// device falls through to a 404 (which is what `restart_device`
    /// will surface as an error — useful for one of the tests below).
    async fn spawn_restart_stub(
        class: &str,
        n: u32,
        status: StatusCode,
    ) -> (String, tokio::sync::oneshot::Sender<()>) {
        let route = format!("/simulator/v1/{class}/{n}/restart");
        let app = Router::new().route(&route, put(move || async move { status }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://127.0.0.1:{port}"), tx)
    }

    #[tokio::test]
    async fn is_healthy_returns_true_on_success() {
        let (base_url, shutdown) = spawn_stub(StatusCode::OK).await;
        assert!(OmniSimProcess::is_healthy(&base_url).await);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn is_healthy_returns_false_on_server_error() {
        let (base_url, shutdown) = spawn_stub(StatusCode::INTERNAL_SERVER_ERROR).await;
        assert!(!OmniSimProcess::is_healthy(&base_url).await);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn is_healthy_returns_false_when_connection_refused() {
        // Port 1 (tcpmux) sits outside every OS's dynamic port range and is
        // privileged on Unix, so no test process can occupy it and nothing
        // on a CI or dev host listens there — the connect reliably refuses.
        // A bind-then-drop ephemeral port raced the rest of the suite —
        // another test could re-bind it in the window and answer the probe.
        assert!(!OmniSimProcess::is_healthy("http://127.0.0.1:1").await);
    }

    #[tokio::test]
    async fn restart_device_returns_ok_on_success() {
        let (base_url, shutdown) = spawn_restart_stub("camera", 0, StatusCode::OK).await;
        let result = OmniSimHandle::restart_device_at(&base_url, "camera", 0).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn restart_device_returns_err_on_404() {
        // Stub registers /camera/0/restart but the test hits /telescope/0/restart.
        let (base_url, shutdown) = spawn_restart_stub("camera", 0, StatusCode::OK).await;
        let err = OmniSimHandle::restart_device_at(&base_url, "telescope", 0)
            .await
            .expect_err("expected an error for unrouted path");
        assert!(err.contains("404"), "expected 404 in error: {err}");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn restart_device_returns_err_on_server_error() {
        let (base_url, shutdown) =
            spawn_restart_stub("camera", 0, StatusCode::INTERNAL_SERVER_ERROR).await;
        let err = OmniSimHandle::restart_device_at(&base_url, "camera", 0)
            .await
            .expect_err("expected an error for 500 response");
        assert!(err.contains("500"), "expected 500 in error: {err}");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn restart_device_serializes_concurrent_restarts() {
        use std::sync::atomic::{AtomicI32, Ordering};
        use std::sync::Arc;

        // Stub that records whether two restart requests were ever in
        // flight at the same time. Each handler bumps an in-flight
        // counter, holds the request open briefly, then decrements —
        // without RESTART_SERIALIZER, 16 concurrent PUTs overlap here
        // reliably (this test failed before the mutex was added).
        let in_flight = Arc::new(AtomicI32::new(0));
        let overlapped = Arc::new(AtomicI32::new(0));
        let (in_flight_h, overlapped_h) = (in_flight.clone(), overlapped.clone());
        let app = Router::new().route(
            "/simulator/v1/camera/0/restart",
            put(move || {
                let in_flight = in_flight_h.clone();
                let overlapped = overlapped_h.clone();
                async move {
                    if in_flight.fetch_add(1, Ordering::SeqCst) > 0 {
                        overlapped.fetch_add(1, Ordering::SeqCst);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let puts: Vec<_> = (0..16)
            .map(|_| {
                let base_url = base_url.clone();
                tokio::spawn(async move {
                    OmniSimHandle::restart_device_at(&base_url, "camera", 0).await
                })
            })
            .collect();
        for put in puts {
            put.await.unwrap().unwrap();
        }
        assert_eq!(
            overlapped.load(Ordering::SeqCst),
            0,
            "restart PUTs overlapped despite RESTART_SERIALIZER"
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn set_safety_monitor_is_safe_puts_form_value() {
        use axum::Form;
        use std::collections::HashMap;

        let (tx_seen, rx_seen) = tokio::sync::oneshot::channel::<String>();
        let tx_seen = std::sync::Arc::new(std::sync::Mutex::new(Some(tx_seen)));
        let app = Router::new().route(
            "/simulator/v1/safetymonitor/0/issafesetting",
            put(move |Form(form): Form<HashMap<String, String>>| {
                let tx_seen = tx_seen.clone();
                async move {
                    if let Some(tx) = tx_seen.lock().unwrap().take() {
                        let _ = tx.send(form.get("IsSafeSetting").cloned().unwrap_or_default());
                    }
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        OmniSimHandle::set_safety_monitor_is_safe_at(&base_url, 0, false)
            .await
            .unwrap();
        assert_eq!(rx_seen.await.unwrap(), "false");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn set_safety_monitor_is_safe_returns_err_on_server_error() {
        let route = "/simulator/v1/safetymonitor/0/issafesetting";
        let app = Router::new().route(
            route,
            put(move || async move { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let err = OmniSimHandle::set_safety_monitor_is_safe_at(&base_url, 0, true)
            .await
            .expect_err("expected an error for 500 response");
        assert!(err.contains("500"), "expected 500 in error: {err}");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn restart_device_returns_err_when_connection_refused() {
        // Port 1: refused by construction, same reasoning as
        // is_healthy_returns_false_when_connection_refused above — a
        // bind-then-drop ephemeral port races the rest of the suite.
        let err = OmniSimHandle::restart_device_at("http://127.0.0.1:1", "camera", 0)
            .await
            .expect_err("expected a transport error");
        assert!(
            err.starts_with("PUT ") && err.contains("failed"),
            "unexpected transport error format: {err}"
        );
    }

    /// Stub answering one Alpaca telescope property PUT with the given
    /// JSON body, capturing the submitted form for assertion.
    async fn spawn_telescope_put_stub(
        property: &str,
        body: serde_json::Value,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<std::collections::HashMap<String, String>>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        use axum::Form;
        use std::collections::HashMap;

        let (tx_seen, rx_seen) = tokio::sync::oneshot::channel::<HashMap<String, String>>();
        let tx_seen = std::sync::Arc::new(std::sync::Mutex::new(Some(tx_seen)));
        let route = format!("/api/v1/telescope/0/{property}");
        let app = Router::new().route(
            &route,
            put(move |Form(form): Form<HashMap<String, String>>| {
                let tx_seen = tx_seen.clone();
                let body = body.clone();
                async move {
                    if let Some(tx) = tx_seen.lock().unwrap().take() {
                        let _ = tx.send(form);
                    }
                    axum::Json(body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://127.0.0.1:{port}"), rx_seen, tx)
    }

    #[tokio::test]
    async fn telescope_form_put_sends_values_and_accepts_error_number_zero() {
        let (base_url, rx_seen, shutdown) = spawn_telescope_put_stub(
            "synctocoordinates",
            serde_json::json!({ "ErrorNumber": 0, "ErrorMessage": "" }),
        )
        .await;
        OmniSimHandle::put_telescope_form_at(
            &base_url,
            0,
            "synctocoordinates",
            &[
                ("RightAscension", "2.5".to_string()),
                ("Declination", "0".to_string()),
            ],
        )
        .await
        .unwrap();
        let form = rx_seen.await.unwrap();
        assert_eq!(form.get("RightAscension").map(String::as_str), Some("2.5"));
        assert_eq!(form.get("Declination").map(String::as_str), Some("0"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn telescope_number_get_parses_value_and_surfaces_alpaca_error() {
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/api/v1/telescope/0/sitelatitude",
                get(|| async {
                    axum::Json(serde_json::json!({ "Value": 51.07861, "ErrorNumber": 0 }))
                }),
            )
            .route(
                "/api/v1/telescope/0/sitelongitude",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "ErrorNumber": 1024,
                        "ErrorMessage": "property not implemented"
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let lat = OmniSimHandle::get_telescope_number_at(&base_url, 0, "sitelatitude")
            .await
            .unwrap();
        assert!((lat - 51.07861).abs() < 1e-9, "unexpected latitude {lat}");

        let err = OmniSimHandle::get_telescope_number_at(&base_url, 0, "sitelongitude")
            .await
            .expect_err("expected the Alpaca error to surface");
        assert!(
            err.contains("1024") && err.contains("not implemented"),
            "unexpected error format: {err}"
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn telescope_helpers_reject_a_body_without_an_error_number() {
        use axum::routing::get;

        // An empty JSON object is what a non-Alpaca endpoint (wrong
        // port, proxy) might answer — both helpers must reject it
        // rather than read the missing ErrorNumber as success.
        let app = Router::new()
            .route(
                "/api/v1/telescope/0/sitelatitude",
                get(|| async { axum::Json(serde_json::json!({})) }),
            )
            .route(
                "/api/v1/telescope/0/tracking",
                put(|| async { axum::Json(serde_json::json!({})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let err = OmniSimHandle::get_telescope_number_at(&base_url, 0, "sitelatitude")
            .await
            .expect_err("a body without ErrorNumber must not read as success");
        assert!(err.contains("without a numeric ErrorNumber"), "{err}");

        let err = OmniSimHandle::put_telescope_form_at(
            &base_url,
            0,
            "tracking",
            &[("Tracking", "true".to_string())],
        )
        .await
        .expect_err("a body without ErrorNumber must not read as success");
        assert!(err.contains("without a numeric ErrorNumber"), "{err}");
        let _ = tx.send(());
    }

    /// Serializes tests that mutate the process-wide `OMNISIM_PATH` /
    /// `OMNISIM_DIR` env vars (`find_binary` reads them). tokio's Mutex so
    /// async tests can hold the guard across `.await` without tripping
    /// `clippy::await_holding_lock`; sync tests use `blocking_lock`.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn restore_env(key: &str, saved: Option<std::ffi::OsString>) {
        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    /// A PATH-resolvable binary that exits quickly (with an error) when
    /// handed `OmniSim`'s `--multi-instance --urls=...` args — a stand-in
    /// for a pre-fork `OmniSim` losing the port-bind race or bailing out.
    fn quick_fail_binary() -> &'static str {
        if cfg!(windows) {
            // whoami rejects unknown arguments and exits fast.
            "whoami"
        } else {
            // false ignores arguments and exits 1 immediately.
            "false"
        }
    }

    #[test]
    fn find_binary_prefers_path_then_dir_then_bare_name() {
        let _lock = ENV_LOCK.blocking_lock();
        let saved_path = std::env::var_os("OMNISIM_PATH");
        let saved_dir = std::env::var_os("OMNISIM_DIR");

        let expected_name = if cfg!(target_os = "windows") {
            "ascom.alpaca.simulators.exe"
        } else {
            "ascom.alpaca.simulators"
        };

        // OMNISIM_PATH wins over OMNISIM_DIR.
        std::env::set_var("OMNISIM_PATH", "/explicit/omnisim-binary");
        std::env::set_var("OMNISIM_DIR", "/some/install/dir");
        let from_path = OmniSimProcess::find_binary();

        // OMNISIM_DIR gets the platform binary name appended — this is the
        // branch local dev setups rely on.
        std::env::remove_var("OMNISIM_PATH");
        let from_dir = OmniSimProcess::find_binary();

        // Neither set: bare name, resolved via PATH at spawn time.
        std::env::remove_var("OMNISIM_DIR");
        let bare = OmniSimProcess::find_binary();

        restore_env("OMNISIM_PATH", saved_path);
        restore_env("OMNISIM_DIR", saved_dir);

        assert_eq!(from_path, "/explicit/omnisim-binary");
        assert_eq!(
            std::path::PathBuf::from(&from_dir),
            std::path::Path::new("/some/install/dir").join(expected_name)
        );
        assert_eq!(bare, expected_name);
    }

    #[tokio::test]
    async fn wait_healthy_fails_fast_when_the_child_exits() {
        // Portable quick-exit child: the health wait must report the exit
        // (with the --multi-instance hint) instead of burning the full
        // 30-second window against a port nothing listens on.
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit 0"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap()
        };
        let err = OmniSimProcess::wait_healthy(&mut child, "http://127.0.0.1:9")
            .await
            .expect_err("an exited child must fail the health wait");
        assert!(err.contains("exited during startup"), "{err}");
        assert!(err.contains("--multi-instance"), "{err}");
    }

    #[tokio::test]
    async fn spawn_on_port_reports_early_exit_and_reaps_the_child() {
        let port = OmniSimProcess::pick_free_port();
        let err = OmniSimProcess::spawn_on_port(quick_fail_binary(), port)
            .await
            .expect_err("a binary that exits at startup must not yield an instance");
        // An early exit is the lost-port-race shape: retryable, not fatal.
        let SpawnFailure::Retry(message) = err else {
            panic!("an early exit must be retryable, got {err:?}");
        };
        assert!(message.contains("exited during startup"), "{message}");
    }

    #[test]
    fn command_pins_invariant_culture_and_scrubs_sanitizer_env() {
        let cmd = OmniSimProcess::command(
            "omnisim-binary",
            "http://127.0.0.1:4242",
            PathBuf::from("/settings/dir"),
        );

        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        assert_eq!(args, ["--multi-instance", "--urls=http://127.0.0.1:4242"]);

        let envs: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> = cmd.get_envs().collect();
        let env = |key: &str| {
            envs.iter()
                .find(|(k, _)| *k == key)
                .unwrap_or_else(|| panic!("command does not touch {key}: {envs:?}"))
                .1
        };
        // The .NET culture pin: without it a comma-decimal host fails to
        // parse the seed profile's decimals and OmniSim advertises the
        // affected device without a UniqueID.
        assert_eq!(
            env("DOTNET_SYSTEM_GLOBALIZATION_INVARIANT"),
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(
            env("OMNISIM_SETTINGS_DIR"),
            Some(std::ffi::OsStr::new("/settings/dir"))
        );
        // Sanitizer injection is removed (None = env_remove), not merely
        // overridden.
        for scrubbed in ["LD_PRELOAD", "ASAN_OPTIONS", "LSAN_OPTIONS"] {
            assert_eq!(env(scrubbed), None, "{scrubbed} must be removed");
        }
    }

    /// A `configureddevices` body with the given entries, in `OmniSim`'s
    /// wire shape (`Value` array plus the standard envelope fields).
    fn roster_body(entries: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "Value": entries,
            "ClientTransactionID": 0,
            "ServerTransactionID": 1,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })
    }

    fn device_entry(device_type: &str, number: u32, unique_id: Option<&str>) -> serde_json::Value {
        let mut entry = serde_json::json!({
            "DeviceName": format!("Alpaca {device_type} Simulator"),
            "DeviceType": device_type,
            "DeviceNumber": number,
        });
        if let Some(id) = unique_id {
            entry["UniqueID"] = serde_json::Value::String(id.to_string());
        }
        entry
    }

    #[test]
    fn devices_without_unique_id_accepts_a_fully_identified_roster() {
        let body = roster_body(serde_json::json!([
            device_entry("Camera", 0, Some("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90")),
            device_entry(
                "CoverCalibrator",
                0,
                Some("fd25fce9-1a64-4c20-852f-6dff9014aebf")
            ),
        ]));
        assert_eq!(
            OmniSimProcess::devices_without_unique_id(&body),
            Vec::<String>::new()
        );
    }

    #[test]
    fn devices_without_unique_id_names_the_entry_missing_its_id() {
        // The shape a device whose constructor threw at startup leaves
        // behind: OmniSim still lists it, but without a UniqueID.
        let body = roster_body(serde_json::json!([
            device_entry("Camera", 0, Some("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90")),
            device_entry("CoverCalibrator", 0, None),
        ]));
        assert_eq!(
            OmniSimProcess::devices_without_unique_id(&body),
            vec!["CoverCalibrator 0".to_string()]
        );
    }

    #[test]
    fn devices_without_unique_id_treats_an_empty_id_as_missing() {
        let body = roster_body(serde_json::json!([device_entry("Telescope", 2, Some(""))]));
        assert_eq!(
            OmniSimProcess::devices_without_unique_id(&body),
            vec!["Telescope 2".to_string()]
        );
    }

    #[test]
    fn devices_without_unique_id_reports_a_null_entry_by_index() {
        // A concurrently mutated device list can serialise a null into
        // the roster.
        let body = roster_body(serde_json::json!([
            device_entry("Camera", 0, Some("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90")),
            serde_json::Value::Null,
        ]));
        assert_eq!(
            OmniSimProcess::devices_without_unique_id(&body),
            vec!["entry #1 is null, not a device object".to_string()]
        );
    }

    #[test]
    fn devices_without_unique_id_reports_a_body_without_a_value_array() {
        let body = serde_json::json!({"ErrorNumber": 0});
        let offenders = OmniSimProcess::devices_without_unique_id(&body);
        assert_eq!(offenders.len(), 1);
        assert!(offenders[0].contains("no `Value` array"), "{offenders:?}");
    }

    #[test]
    fn devices_without_unique_id_bounds_the_quoted_body() {
        // A large malformed reply is quoted as an excerpt, not verbatim,
        // so the panic text stays readable.
        let body = serde_json::json!({"Blob": "x".repeat(5_000)});
        let offenders = OmniSimProcess::devices_without_unique_id(&body);
        assert_eq!(offenders.len(), 1);
        assert!(offenders[0].chars().count() < 300, "{}", offenders[0].len());
        assert!(offenders[0].ends_with('…'), "{}", offenders[0]);
    }

    /// Stub `GET /management/v1/configureddevices` answering `status` with
    /// `body` (served verbatim, so a non-JSON body can be tested too).
    async fn spawn_roster_stub(
        status: StatusCode,
        body: String,
    ) -> (String, tokio::sync::oneshot::Sender<()>) {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(move || async move { (status, body) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://127.0.0.1:{port}"), tx)
    }

    #[tokio::test]
    async fn check_device_roster_passes_a_fully_identified_roster() {
        let body = roster_body(serde_json::json!([device_entry(
            "Camera",
            0,
            Some("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90")
        )]));
        let (base_url, shutdown) = spawn_roster_stub(StatusCode::OK, body.to_string()).await;
        OmniSimProcess::check_device_roster(&base_url, 65001)
            .await
            .unwrap();
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn check_device_roster_is_fatal_and_names_the_device_missing_its_id() {
        let body = roster_body(serde_json::json!([
            device_entry("Camera", 0, Some("3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90")),
            device_entry("CoverCalibrator", 0, None),
        ]));
        let (base_url, shutdown) = spawn_roster_stub(StatusCode::OK, body.to_string()).await;
        let err = OmniSimProcess::check_device_roster(&base_url, 65002)
            .await
            .expect_err("a device without a UniqueID must fail the roster gate");
        let _ = shutdown.send(());
        let SpawnFailure::Fatal(diagnostic) = err else {
            panic!("an unusable roster must be fatal, not retried: {err:?}");
        };
        assert!(diagnostic.contains("CoverCalibrator 0"), "{diagnostic}");
        assert!(diagnostic.contains("UniqueID"), "{diagnostic}");
        assert!(
            diagnostic.contains("DOTNET_SYSTEM_GLOBALIZATION_INVARIANT"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("Camera 0"), "{diagnostic}");
    }

    #[tokio::test]
    async fn check_device_roster_retries_when_the_probe_itself_fails() {
        // A 500 or a non-JSON body says nothing about the roster; the
        // health probe just answered, so treat it as a fresh anomaly and
        // let get_or_spawn retry rather than condemn the instance.
        let (base_url, shutdown) =
            spawn_roster_stub(StatusCode::INTERNAL_SERVER_ERROR, String::new()).await;
        let err = OmniSimProcess::check_device_roster(&base_url, 65003)
            .await
            .expect_err("HTTP 500 must not pass the roster gate");
        let _ = shutdown.send(());
        let SpawnFailure::Retry(message) = err else {
            panic!("a failed probe must be retryable: {err:?}");
        };
        assert!(message.contains("HTTP 500"), "{message}");

        let (base_url, shutdown) = spawn_roster_stub(StatusCode::OK, "not json".to_string()).await;
        let err = OmniSimProcess::check_device_roster(&base_url, 65004)
            .await
            .expect_err("a non-JSON body must not pass the roster gate");
        let _ = shutdown.send(());
        let SpawnFailure::Retry(message) = err else {
            panic!("a failed probe must be retryable: {err:?}");
        };
        assert!(message.contains("non-JSON"), "{message}");
    }

    #[tokio::test]
    async fn check_device_roster_retries_when_nothing_listens() {
        let err = OmniSimProcess::check_device_roster("http://127.0.0.1:9", 65005)
            .await
            .expect_err("a dead endpoint must not pass the roster gate");
        assert!(matches!(err, SpawnFailure::Retry(_)), "{err:?}");
    }

    #[test]
    fn exception_lines_from_log_quotes_each_exception_with_its_message_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("omnisim.stdout.log");
        std::fs::write(
            &log,
            "info: Loading Camera 0\n\
             16/08/2026 10:24:58 [Information] - CoverCalibrator 0 - Exception while creating CoverCalibrator simulator: \n\
             The input string '0.6' was not in a correct format.\n\
             info: Loading Telescope 0\n",
        )
        .unwrap();
        let picked = OmniSimProcess::exception_lines_from_log(&log).unwrap();
        assert_eq!(
            picked,
            "16/08/2026 10:24:58 [Information] - CoverCalibrator 0 - Exception while creating CoverCalibrator simulator: \n\
             The input string '0.6' was not in a correct format."
        );
    }

    #[test]
    fn exception_lines_from_log_is_none_without_an_exception_or_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("omnisim.stdout.log");
        std::fs::write(&log, "info: Loading Camera 0\ninfo: ready\n").unwrap();
        assert_eq!(OmniSimProcess::exception_lines_from_log(&log), None);
        assert_eq!(
            OmniSimProcess::exception_lines_from_log(&dir.path().join("missing.log")),
            None
        );
    }

    #[test]
    fn exception_lines_from_log_caps_a_chatty_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("omnisim.stdout.log");
        let chatty = "Exception here\nmessage\n".repeat(50);
        std::fs::write(&log, chatty).unwrap();
        let picked = OmniSimProcess::exception_lines_from_log(&log).unwrap();
        assert_eq!(picked.lines().count(), 12);
    }

    #[test]
    fn roster_diagnostic_quotes_the_startup_log_for_that_port() {
        // The diagnostic reads the same per-port stdout log the spawn
        // wrote, so the exception OmniSim printed while constructing the
        // device lands in the panic text.
        let port = 65006;
        let log = OmniSimProcess::stdout_log_path(port).expect("a writable log dir");
        std::fs::write(
            &log,
            "CoverCalibrator 0 - Exception while creating CoverCalibrator simulator: \n\
             The input string '0.6' was not in a correct format.\n",
        )
        .unwrap();
        let diagnostic = OmniSimProcess::roster_diagnostic(
            "http://127.0.0.1:65006",
            &["CoverCalibrator 0".to_string()],
            port,
        );
        let _ = std::fs::remove_file(&log);
        assert!(diagnostic.contains("CoverCalibrator 0"), "{diagnostic}");
        assert!(
            diagnostic.contains("The input string '0.6' was not in a correct format."),
            "{diagnostic}"
        );
    }

    #[test]
    fn roster_diagnostic_points_at_the_log_when_it_has_no_exception() {
        let port = 65007;
        let log = OmniSimProcess::stdout_log_path(port).expect("a writable log dir");
        std::fs::write(&log, "info: nothing to see\n").unwrap();
        let diagnostic = OmniSimProcess::roster_diagnostic(
            "http://127.0.0.1:65007",
            &["Telescope 0".to_string()],
            port,
        );
        let _ = std::fs::remove_file(&log);
        assert!(
            diagnostic.contains("has no `Exception` line"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains(&log.display().to_string()),
            "{diagnostic}"
        );
    }

    #[tokio::test]
    async fn get_or_spawn_panics_with_the_fork_floor_hint_after_all_attempts() {
        let _lock = ENV_LOCK.lock().await;
        let saved_path = std::env::var_os("OMNISIM_PATH");
        let saved_dir = std::env::var_os("OMNISIM_DIR");
        std::env::set_var("OMNISIM_PATH", quick_fail_binary());
        std::env::remove_var("OMNISIM_DIR");

        // get_or_spawn only touches the global OMNISIM singleton via its
        // caller (OmniSimHandle::start), so calling it directly is safe;
        // run it on a task so the expected panic is observable.
        let joined = tokio::spawn(async { OmniSimProcess::get_or_spawn().await }).await;

        restore_env("OMNISIM_PATH", saved_path);
        restore_env("OMNISIM_DIR", saved_dir);

        let join_err = joined.expect_err("get_or_spawn must panic when every attempt fails");
        assert!(join_err.is_panic());
        let message = *join_err
            .into_panic()
            .downcast::<String>()
            .expect("panic payload is the formatted message");
        assert!(
            message.contains(&format!("after {SPAWN_ATTEMPTS} attempts")),
            "{message}"
        );
        assert!(message.contains("v0.5.0-467.2"), "{message}");
    }

    #[test]
    fn prepare_settings_dir_gives_every_call_its_own_directory() {
        // Concurrent spawns in one process (parallel unit tests, or
        // get_or_spawn's retries overlapping a sibling test's spawn) must
        // not converge on a shared path: one call's remove_dir_all would
        // race another's create_dir_all, which on Windows surfaces as an
        // ERROR_ACCESS_DENIED panic while the directory sits
        // delete-pending.
        let dirs: Vec<PathBuf> = (0..8)
            .map(|_| std::thread::spawn(OmniSimProcess::prepare_settings_dir))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        let unique: std::collections::HashSet<&PathBuf> = dirs.iter().collect();
        assert_eq!(
            unique.len(),
            dirs.len(),
            "settings dirs must be distinct: {dirs:?}"
        );
        for dir in &dirs {
            assert!(dir.is_dir(), "{} was not created", dir.display());
        }
    }

    #[test]
    fn seed_config_source_prefers_the_compile_time_path() {
        let tmp = tempfile::tempdir().unwrap();
        let compile_time = tmp.path().join("omnisim-config");
        std::fs::create_dir_all(&compile_time).unwrap();
        let found = OmniSimProcess::seed_config_source_from(compile_time.clone(), None);
        assert_eq!(found, Some(compile_time));
    }

    #[test]
    fn seed_config_source_walks_cwd_ancestors_when_compile_time_path_is_dead() {
        // The Bazel case: the compile-time path points into a build sandbox
        // that no longer exists, and the seed sits in the runfiles tree a
        // few directories above the (chdir'd) cwd.
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp
            .path()
            .join("crates")
            .join("bdd-infra")
            .join("omnisim-config");
        std::fs::create_dir_all(&seed).unwrap();
        let cwd = tmp.path().join("services").join("rp");
        std::fs::create_dir_all(&cwd).unwrap();

        let found = OmniSimProcess::seed_config_source_from(
            tmp.path().join("no-such-sandbox").join("omnisim-config"),
            Some(cwd),
        );
        assert_eq!(
            found.map(|p| p.canonicalize().unwrap()),
            Some(seed.canonicalize().unwrap())
        );
    }

    #[test]
    fn seed_config_source_returns_none_when_no_candidate_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let found = OmniSimProcess::seed_config_source_from(
            tmp.path().join("missing"),
            Some(tmp.path().to_path_buf()),
        );
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn telescope_number_get_surfaces_http_error_status() {
        use axum::routing::get;

        let app = Router::new().route(
            "/api/v1/telescope/0/sitelatitude",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let err = OmniSimHandle::get_telescope_number_at(&base_url, 0, "sitelatitude")
            .await
            .expect_err("a non-success HTTP status must not read as success");
        assert!(err.contains("500"), "{err}");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn telescope_form_put_surfaces_http_error_status() {
        let app = Router::new().route(
            "/api/v1/telescope/0/tracking",
            put(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        let base_url = format!("http://127.0.0.1:{port}");

        let err = OmniSimHandle::put_telescope_form_at(
            &base_url,
            0,
            "tracking",
            &[("Tracking", "true".to_string())],
        )
        .await
        .expect_err("a non-success HTTP status must not read as success");
        assert!(err.contains("500"), "{err}");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn telescope_form_put_surfaces_alpaca_error_number() {
        // OmniSim refuses e.g. a sync with tracking off as HTTP 200 +
        // a non-zero ErrorNumber — the helper must fail loud on it.
        let (base_url, _rx_seen, shutdown) = spawn_telescope_put_stub(
            "synctocoordinates",
            serde_json::json!({
                "ErrorNumber": 1036,
                "ErrorMessage": "SyncToCoordinates is not allowed when tracking is False"
            }),
        )
        .await;
        let err = OmniSimHandle::put_telescope_form_at(
            &base_url,
            0,
            "synctocoordinates",
            &[("RightAscension", "2.5".to_string())],
        )
        .await
        .expect_err("expected the Alpaca error to surface");
        assert!(
            err.contains("1036") && err.contains("tracking is False"),
            "unexpected error format: {err}"
        );
        let _ = shutdown.send(());
    }
}

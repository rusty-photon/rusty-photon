//! Fluent builder for rp's JSON config + helper for the calibrator-flats
//! service config.
//!
//! Everything here is pure Rust — no I/O, no process spawning — so it's
//! trivial to unit-test and cheap to call from `Given` steps.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::scratch::scratch_dir;

/// Per-process counter so each call to [`RpConfigBuilder::build`] produces a
/// distinct `data_directory` and `session_state_file` inside this process's
/// [`scratch_dir`]: scenario N never inherits scenario N-1's session state
/// when the earlier one did not land cleanly on `idle`. Uniqueness *across*
/// processes — other test binaries, shards, or an earlier run's leftovers in
/// the same temp directory — comes from the scratch directory's random name,
/// not from this counter or the PID.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Camera equipment entry. `cooler_targets_c` is the dark-library
/// setpoint ladder (rp.md § Camera Cooling); empty ⇒ rp never touches
/// the camera's cooler, which is what almost every scenario wants.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
    pub cooler_targets_c: Vec<i32>,
}

/// Filter wheel equipment entry.
#[derive(Debug, Clone)]
pub struct FilterWheelConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
    pub filters: Vec<String>,
}

/// Cover-calibrator equipment entry.
#[derive(Debug, Clone)]
pub struct CoverCalibratorConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
    /// Override `cover_calibrator.poll_interval` in the emitted rp
    /// config. `None` ⇒ rp's default (3 s). The BDD harness pins this
    /// to a short duration (~100 ms) so cover/calibrator scenarios
    /// don't sit through 3-second polls; production rp deployments use
    /// the upstream default. The `OmniSim` profile we ship at
    /// `crates/bdd-infra/omnisim-config/...` keeps the simulator-side
    /// transitions short too — both knobs need to be small for the
    /// scenario wall-clock to drop.
    pub poll_interval: Option<std::time::Duration>,
}

/// Focuser equipment entry. `min_position` / `max_position` are the
/// operator-supplied safe-travel bounds enforced by `move_focuser`.
#[derive(Debug, Clone)]
pub struct FocuserConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
    pub min_position: Option<i32>,
    pub max_position: Option<i32>,
}

/// Singular mount equipment entry. `rp` deployments have at most one
/// mount — piggyback rigs share one across multiple optical trains —
/// so the builder field below is `Option<MountConfig>`, not a `Vec`.
#[derive(Debug, Clone)]
pub struct MountConfig {
    pub alpaca_url: String,
    pub device_number: u32,
    /// Optional post-`Slewing == false` settle time. `None` ⇒ rp's
    /// default (zero). Per-call `settle_after` on `slew` overrides.
    pub settle_after_slew: Option<std::time::Duration>,
}

/// Safety-monitor equipment entry.
#[derive(Debug, Clone)]
pub struct SafetyMonitorConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
}

/// Switch equipment entry.
#[derive(Debug, Clone)]
pub struct SwitchConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
}

/// Rotator equipment entry.
#[derive(Debug, Clone)]
pub struct RotatorConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
}

/// `ObservingConditions` equipment entry.
#[derive(Debug, Clone)]
pub struct ObservingConditionsConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
}

/// Dome equipment entry.
#[derive(Debug, Clone)]
pub struct DomeConfig {
    pub id: String,
    pub alpaca_url: String,
    pub device_number: u32,
}

/// Plate-solver service config — emitted as the top-level
/// `plate_solver` block in rp's JSON config (parallel to `mount`,
/// `guider`, etc.; the plate solver is an rp-managed service, not
/// equipment).
#[derive(Debug, Clone)]
pub struct PlateSolverConfig {
    pub url: String,
    /// rp HTTP-client outer timeout (the connection-side backstop).
    /// `None` ⇒ rp's default (`60s`).
    pub timeout: Option<std::time::Duration>,
    /// Operator-set search radius applied when the per-call MCP
    /// parameter is omitted. `None` ⇒ omit from rp config (wrapper
    /// falls through to ASTAP's own default).
    pub default_search_radius_deg: Option<f64>,
}

/// Guider service config — emitted as the `equipment.mount.guiding`
/// block in rp's JSON config.
///
/// Guiding is mount-scoped: the guider corrects and dithers by moving
/// the mount, so rp rejects the block anywhere else. All thresholds are
/// guide-camera pixels.
#[derive(Debug, Clone)]
pub struct GuiderConfig {
    pub url: String,
    /// rp HTTP-client deadline for the quick guider calls. `None` ⇒
    /// rp's default (`90s`).
    pub timeout: Option<std::time::Duration>,
    /// Operator-set settle defaults forwarded on every
    /// `start_guiding` / `dither` call. `None` fields are omitted
    /// from the emitted block (the guider service's own `settling`
    /// config then applies).
    pub settle_pixels: Option<f64>,
    pub settle_time: Option<std::time::Duration>,
    pub settle_timeout: Option<std::time::Duration>,
    /// Default `dither` amount when the per-call `pixels` parameter
    /// is omitted.
    pub dither_pixels: Option<f64>,
    /// Rotation threshold above which rp clears the PHD2 calibration
    /// when rotating a guide-coupled train. `None` ⇒ rp's default (5°).
    pub recalibrate_above_deg: Option<f64>,
    /// The `focus_watch` sub-block (rp.md § Guide Focus Watch),
    /// passed through as raw JSON. `None` ⇒ omit (watch disabled).
    pub focus_watch: Option<Value>,
}

impl GuiderConfig {
    /// A url-only config: every default left to rp / the service.
    #[must_use]
    pub const fn url_only(url: String) -> Self {
        Self {
            url,
            timeout: None,
            settle_pixels: None,
            settle_time: None,
            settle_timeout: None,
            dither_pixels: None,
            recalibrate_above_deg: None,
            focus_watch: None,
        }
    }
}

/// One `equipment.optical_trains[]` entry (rp.md § Optical Trains):
/// an ordered list of roster device ids, objective side first,
/// terminating in a camera.
#[derive(Debug, Clone)]
pub struct OpticalTrainConfig {
    pub id: String,
    /// `"imaging"` or `"guiding"`. `None` ⇒ omit the field (rp
    /// defaults to imaging).
    pub purpose: Option<String>,
    /// Effective focal length of the light path in millimetres.
    /// `None` ⇒ omit the field (captures through this train's camera
    /// carry no `optics` block).
    pub focal_length_mm: Option<f64>,
    /// Default framing angle in degrees east of north (layer two of
    /// the effective position angle, rp.md § Target Store → Position
    /// angle). `None` ⇒ omit the field.
    pub default_position_angle_degrees: Option<f64>,
    pub devices: Vec<String>,
    /// Per-train V-curve sweep parameters (`optical_trains[].auto_focus`).
    /// `None` ⇒ omit the block.
    pub auto_focus: Option<TrainAutoFocusConfig>,
}

/// The `optical_trains[].auto_focus` block (rp.md § Optical Trains).
///
/// Which fields rp accepts depends on the train's purpose — imaging
/// blocks carry the capture fields, the guiding train's block the
/// metric-sweep ones — so everything but the geometry is optional
/// here and `None` fields are omitted from the emitted JSON.
/// Optional fields the block also accepts (`threshold_sigma`,
/// `min_fit_points`) are omitted — scenarios that need them can grow
/// this struct.
#[derive(Debug, Clone)]
pub struct TrainAutoFocusConfig {
    /// Per-frame exposure, humantime string (e.g. `"100ms"`).
    /// Capture sweeps only.
    pub duration: Option<String>,
    pub step_size: i64,
    pub half_width: i64,
    /// Capture sweeps only.
    pub min_area: Option<i64>,
    /// Capture sweeps only.
    pub max_area: Option<i64>,
    /// Metric (guiding-train) sweeps only.
    pub frames_per_step: Option<i64>,
}

/// Overrides for rp's top-level `cooling` block (rp.md § Camera
/// Cooling → Tuning).
///
/// `None` fields are omitted so rp's defaults apply; the BDD harness
/// pins the timing knobs short so a cooldown pass completes in test
/// time against the simulator's fast cooler.
#[derive(Debug, Clone, Default)]
pub struct CoolingOverrides {
    pub poll_interval: Option<std::time::Duration>,
    pub plateau_window: Option<std::time::Duration>,
    pub warmup_step_interval: Option<std::time::Duration>,
    pub max_cooldown: Option<std::time::Duration>,
}

impl CoolingOverrides {
    /// The timing profile the camera-cooling scenarios use: 250 ms
    /// polls, a 1 s plateau window (the simulator's curve updates
    /// every few ms, so 1 s of quiet is a real plateau), 100 ms
    /// warm-up steps, and a 30 s cooldown backstop.
    #[must_use]
    pub const fn fast() -> Self {
        Self {
            poll_interval: Some(std::time::Duration::from_millis(250)),
            plateau_window: Some(std::time::Duration::from_secs(1)),
            warmup_step_interval: Some(std::time::Duration::from_millis(100)),
            max_cooldown: Some(std::time::Duration::from_secs(30)),
        }
    }
}

/// Accumulates equipment and plugin entries, then emits rp's JSON config.
#[derive(Debug, Default, Clone)]
pub struct RpConfigBuilder {
    pub cameras: Vec<CameraConfig>,
    pub filter_wheels: Vec<FilterWheelConfig>,
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    pub focusers: Vec<FocuserConfig>,
    /// Safety monitors gating the session (see rp.md § Safety).
    pub safety_monitors: Vec<SafetyMonitorConfig>,
    /// Switches — roster membership + connectivity status only (rp.md §
    /// Equipment Integration); no MCP tool integration yet.
    pub switches: Vec<SwitchConfig>,
    /// Rotators — roster membership + connectivity status only.
    pub rotators: Vec<RotatorConfig>,
    /// `ObservingConditions` devices — roster membership + connectivity
    /// status only.
    pub observing_conditions: Vec<ObservingConditionsConfig>,
    /// Domes — roster membership + connectivity status only.
    pub domes: Vec<DomeConfig>,
    /// Override `safety.poll_interval` in the emitted rp config.
    /// `None` ⇒ rp's default (10 s). Safety scenarios pin this short
    /// (~250 ms) so unsafe/safe transitions are detected quickly.
    pub safety_poll_interval: Option<std::time::Duration>,
    /// Override `equipment.reconnect_interval` in the emitted rp
    /// config. `None` ⇒ rp's default (30 s). Session-recovery
    /// scenarios pin this short (~500 ms) so the reconnect supervisor
    /// heals dead device sessions in test time.
    pub reconnect_interval: Option<std::time::Duration>,
    /// Singular mount — at most one per `rp` deployment.
    pub mount: Option<MountConfig>,
    /// Optional plate-solver service config. `None` ⇒ omit the
    /// top-level `plate_solver` block from the emitted config so
    /// rp's `plate_solve` MCP tool reports "not configured".
    pub plate_solver: Option<PlateSolverConfig>,
    /// Optional guider service config, emitted as
    /// `equipment.mount.guiding`. `None` ⇒ omit the block so rp's
    /// guiding MCP tools report "not configured". Setting it requires
    /// a mount ([`RpConfigBuilder::build`] panics otherwise) — guiding
    /// is mount-scoped by rp's schema.
    pub guider: Option<GuiderConfig>,
    /// Optical trains (`equipment.optical_trains`): ordered device-id
    /// lists, objective side first, terminating in a camera.
    pub optical_trains: Vec<OpticalTrainConfig>,
    /// Optional `(latitude_degrees, longitude_degrees)` site block.
    /// Required for ephemeris-driven scenarios (planner, twilight,
    /// alt/az MCP tools) and for exercising the mount-side site
    /// validation path. None ⇒ rp's `site` field stays absent.
    pub site: Option<(f64, f64)>,
    pub plugin_configs: Vec<Value>,
    /// Override `session.data_directory`. When `None`, the builder
    /// generates a fresh per-call path. The cross-restart BDD scenarios
    /// need to pin the same path across two `start_rp` calls.
    pub data_directory: Option<String>,
    /// Override `session.session_state_file`. When `None`, the builder
    /// generates a fresh per-call path, so an rp respawn never finds a
    /// stale session registry by accident. The startup-recovery BDD
    /// scenarios pin this so the restarted rp reads the state its
    /// predecessor persisted.
    pub session_state_file: Option<String>,
    /// Override `imaging.cache_max_mib` / `cache_max_images`. When `None`,
    /// rp's defaults apply (1024 MiB / 8 images).
    pub imaging_overrides: Option<(usize, usize)>,
    /// Override the `centering` block's `(solve_time_estimate,
    /// slew_overhead_estimate)`. When `None`, the block is omitted and
    /// rp's defaults apply (30 s / 10 s). Shrinking these lets a test
    /// drive a sub-second `centering_started` `max_duration_ms` for the
    /// operation watchdog (the advisory outer-loop deadline is
    /// `max_attempts × (duration + solve_time_estimate +
    /// slew_overhead_estimate)`).
    pub centering: Option<(std::time::Duration, std::time::Duration)>,
    /// Override the `cooling` block's timing knobs. When `None`, the
    /// block is omitted and rp's defaults apply (10 s polls, 2 m
    /// plateau window — far too slow for test scenarios).
    pub cooling: Option<CoolingOverrides>,
}

impl RpConfigBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_camera(&mut self, camera: CameraConfig) -> &mut Self {
        self.cameras.push(camera);
        self
    }

    pub fn add_filter_wheel(&mut self, fw: FilterWheelConfig) -> &mut Self {
        self.filter_wheels.push(fw);
        self
    }

    pub fn add_cover_calibrator(&mut self, cc: CoverCalibratorConfig) -> &mut Self {
        self.cover_calibrators.push(cc);
        self
    }

    pub fn add_focuser(&mut self, foc: FocuserConfig) -> &mut Self {
        self.focusers.push(foc);
        self
    }

    pub fn add_safety_monitor(&mut self, sm: SafetyMonitorConfig) -> &mut Self {
        self.safety_monitors.push(sm);
        self
    }

    pub fn add_switch(&mut self, switch: SwitchConfig) -> &mut Self {
        self.switches.push(switch);
        self
    }

    pub fn add_rotator(&mut self, rotator: RotatorConfig) -> &mut Self {
        self.rotators.push(rotator);
        self
    }

    pub fn add_observing_conditions(&mut self, oc: ObservingConditionsConfig) -> &mut Self {
        self.observing_conditions.push(oc);
        self
    }

    pub fn add_dome(&mut self, dome: DomeConfig) -> &mut Self {
        self.domes.push(dome);
        self
    }

    /// Override rp's safety poll interval (overwrites any prior call).
    /// When unset, the emitted `safety` block is empty and rp's default
    /// (10 s) applies.
    pub const fn with_safety_poll_interval(&mut self, interval: std::time::Duration) -> &mut Self {
        self.safety_poll_interval = Some(interval);
        self
    }

    /// Override rp's equipment reconnect interval (overwrites any prior
    /// call). When unset, the key is omitted and rp's default (30 s)
    /// applies — far too slow for session-recovery scenarios.
    pub const fn with_reconnect_interval(&mut self, interval: std::time::Duration) -> &mut Self {
        self.reconnect_interval = Some(interval);
        self
    }

    /// Set the singular mount config (overwrites any prior call).
    pub fn with_mount(&mut self, mount: MountConfig) -> &mut Self {
        self.mount = Some(mount);
        self
    }

    /// Set the plate-solver service config (overwrites any prior
    /// call). When unset, the emitted rp config has no
    /// `plate_solver` block and the `plate_solve` MCP tool reports
    /// "not configured".
    pub fn with_plate_solver(&mut self, plate_solver: PlateSolverConfig) -> &mut Self {
        self.plate_solver = Some(plate_solver);
        self
    }

    /// Set the guider service config (overwrites any prior call),
    /// emitted as `equipment.mount.guiding` — call [`Self::with_mount`]
    /// too, or [`Self::build`] panics. When unset, the emitted rp
    /// config has no guiding block and the guiding MCP tools report
    /// "not configured".
    pub fn with_guider(&mut self, guider: GuiderConfig) -> &mut Self {
        self.guider = Some(guider);
        self
    }

    /// Append an optical train (`equipment.optical_trains[]`).
    pub fn add_optical_train(&mut self, train: OpticalTrainConfig) -> &mut Self {
        self.optical_trains.push(train);
        self
    }

    /// Set the observer site (latitude/longitude in degrees). Used by
    /// ephemeris and planner scenarios; also required to exercise
    /// the mount-side site validation rule on connect.
    pub const fn with_site(&mut self, latitude_degrees: f64, longitude_degrees: f64) -> &mut Self {
        self.site = Some((latitude_degrees, longitude_degrees));
        self
    }

    pub fn add_plugin(&mut self, plugin: Value) -> &mut Self {
        self.plugin_configs.push(plugin);
        self
    }

    /// Pin `session.data_directory` to an explicit path. Used by the
    /// cross-restart BDD scenarios to keep two consecutive rp processes
    /// pointing at the same on-disk archive.
    pub fn with_data_directory(&mut self, path: impl Into<String>) -> &mut Self {
        self.data_directory = Some(path.into());
        self
    }

    /// Pin `session.session_state_file` to an explicit path. Used by the
    /// startup-recovery BDD scenarios to keep two consecutive rp
    /// processes reading and writing the same session registry.
    pub fn with_session_state_file(&mut self, path: impl Into<String>) -> &mut Self {
        self.session_state_file = Some(path.into());
        self
    }

    /// Override the imaging-cache budgets (`cache_max_mib`,
    /// `cache_max_images`). Used by tests that want to drive evictions
    /// (e.g. setting `cache_max_images = 1` so the second capture evicts
    /// the first).
    pub const fn with_imaging(
        &mut self,
        cache_max_mib: usize,
        cache_max_images: usize,
    ) -> &mut Self {
        self.imaging_overrides = Some((cache_max_mib, cache_max_images));
        self
    }

    /// Override the `centering` deadline estimates (`solve_time_estimate`,
    /// `slew_overhead_estimate`). Used by the operation-watchdog e2e to
    /// shrink the advisory `centering_started` `max_duration_ms` so the
    /// Sentinel watchdog's per-operation timer fires in a couple of
    /// seconds instead of the ~40 s the defaults imply.
    pub const fn with_centering(
        &mut self,
        solve_time_estimate: std::time::Duration,
        slew_overhead_estimate: std::time::Duration,
    ) -> &mut Self {
        self.centering = Some((solve_time_estimate, slew_overhead_estimate));
        self
    }

    /// Set the `cooling` block's timing overrides (overwrites any
    /// prior call). When unset, the block is omitted and rp's
    /// defaults apply.
    pub const fn with_cooling(&mut self, cooling: CoolingOverrides) -> &mut Self {
        self.cooling = Some(cooling);
        self
    }

    /// Serialize into the JSON shape rp's config loader expects.
    pub fn build(&self) -> Value {
        let mut safety = serde_json::json!({});
        if let Some(poll) = self.safety_poll_interval {
            set_key(&mut safety, "poll_interval", duration_ms(poll));
        }

        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);

        let data_directory = self.data_directory.clone().unwrap_or_else(|| {
            scratch_dir()
                .join(format!("data-{seq}"))
                .to_string_lossy()
                .to_string()
        });

        let session_state_file = self.session_state_file.clone().unwrap_or_else(|| {
            scratch_dir()
                .join(format!("session-{seq}.json"))
                .to_string_lossy()
                .to_string()
        });

        let mut config = serde_json::json!({
            "session": {
                "data_directory": data_directory,
                "session_state_file": session_state_file,
                "file_naming_pattern": "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
            },
            "equipment": {
                "cameras": self.cameras_value(),
                "optical_trains": self.optical_trains_value(),
                "mount": self.mount_value(),
                "focusers": self.focusers_value(),
                "filter_wheels": self.filter_wheels_value(),
                "cover_calibrators": self.cover_calibrators_value(),
                "safety_monitors": device_list(self.safety_monitors.iter().map(|d| (&d.id, &d.alpaca_url, d.device_number))),
                "switches": device_list(self.switches.iter().map(|d| (&d.id, &d.alpaca_url, d.device_number))),
                "rotators": device_list(self.rotators.iter().map(|d| (&d.id, &d.alpaca_url, d.device_number))),
                "observing_conditions": device_list(self.observing_conditions.iter().map(|d| (&d.id, &d.alpaca_url, d.device_number))),
                "domes": device_list(self.domes.iter().map(|d| (&d.id, &d.alpaca_url, d.device_number)))
            },
            "plugins": self.plugin_configs,
            // Target-store settings (`config["target_store"]`) are injected
            // by rp's own BDD world when a scenario needs them; targets
            // themselves are seeded post-boot via the `add_target` MCP tool
            // (the legacy `targets[]` config array was retired).
            "planner": {
                "min_altitude_degrees": 20,
                "dawn_buffer_minutes": 30,
                "prefer_transiting": true,
                "minimize_filter_changes": true
            },
            "safety": safety,
            "server": {
                "port": 0,
                "bind_address": "127.0.0.1"
            }
        });

        if let Some(interval) = self.reconnect_interval {
            // The literal above always carries an `equipment` object.
            if let Some(equipment) = config.get_mut("equipment") {
                set_key(equipment, "reconnect_interval", duration_ms(interval));
            }
        }

        if let Some((max_mib, max_images)) = self.imaging_overrides {
            set_key(
                &mut config,
                "imaging",
                serde_json::json!({
                    "cache_max_mib": max_mib,
                    "cache_max_images": max_images,
                }),
            );
        }

        if let Some((lat, lon)) = self.site {
            set_key(
                &mut config,
                "site",
                serde_json::json!({
                    "latitude_degrees": lat,
                    "longitude_degrees": lon,
                }),
            );
        }

        if let Some(block) = self.plate_solver_value() {
            set_key(&mut config, "plate_solver", block);
        }

        if let Some((solve, slew_overhead)) = self.centering {
            set_key(
                &mut config,
                "centering",
                serde_json::json!({
                    "solve_time_estimate": format!("{}ms", solve.as_millis()),
                    "slew_overhead_estimate": format!("{}ms", slew_overhead.as_millis()),
                }),
            );
        }

        if let Some(block) = self.cooling_value() {
            set_key(&mut config, "cooling", block);
        }

        config
    }

    fn cameras_value(&self) -> Vec<Value> {
        self.cameras
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "name": c.id,
                    "alpaca_url": c.alpaca_url,
                    "device_type": "camera",
                    "device_number": c.device_number,
                    "cooler_targets_c": c.cooler_targets_c,
                    "gain": 100,
                    "offset": 50
                })
            })
            .collect()
    }

    fn filter_wheels_value(&self) -> Vec<Value> {
        self.filter_wheels
            .iter()
            .map(|fw| {
                serde_json::json!({
                    "id": fw.id,
                    "alpaca_url": fw.alpaca_url,
                    "device_number": fw.device_number,
                    "filters": fw.filters
                })
            })
            .collect()
    }

    fn cover_calibrators_value(&self) -> Vec<Value> {
        self.cover_calibrators
            .iter()
            .map(|cc| {
                let mut obj = serde_json::json!({
                    "id": cc.id,
                    "alpaca_url": cc.alpaca_url,
                    "device_number": cc.device_number,
                });
                if let Some(poll) = cc.poll_interval {
                    set_key(&mut obj, "poll_interval", duration_ms(poll));
                }
                obj
            })
            .collect()
    }

    fn focusers_value(&self) -> Vec<Value> {
        self.focusers
            .iter()
            .map(|f| {
                let mut obj = serde_json::json!({
                    "id": f.id,
                    "alpaca_url": f.alpaca_url,
                    "device_number": f.device_number,
                });
                if let Some(min) = f.min_position {
                    set_key(&mut obj, "min_position", serde_json::json!(min));
                }
                if let Some(max) = f.max_position {
                    set_key(&mut obj, "max_position", serde_json::json!(max));
                }
                obj
            })
            .collect()
    }

    /// The singular `equipment.mount` block (`null` when no mount is
    /// configured), carrying the mount-scoped `guiding` block.
    fn mount_value(&self) -> Value {
        assert!(
            self.mount.is_some() || self.guider.is_none(),
            "guiding is mount-scoped (equipment.mount.guiding): \
             call with_mount before with_guider"
        );
        self.mount.as_ref().map_or(Value::Null, |m| {
            let mut obj = serde_json::json!({
                "alpaca_url": m.alpaca_url,
                "device_number": m.device_number,
            });
            if let Some(d) = m.settle_after_slew {
                set_key(&mut obj, "settle_after_slew", duration_ms(d));
            }
            if let Some(g) = &self.guider {
                set_key(&mut obj, "guiding", guiding_block(g));
            }
            obj
        })
    }

    fn optical_trains_value(&self) -> Vec<Value> {
        self.optical_trains
            .iter()
            .map(|t| {
                let mut obj = serde_json::json!({
                    "id": t.id,
                    "devices": t.devices,
                });
                if let Some(p) = &t.purpose {
                    set_key(&mut obj, "purpose", serde_json::json!(p));
                }
                if let Some(f) = t.focal_length_mm {
                    set_key(&mut obj, "focal_length_mm", serde_json::json!(f));
                }
                if let Some(a) = t.default_position_angle_degrees {
                    set_key(
                        &mut obj,
                        "default_position_angle_degrees",
                        serde_json::json!(a),
                    );
                }
                if let Some(af) = &t.auto_focus {
                    let mut block = serde_json::json!({
                        "step_size": af.step_size,
                        "half_width": af.half_width,
                    });
                    if let Some(d) = &af.duration {
                        set_key(&mut block, "duration", serde_json::json!(d));
                    }
                    if let Some(v) = af.min_area {
                        set_key(&mut block, "min_area", serde_json::json!(v));
                    }
                    if let Some(v) = af.max_area {
                        set_key(&mut block, "max_area", serde_json::json!(v));
                    }
                    if let Some(v) = af.frames_per_step {
                        set_key(&mut block, "frames_per_step", serde_json::json!(v));
                    }
                    set_key(&mut obj, "auto_focus", block);
                }
                obj
            })
            .collect()
    }

    /// The top-level `plate_solver` block; `None` when unconfigured.
    fn plate_solver_value(&self) -> Option<Value> {
        self.plate_solver.as_ref().map(|ps| {
            let mut block = serde_json::json!({
                "url": ps.url,
            });
            if let Some(t) = ps.timeout {
                set_key(&mut block, "timeout", duration_ms(t));
            }
            if let Some(r) = ps.default_search_radius_deg {
                set_key(
                    &mut block,
                    "default_search_radius_deg",
                    serde_json::json!(r),
                );
            }
            block
        })
    }

    /// The top-level `cooling` timing-override block; `None` when
    /// unconfigured.
    fn cooling_value(&self) -> Option<Value> {
        self.cooling.as_ref().map(|cooling| {
            let mut block = serde_json::json!({});
            if let Some(d) = cooling.poll_interval {
                set_key(&mut block, "poll_interval", duration_ms(d));
            }
            if let Some(d) = cooling.plateau_window {
                set_key(&mut block, "plateau_window", duration_ms(d));
            }
            if let Some(d) = cooling.warmup_step_interval {
                set_key(&mut block, "warmup_step_interval", duration_ms(d));
            }
            if let Some(d) = cooling.max_cooldown {
                set_key(&mut block, "max_cooldown", duration_ms(d));
            }
            block
        })
    }
}

/// One plain `{id, alpaca_url, device_number}` equipment entry per
/// `(id, alpaca_url, device_number)` triple — the shape shared by every
/// device kind without extra fields.
fn device_list<'a>(devices: impl Iterator<Item = (&'a String, &'a String, u32)>) -> Vec<Value> {
    devices
        .map(|(id, alpaca_url, device_number)| {
            serde_json::json!({
                "id": id,
                "alpaca_url": alpaca_url,
                "device_number": device_number,
            })
        })
        .collect()
}

/// A `Duration` in the `"<n>ms"` spelling rp's humantime-based config
/// fields accept.
fn duration_ms(d: std::time::Duration) -> Value {
    serde_json::json!(format!("{}ms", d.as_millis()))
}

/// Insert `key` into a JSON object built from a `json!({...})` literal.
///
/// `Value`'s `IndexMut` panics on non-object receivers; every caller here
/// holds an object literal, so `as_object_mut` always succeeds and the
/// `debug_assert` only exists to keep a future non-object caller loud.
fn set_key(obj: &mut Value, key: &str, value: Value) {
    let Some(map) = obj.as_object_mut() else {
        debug_assert!(false, "set_key on non-object JSON value: {obj}");
        return;
    };
    map.insert(key.to_owned(), value);
}

/// Serialize a [`GuiderConfig`] into the `equipment.mount.guiding`
/// block shape, omitting `None` fields so rp's defaults apply.
fn guiding_block(g: &GuiderConfig) -> Value {
    let mut block = serde_json::json!({
        "url": g.url,
    });
    if let Some(t) = g.timeout {
        set_key(
            &mut block,
            "timeout",
            serde_json::json!(format!("{}ms", t.as_millis())),
        );
    }
    if let Some(p) = g.settle_pixels {
        set_key(&mut block, "settle_pixels", serde_json::json!(p));
    }
    if let Some(t) = g.settle_time {
        set_key(
            &mut block,
            "settle_time",
            serde_json::json!(format!("{}ms", t.as_millis())),
        );
    }
    if let Some(t) = g.settle_timeout {
        set_key(
            &mut block,
            "settle_timeout",
            serde_json::json!(format!("{}ms", t.as_millis())),
        );
    }
    if let Some(p) = g.dither_pixels {
        set_key(&mut block, "dither_pixels", serde_json::json!(p));
    }
    if let Some(d) = g.recalibrate_above_deg {
        set_key(&mut block, "recalibrate_above_deg", serde_json::json!(d));
    }
    if let Some(w) = &g.focus_watch {
        set_key(&mut block, "focus_watch", w.clone());
    }
    block
}

/// Build a JSON config for the calibrator-flats service from a flat plan.
///
/// The resulting config drives the real calibrator-flats orchestrator
/// process against `OmniSim`'s simulated camera/filter wheel/cover calibrator.
/// Tolerance is `1.0` and `max_iterations = 1` so tests verify end-to-end
/// plumbing (3-process coordination, cover lifecycle, session lifecycle)
/// rather than convergence math — the latter is covered by unit tests.
#[must_use]
pub fn build_calibrator_flats_config(
    filters: &[(String, u32)],
    filter_wheel_id: Option<&str>,
) -> Value {
    let filter_entries: Vec<Value> = filters
        .iter()
        .map(|(name, count)| {
            serde_json::json!({
                "name": name,
                "count": count,
            })
        })
        .collect();

    let mut config = serde_json::json!({
        "camera_id": "main-cam",
        "calibrator_id": "flat-panel",
        "target_adu_fraction": 0.5,
        "tolerance": 1.0,
        "max_iterations": 1,
        "initial_duration": "100ms",
        "filters": filter_entries,
        // Port 0, not the omitted-block default. calibrator-flats falls back to
        // a fixed 11170, which every concurrent instance then fights over: a
        // second copy of the suite (`--runs_per_test`, a second worktree, a
        // service already running on the box) loses the bind and dies before
        // printing its address, surfacing as "failed to parse bound port".
        // Nothing reads the port from the config — the harness takes it from
        // the spawned process's `bound_addr=` line.
        "server": {
            "port": 0,
            "bind_address": "127.0.0.1"
        }
    });
    if let Some(fw) = filter_wheel_id {
        set_key(&mut config, "filter_wheel_id", serde_json::json!(fw));
    }
    config
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn empty_builder_produces_minimal_config() {
        let cfg = RpConfigBuilder::new().build();
        let equipment = cfg.get("equipment").unwrap();
        assert_eq!(
            equipment
                .get("cameras")
                .unwrap()
                .as_array()
                .unwrap()
                .as_slice(),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(
            equipment
                .get("filter_wheels")
                .unwrap()
                .as_array()
                .unwrap()
                .as_slice(),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(
            equipment
                .get("cover_calibrators")
                .unwrap()
                .as_array()
                .unwrap()
                .as_slice(),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(
            cfg.get("plugins").unwrap().as_array().unwrap().as_slice(),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(cfg["server"]["port"], 0);
        assert_eq!(cfg["server"]["bind_address"], "127.0.0.1");
    }

    #[test]
    fn filter_wheel_entry_carries_no_pairing_back_reference() {
        let mut b = RpConfigBuilder::new();
        b.add_filter_wheel(FilterWheelConfig {
            id: "main-fw".to_string(),
            alpaca_url: "http://127.0.0.1:1234".to_string(),
            device_number: 0,
            filters: vec!["Luminance".to_string()],
        });
        let cfg = b.build();
        let fw = &cfg["equipment"]["filter_wheels"][0];
        assert_eq!(fw["id"], "main-fw");
        assert!(
            fw.get("camera_id").is_none(),
            "camera pairing lives in optical_trains, not on the wheel; got: {fw}"
        );
    }

    #[test]
    fn optical_trains_empty_by_default_and_emit_in_order() {
        let cfg = RpConfigBuilder::new().build();
        assert_eq!(cfg["equipment"]["optical_trains"], serde_json::json!([]));

        let mut b = RpConfigBuilder::new();
        b.add_optical_train(OpticalTrainConfig {
            id: "main".to_string(),
            purpose: Some("imaging".to_string()),
            focal_length_mm: Some(1000.0),
            default_position_angle_degrees: None,
            devices: vec!["main-focuser".to_string(), "main-cam".to_string()],
            auto_focus: Some(TrainAutoFocusConfig {
                duration: Some("100ms".to_string()),
                step_size: 100,
                half_width: 200,
                min_area: Some(5),
                max_area: Some(65_536),
                frames_per_step: None,
            }),
        });
        b.add_optical_train(OpticalTrainConfig {
            id: "guide".to_string(),
            purpose: None,
            focal_length_mm: None,
            default_position_angle_degrees: None,
            devices: vec!["guide-cam".to_string()],
            auto_focus: None,
        });
        let cfg = b.build();
        let trains = cfg["equipment"]["optical_trains"].as_array().unwrap();
        assert_eq!(trains.len(), 2);
        assert_eq!(trains[0]["id"], "main");
        assert_eq!(trains[0]["purpose"], "imaging");
        assert_eq!(trains[0]["focal_length_mm"], 1000.0);
        assert_eq!(
            trains[0]["devices"],
            serde_json::json!(["main-focuser", "main-cam"])
        );
        assert_eq!(
            trains[0]["auto_focus"],
            serde_json::json!({
                "duration": "100ms",
                "step_size": 100,
                "half_width": 200,
                "min_area": 5,
                "max_area": 65_536,
            })
        );
        assert!(
            trains[1].get("purpose").is_none(),
            "a None purpose must omit the field so rp's imaging default applies"
        );
        assert!(
            trains[1].get("focal_length_mm").is_none(),
            "a None focal length must omit the field (no optics block)"
        );
        assert!(
            trains[1].get("auto_focus").is_none(),
            "a None auto_focus must omit the block"
        );
    }

    #[test]
    fn site_block_omitted_by_default() {
        let cfg = RpConfigBuilder::new().build();
        assert!(
            cfg.get("site").is_none(),
            "expected site key to be absent when not set, got: {:?}",
            cfg.get("site")
        );
    }

    #[test]
    fn with_site_emits_site_block() {
        let mut b = RpConfigBuilder::new();
        b.with_site(47.6062, -122.3321);
        let cfg = b.build();
        let site = cfg.get("site").expect("site block must be present");
        assert_eq!(site["latitude_degrees"], 47.6062);
        assert_eq!(site["longitude_degrees"], -122.3321);
    }

    #[test]
    fn add_plugin_accumulates() {
        let mut b = RpConfigBuilder::new();
        b.add_plugin(serde_json::json!({"name": "a", "type": "event"}));
        b.add_plugin(serde_json::json!({"name": "b", "type": "orchestrator"}));
        let cfg = b.build();
        let plugins = cfg["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0]["name"], "a");
        assert_eq!(plugins[1]["name"], "b");
    }

    #[test]
    fn empty_builder_emits_null_mount() {
        let cfg = RpConfigBuilder::new().build();
        assert!(cfg["equipment"]["mount"].is_null());
    }

    #[test]
    fn with_mount_emits_typed_block() {
        let mut b = RpConfigBuilder::new();
        b.with_mount(MountConfig {
            alpaca_url: "http://127.0.0.1:11122".to_string(),
            device_number: 0,
            settle_after_slew: Some(std::time::Duration::from_millis(150)),
        });
        let cfg = b.build();
        let mount = &cfg["equipment"]["mount"];
        assert_eq!(mount["alpaca_url"], "http://127.0.0.1:11122");
        assert_eq!(mount["device_number"], 0);
        assert_eq!(mount["settle_after_slew"], "150ms");
    }

    #[test]
    fn with_mount_omits_settle_when_none() {
        let mut b = RpConfigBuilder::new();
        b.with_mount(MountConfig {
            alpaca_url: "http://127.0.0.1:11122".to_string(),
            device_number: 0,
            settle_after_slew: None,
        });
        let cfg = b.build();
        assert!(cfg["equipment"]["mount"]["settle_after_slew"].is_null());
    }

    #[test]
    fn plate_solver_block_omitted_by_default() {
        let cfg = RpConfigBuilder::new().build();
        assert!(
            cfg.get("plate_solver").is_none(),
            "expected plate_solver key to be absent when not set, got: {:?}",
            cfg.get("plate_solver")
        );
    }

    #[test]
    fn with_plate_solver_emits_url_only_block() {
        let mut b = RpConfigBuilder::new();
        b.with_plate_solver(PlateSolverConfig {
            url: "http://127.0.0.1:11131".to_string(),
            timeout: None,
            default_search_radius_deg: None,
        });
        let cfg = b.build();
        let ps = &cfg["plate_solver"];
        assert_eq!(ps["url"], "http://127.0.0.1:11131");
        assert!(
            ps.get("timeout").is_none(),
            "expected timeout to be omitted when None"
        );
        assert!(
            ps.get("default_search_radius_deg").is_none(),
            "expected default_search_radius_deg to be omitted when None"
        );
    }

    #[test]
    fn with_plate_solver_emits_timeout_and_default_search_radius() {
        let mut b = RpConfigBuilder::new();
        b.with_plate_solver(PlateSolverConfig {
            url: "http://127.0.0.1:11131".to_string(),
            timeout: Some(std::time::Duration::from_secs(30)),
            default_search_radius_deg: Some(3.5),
        });
        let cfg = b.build();
        let ps = &cfg["plate_solver"];
        assert_eq!(ps["url"], "http://127.0.0.1:11131");
        assert_eq!(ps["timeout"], "30000ms");
        assert_eq!(ps["default_search_radius_deg"], 3.5);
    }

    #[test]
    fn guiding_block_omitted_by_default() {
        let mut b = RpConfigBuilder::new();
        b.with_mount(MountConfig {
            alpaca_url: "http://127.0.0.1:11122".to_string(),
            device_number: 0,
            settle_after_slew: None,
        });
        let cfg = b.build();
        assert!(
            cfg["equipment"]["mount"].get("guiding").is_none(),
            "expected mount.guiding to be absent when no guider is set, got: {:?}",
            cfg["equipment"]["mount"].get("guiding")
        );
    }

    #[test]
    fn with_guider_emits_url_only_block_under_the_mount() {
        let mut b = RpConfigBuilder::new();
        b.with_mount(MountConfig {
            alpaca_url: "http://127.0.0.1:11122".to_string(),
            device_number: 0,
            settle_after_slew: None,
        });
        b.with_guider(GuiderConfig::url_only("http://127.0.0.1:11130".to_string()));
        let cfg = b.build();
        assert!(
            cfg.get("guider").is_none(),
            "the retired top-level guider block must never be emitted"
        );
        let g = &cfg["equipment"]["mount"]["guiding"];
        assert_eq!(g["url"], "http://127.0.0.1:11130");
        for field in [
            "timeout",
            "settle_pixels",
            "settle_time",
            "settle_timeout",
            "dither_pixels",
            "recalibrate_above_deg",
        ] {
            assert!(
                g.get(field).is_none(),
                "expected '{field}' to be omitted when None"
            );
        }
    }

    #[test]
    fn with_guider_emits_full_overrides() {
        let mut b = RpConfigBuilder::new();
        b.with_mount(MountConfig {
            alpaca_url: "http://127.0.0.1:11122".to_string(),
            device_number: 0,
            settle_after_slew: None,
        });
        b.with_guider(GuiderConfig {
            url: "http://127.0.0.1:11130".to_string(),
            timeout: Some(std::time::Duration::from_mins(2)),
            settle_pixels: Some(0.8),
            settle_time: Some(std::time::Duration::from_secs(8)),
            settle_timeout: Some(std::time::Duration::from_secs(40)),
            dither_pixels: Some(5.0),
            recalibrate_above_deg: Some(10.0),
            focus_watch: Some(serde_json::json!({ "window": 5 })),
        });
        let cfg = b.build();
        let g = &cfg["equipment"]["mount"]["guiding"];
        assert_eq!(g["url"], "http://127.0.0.1:11130");
        assert_eq!(g["timeout"], "120000ms");
        assert_eq!(g["settle_pixels"], 0.8);
        assert_eq!(g["settle_time"], "8000ms");
        assert_eq!(g["settle_timeout"], "40000ms");
        assert_eq!(g["dither_pixels"], 5.0);
        assert_eq!(g["recalibrate_above_deg"], 10.0);
        assert_eq!(g["focus_watch"]["window"], 5);
    }

    #[test]
    #[should_panic(expected = "guiding is mount-scoped")]
    fn with_guider_without_mount_panics_at_build() {
        let mut b = RpConfigBuilder::new();
        b.with_guider(GuiderConfig::url_only("http://127.0.0.1:11130".to_string()));
        b.build();
    }

    #[test]
    fn centering_block_omitted_by_default() {
        let cfg = RpConfigBuilder::new().build();
        assert!(
            cfg.get("centering").is_none(),
            "expected centering key absent when not set, got: {:?}",
            cfg.get("centering")
        );
    }

    #[test]
    fn with_centering_emits_humantime_block() {
        let mut b = RpConfigBuilder::new();
        b.with_centering(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(500),
        );
        let cfg = b.build();
        let c = &cfg["centering"];
        assert_eq!(c["solve_time_estimate"], "1000ms");
        assert_eq!(c["slew_overhead_estimate"], "500ms");
    }

    #[test]
    fn safety_block_empty_and_no_monitors_by_default() {
        let cfg = RpConfigBuilder::new().build();
        assert_eq!(cfg["safety"], serde_json::json!({}));
        assert_eq!(
            cfg["equipment"]["safety_monitors"]
                .as_array()
                .unwrap()
                .as_slice(),
            Vec::<serde_json::Value>::new()
        );
    }

    #[test]
    fn safety_monitor_and_poll_interval_are_emitted() {
        let mut b = RpConfigBuilder::new();
        b.add_safety_monitor(SafetyMonitorConfig {
            id: "weather-watcher".to_string(),
            alpaca_url: "http://127.0.0.1:32323".to_string(),
            device_number: 0,
        });
        b.with_safety_poll_interval(std::time::Duration::from_millis(250));
        let cfg = b.build();
        let sm = &cfg["equipment"]["safety_monitors"][0];
        assert_eq!(sm["id"], "weather-watcher");
        assert_eq!(sm["alpaca_url"], "http://127.0.0.1:32323");
        assert_eq!(sm["device_number"], 0);
        assert_eq!(cfg["safety"]["poll_interval"], "250ms");
    }

    #[test]
    fn build_omits_the_retired_targets_key() {
        // The legacy `targets[]` planner array is gone; targets live in
        // the redb store (seeded post-boot via the `add_target` MCP
        // tool). The shared builder emits no `targets` key, and rp opens
        // the store with defaults when `target_store` is absent — the
        // store-settings override is injected by rp's own BDD world.
        let cfg = RpConfigBuilder::new().build();
        assert!(
            cfg.get("targets").is_none(),
            "the retired `targets` array must not be emitted"
        );
        assert!(
            cfg.get("target_store").is_none(),
            "target-store settings are injected by rp's BDD world, not the shared builder"
        );
    }

    #[test]
    fn calibrator_flats_config_embeds_plan() {
        let plan = vec![("Luminance".to_string(), 2), ("Red".to_string(), 3)];
        let cfg = build_calibrator_flats_config(&plan, Some("main-fw"));
        assert_eq!(cfg["camera_id"], "main-cam");
        assert_eq!(cfg["filter_wheel_id"], "main-fw");
        assert_eq!(cfg["calibrator_id"], "flat-panel");
        assert_eq!(cfg["max_iterations"], 1);
        assert_eq!(cfg["tolerance"], 1.0);
        let filters = cfg["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0]["name"], "Luminance");
        assert_eq!(filters[0]["count"], 2);
        assert_eq!(filters[1]["name"], "Red");
        assert_eq!(filters[1]["count"], 3);
    }

    #[test]
    fn calibrator_flats_config_omits_absent_filter_wheel() {
        let plan = vec![("OSC".to_string(), 3)];
        let cfg = build_calibrator_flats_config(&plan, None);
        assert!(
            cfg.get("filter_wheel_id").is_none(),
            "a filterless plan must omit filter_wheel_id entirely"
        );
        assert_eq!(cfg["filters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn calibrator_flats_config_asks_for_an_ephemeral_port() {
        // Omitting the block lets the service fall back to its fixed default
        // port, which collides the moment two instances exist.
        let cfg = build_calibrator_flats_config(&[("Luminance".to_string(), 1)], None);
        assert_eq!(cfg["server"]["port"], 0);
        assert_eq!(cfg["server"]["bind_address"], "127.0.0.1");
    }

    /// The default registry and data directory live in this process's
    /// scratch directory and differ from build to build.
    #[test]
    fn default_session_paths_are_distinct_and_inside_the_scratch_dir() {
        let first = RpConfigBuilder::new().build();
        let second = RpConfigBuilder::new().build();
        for key in ["session_state_file", "data_directory"] {
            let a = std::path::PathBuf::from(first["session"][key].as_str().unwrap());
            let b = std::path::PathBuf::from(second["session"][key].as_str().unwrap());
            assert_ne!(a, b, "{key} must differ between builds");
            assert_eq!(
                a.parent().unwrap(),
                scratch_dir(),
                "{key} = {}",
                a.display()
            );
            assert_eq!(
                b.parent().unwrap(),
                scratch_dir(),
                "{key} = {}",
                b.display()
            );
        }
    }

    /// A freshly built config never points rp at an existing registry — the
    /// scratch directory is created empty, so there is nothing for rp's
    /// startup recovery to restore.
    #[test]
    fn default_session_state_file_does_not_pre_exist() {
        let cfg = RpConfigBuilder::new().build();
        let path = std::path::Path::new(cfg["session"]["session_state_file"].as_str().unwrap());
        assert!(
            !path.exists(),
            "{} exists before rp ever ran",
            path.display()
        );
    }

    /// Pinned paths win over the defaults verbatim.
    #[test]
    fn pinned_session_paths_are_emitted_verbatim() {
        let mut b = RpConfigBuilder::new();
        b.with_data_directory("/pinned/data")
            .with_session_state_file("/pinned/session.json");
        let cfg = b.build();
        assert_eq!(cfg["session"]["data_directory"], "/pinned/data");
        assert_eq!(cfg["session"]["session_state_file"], "/pinned/session.json");
    }
}

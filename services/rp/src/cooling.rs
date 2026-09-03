//! Camera-cooling controller (rp.md § Camera Cooling): the body of the
//! `start_cooldown` / `start_warmup` MCP tools.
//!
//! Each camera's `cooler_targets_c` lists the dark-library setpoint
//! ladder — the only temperatures rp ever regulates at.
//! [`CoolingController::start_cooldown`] runs one background **cooldown
//! pass** per ladder camera: adopt a cooler already regulating at a
//! ladder rung, else command the lowest rung, poll
//! `CCDTemperature`/`CoolerPower`, and either stabilize there (within
//! tolerance for a full plateau window, with power headroom) or detect
//! tonight's floor (a plateau above the rung, or the rung held only at
//! pegged power) and snap **up** to the lowest rung clearing the floor
//! by the regulation margin. When no rung qualifies the cooler is
//! switched off and the night proceeds uncooled. The chosen rung is held
//! until [`CoolingController::start_warmup`] ramps the setpoint up in
//! +5 °C steps and switches the cooler off. Both entry points are
//! idempotent, return at once, and are what the tools expose; rp never
//! calls them on its own initiative (no cooler actuation at startup or
//! on a safety transition — tenet 3). `do_capture` reads
//! [`CoolingController::rung_for`] to stamp each exposure document.

use std::collections::HashMap;
use std::iter::successors;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ascom_alpaca::api::Camera;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::CoolingConfig;
use crate::equipment::EquipmentRegistry;
use crate::events::EventBus;

/// Warm-up ramp step (rp.md § Camera Cooling): +5 °C per
/// `cooling.warmup_step_interval`, matching the ladder grid.
const WARMUP_STEP_C: f64 = 5.0;

/// Which background task a camera's handle is running — what makes a
/// re-issued `start_cooldown` / `start_warmup` idempotent (a running
/// pass or ramp of the same kind is left to finish).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskKind {
    Cooldown,
    Warmup,
}

/// Per-camera cooling state. `rung_c` is the dark-library rung the
/// controller currently commands (what `do_capture` records);
/// `commanded_c` is the raw setpoint last written to the device — they
/// diverge during warm-up, when the setpoint ramps off-grid and
/// `rung_c` is already cleared.
#[derive(Default)]
struct CameraCooling {
    rung_c: Option<i32>,
    commanded_c: Option<f64>,
    task: Option<(TaskKind, tokio::task::JoinHandle<()>)>,
}

impl CameraCooling {
    /// The kind of task still running for the camera, if any.
    fn running(&self) -> Option<TaskKind> {
        self.task
            .as_ref()
            .and_then(|(kind, handle)| (!handle.is_finished()).then_some(*kind))
    }

    /// Abort whatever task is stored (a finished one is a no-op).
    fn abort_task(&mut self) {
        if let Some((_, task)) = self.task.take() {
            task.abort();
        }
    }
}

pub struct CoolingController {
    equipment: Arc<EquipmentRegistry>,
    event_bus: Arc<EventBus>,
    config: CoolingConfig,
    states: Mutex<HashMap<String, CameraCooling>>,
}

/// Sampling state for one cooldown pass: the rung currently commanded
/// and the plateau window of `(when, temperature, power)` samples
/// backing the stabilization and floor verdicts. Pure bookkeeping —
/// every device command stays in [`CoolingController::cooldown_pass`].
struct CooldownPhase<'a> {
    config: &'a CoolingConfig,
    /// The rung currently commanded, °C.
    target: i32,
    /// The floor measured by a plateau, carried into the
    /// `cooler_stabilized` payload after a snap-up. `None` when the
    /// lowest rung stabilized directly (no floor was measured).
    floor_c: Option<f64>,
    /// When the pass started (the `max_cooldown` backstop anchor).
    pass_start: Instant,
    /// When the current rung was commanded — samples must span the
    /// plateau window within one rung before any verdict.
    phase_start: Instant,
    /// Samples of the current phase within the plateau window:
    /// (when, temperature, power).
    samples: Vec<(Instant, f64, Option<f64>)>,
}

impl<'a> CooldownPhase<'a> {
    const fn new(config: &'a CoolingConfig, target: i32, now: Instant) -> Self {
        Self {
            config,
            target,
            floor_c: None,
            pass_start: now,
            phase_start: now,
            samples: Vec::new(),
        }
    }

    fn timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.pass_start) >= self.config.max_cooldown
    }

    /// Record a poll sample and drop samples older than the plateau
    /// window.
    fn record_sample(&mut self, now: Instant, temp: f64, power: Option<f64>) {
        self.samples.push((now, temp, power));
        self.samples
            .retain(|(t, _, _)| now.duration_since(*t) <= self.config.plateau_window);
    }

    /// Whether the current rung's samples span a full plateau window.
    fn window_spanned(&self, now: Instant) -> bool {
        now.duration_since(self.phase_start) >= self.config.plateau_window
            && self.samples.len() >= 2
    }

    /// The readable power samples in the window, oldest first.
    fn readable_powers(&self) -> impl Iterator<Item = f64> + '_ {
        self.samples.iter().filter_map(|(_, _, p)| *p)
    }

    /// The most recent readable power sample in the window.
    fn last_power(&self) -> Option<f64> {
        self.samples.iter().rev().find_map(|(_, _, p)| *p)
    }

    /// Stabilized verdict: a full window with every sample at the rung
    /// (within tolerance) and no sample above the power ceiling.
    fn stabilized(&self, now: Instant) -> bool {
        if !self.window_spanned(now) {
            return false;
        }
        let at_rung = self
            .samples
            .iter()
            .all(|(_, t, _)| (t - f64::from(self.target)).abs() <= self.config.tolerance_c);
        // `all` on an empty iterator is true, matching the original
        // "no readable power ⇒ the headroom criterion is disabled".
        let power_ok = self
            .readable_powers()
            .all(|p| p <= self.config.max_cooler_power_pct);
        at_rung && power_ok
    }

    /// Floor verdict: a plateau that sits above the rung, or holds it
    /// only at pegged power — or the backstop expiring — makes `temp`
    /// tonight's floor. Records it for the eventual `cooler_stabilized`
    /// payload.
    fn detect_floor(&mut self, now: Instant, temp: f64, timed_out: bool) -> Option<f64> {
        let plateaued = self.window_spanned(now) && {
            let (min, max) = self
                .samples
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), (_, t, _)| {
                    (lo.min(*t), hi.max(*t))
                });
            max - min < self.config.plateau_threshold_c
        };
        let above_rung = temp > f64::from(self.target) + self.config.tolerance_c;
        // Pegged needs at least one readable sample — peek keeps the
        // non-empty check and the all-pass in a single iterator walk,
        // block-scoped so the borrow ends before `floor_c` is written.
        let pegged = {
            let mut powers = self.readable_powers().peekable();
            powers.peek().is_some() && powers.all(|p| p > self.config.max_cooler_power_pct)
        };
        if (plateaued && (above_rung || pegged)) || timed_out {
            self.floor_c = Some(temp);
            Some(temp)
        } else {
            None
        }
    }

    /// The lowest rung clearing `floor` by the regulation margin, above
    /// the current one — selection only moves up.
    fn snap_target(&self, ladder: &[i32], floor: f64) -> Option<i32> {
        ladder
            .iter()
            .copied()
            .find(|r| f64::from(*r) >= floor + self.config.regulation_margin_c && *r > self.target)
    }

    /// Re-anchor the window on a newly commanded rung.
    fn advance(&mut self, next: i32, now: Instant) {
        self.target = next;
        self.samples.clear();
        self.phase_start = now;
    }
}

impl CoolingController {
    pub fn new(
        equipment: Arc<EquipmentRegistry>,
        event_bus: Arc<EventBus>,
        config: CoolingConfig,
    ) -> Self {
        Self {
            equipment,
            event_bus,
            config,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// The dark-library rung currently commanded for a camera — `None`
    /// when rp is not cooling it (empty ladder, skipped, uncooled after
    /// `cooler_unreachable`, or warming up).
    pub fn rung_for(&self, camera_id: &str) -> Option<i32> {
        self.lock_states()
            .get(camera_id)
            .and_then(|entry| entry.rung_c)
    }

    /// The `start_cooldown` tool (rp.md § Camera Cooling → Selection):
    /// spawn one cooldown task per ladder camera and return the camera
    /// ids it is driving. Idempotent: a cooldown pass already running
    /// for a camera is left to finish, a warm-up ramp is cancelled and
    /// superseded, and the task itself adopts a cooler already
    /// regulating at a ladder rung (see [`Self::run_cooldown`]).
    pub fn start_cooldown(self: &Arc<Self>) -> Vec<String> {
        let mut driving = Vec::new();
        for (camera_id, ladder) in self.ladder_cameras() {
            driving.push(camera_id.clone());
            if self.running_task(&camera_id) == Some(TaskKind::Cooldown) {
                debug!(
                    camera_id,
                    "cooldown pass already running; leaving it to finish"
                );
                continue;
            }
            self.abort_task(&camera_id);
            let ctrl = Arc::clone(self);
            let id = camera_id.clone();
            let handle = tokio::spawn(async move { ctrl.run_cooldown(&id, &ladder).await });
            self.store_task(&camera_id, TaskKind::Cooldown, handle);
        }
        driving
    }

    /// The `start_warmup` tool (rp.md § Camera Cooling → Warm-up): ramp
    /// every camera rp is cooling warm, then switch its cooler off, and
    /// return the camera ids being warmed. Cameras rp never commanded
    /// are untouched (and unlisted). Idempotent: a ramp already running
    /// is left to finish. rp never calls this on a safety transition —
    /// the cooler holds its rung through an interruption.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the guard covers exactly the collection loop and is dropped before any spawn; the block scope is already minimal"
    )]
    pub fn start_warmup(self: &Arc<Self>) -> Vec<String> {
        // Collect under the lock, spawn after: a spawned warm-up task
        // re-locks `states` almost immediately (`set_commanded`), and
        // holding the guard across `tokio::spawn` would block a runtime
        // worker on the mutex until the loop finishes.
        let mut warming = Vec::new();
        let to_spawn: Vec<(String, f64)> = {
            let mut states = self.lock_states();
            let mut to_spawn = Vec::new();
            for (camera_id, entry) in states.iter_mut() {
                if entry.running() == Some(TaskKind::Warmup) {
                    debug!(camera_id, "warm-up already running; leaving it to finish");
                    warming.push(camera_id.clone());
                    continue;
                }
                entry.abort_task();
                let Some(from_c) = entry.commanded_c else {
                    continue;
                };
                // Frames captured during the ramp are off the grid —
                // stop recording a rung immediately.
                entry.rung_c = None;
                warming.push(camera_id.clone());
                to_spawn.push((camera_id.clone(), from_c));
            }
            to_spawn
        };
        for (camera_id, from_c) in to_spawn {
            let ctrl = Arc::clone(self);
            let id = camera_id.clone();
            let handle = tokio::spawn(async move { ctrl.run_warmup(&id, from_c).await });
            self.store_task(&camera_id, TaskKind::Warmup, handle);
        }
        warming
    }

    /// The ladder rung a cooler is already holding — `CoolerOn` true,
    /// `SetCCDTemperature` exactly on a ladder entry, `CCDTemperature`
    /// within `tolerance_c` of it and, when readable, `CoolerPower` at
    /// or below `max_cooler_power_pct` — or `None` when it is off,
    /// off-grid, unreadable, or **commanded but not there yet**, in
    /// which case a fresh pass decides. The last case is an rp restart
    /// mid-pass: the driver already shows the lowest rung, but adopting
    /// it would skip floor detection and could leave the camera pegged
    /// at an unreachable rung all night; the pass re-commands the same
    /// rung and selects properly. The camera driver, not rp, is the
    /// source of truth for cooler state: this is how a re-issued
    /// `start_cooldown` (after an rp restart, or on a workflow's
    /// resume) gets a settled rung back without re-selecting —
    /// re-selecting mid-night would split the night across dark
    /// libraries.
    async fn adoptable_rung(
        &self,
        camera_id: &str,
        cam: &Arc<dyn Camera>,
        ladder: &[i32],
    ) -> Option<i32> {
        if !cam.cooler_on().await.unwrap_or(false) {
            return None;
        }
        // GET SetCCDTemperature — the setpoint the driver is currently
        // regulating at.
        let setpoint = match cam.set_ccd_temperature().await {
            Ok(setpoint) => setpoint,
            Err(e) => {
                debug!(camera_id, error = %e, "SetCCDTemperature read failed; nothing to adopt");
                return None;
            }
        };
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "cooler setpoints are tens of degrees; `as` saturates at the i32 rails and the ladder-membership check rejects anything absurd"
        )]
        let rung = setpoint.round() as i32;
        if (setpoint - f64::from(rung)).abs() >= 1e-6 || !ladder.contains(&rung) {
            return None;
        }
        // At the rung, not merely commanded to it: the same "at the
        // rung" and power-headroom criteria the pass's stabilized
        // verdict applies, on a single sample.
        let temp = match cam.ccd_temperature().await {
            Ok(temp) => temp,
            Err(e) => {
                debug!(camera_id, error = %e, "CCDTemperature read failed; nothing to adopt");
                return None;
            }
        };
        if (temp - setpoint).abs() > self.config.tolerance_c {
            debug!(
                camera_id,
                rung_c = rung,
                temp_c = temp,
                "cooler commanded to a configured rung but not there yet; running the pass"
            );
            return None;
        }
        if cam.can_get_cooler_power().await.unwrap_or(false) {
            if let Ok(power) = cam.cooler_power().await {
                if power > self.config.max_cooler_power_pct {
                    debug!(
                        camera_id,
                        rung_c = rung,
                        power_pct = power,
                        "cooler holds a configured rung only at pegged power; running the pass"
                    );
                    return None;
                }
            }
        }
        Some(rung)
    }

    /// One camera's `start_cooldown` task: adopt a cooler already
    /// regulating at a ladder rung (no command, no event), else run the
    /// cooldown pass.
    async fn run_cooldown(self: &Arc<Self>, camera_id: &str, ladder: &[i32]) {
        let Some(cam) = self.device(camera_id) else {
            warn!(
                camera_id,
                "cooler ladder configured but the camera is not connected; skipping cooling"
            );
            return;
        };
        if let Some(rung) = self.adoptable_rung(camera_id, &cam, ladder).await {
            // `debug!`, not `info!`: a document re-issues `start_cooldown`
            // on every start and resume, and adoption is the routine,
            // non-actionable outcome of that.
            debug!(
                camera_id,
                rung_c = rung,
                "cooler already regulating at a configured rung; adopting it without re-selecting"
            );
            self.set_rung(camera_id, rung);
            return;
        }
        match cam.can_set_ccd_temperature().await {
            Ok(true) => {}
            Ok(false) => {
                warn!(camera_id,
                      "cooler ladder configured but the camera reports CanSetCCDTemperature = false; skipping cooling");
                return;
            }
            Err(e) => {
                warn!(camera_id, error = %e, "CanSetCCDTemperature read failed; skipping cooling");
                return;
            }
        }
        // No capability probe fallback here: power is a *criterion*,
        // not a requirement — an unreadable CoolerPower only disables
        // the headroom check.
        let power_readable = match cam.can_get_cooler_power().await {
            Ok(v) => v,
            Err(e) => {
                debug!(camera_id, error = %e,
                       "CanGetCoolerPower read failed; the power-headroom criterion is disabled");
                false
            }
        };
        self.cooldown_pass(camera_id, &cam, ladder, power_readable)
            .await;
    }

    /// The single cooldown pass (rp.md § Camera Cooling → Selection at
    /// session start). Ends in exactly one of: stabilized at a rung
    /// (`cooler_stabilized`), uncooled (`cooler_unreachable`, cooler
    /// off), or an aborted command sequence (cooler off, no event).
    async fn cooldown_pass(
        self: &Arc<Self>,
        camera_id: &str,
        cam: &Arc<dyn Camera>,
        ladder: &[i32],
        power_readable: bool,
    ) {
        let Some(&lowest) = ladder.first() else {
            return;
        };
        debug!(
            camera_id,
            target_c = lowest,
            "cooldown pass: commanding the lowest rung"
        );
        // Record the commanded intent BEFORE the first mutating call: a
        // `start_warmup` racing this task (it aborts it at any await
        // point) must find `commanded_c` set once the device may have
        // been touched, so the warm-up path always takes over an
        // in-flight cooldown instead of leaving the cooler commanded.
        self.set_commanded(camera_id, f64::from(lowest));
        if let Err(e) = cam.set_set_ccd_temperature(f64::from(lowest)).await {
            warn!(camera_id, error = %e, "SetCCDTemperature failed; skipping cooling");
            self.clear_state(camera_id);
            return;
        }
        if let Err(e) = cam.set_cooler_on(true).await {
            warn!(camera_id, error = %e, "CoolerOn(true) failed; skipping cooling");
            self.clear_state(camera_id);
            return;
        }
        self.set_rung(camera_id, lowest);

        let mut phase = CooldownPhase::new(&self.config, lowest, Instant::now());
        loop {
            tokio::time::sleep(self.config.poll_interval).await;
            let now = Instant::now();
            let timed_out = phase.timed_out(now);
            let temp = match cam.ccd_temperature().await {
                Ok(t) => t,
                Err(e) => {
                    // Transient read failures skip a sample — but the
                    // backstop must still bound the pass: a camera whose
                    // temperature never reads can select nothing, and the
                    // cooler must not be left commanded indefinitely.
                    debug!(camera_id, error = %e, "CCDTemperature read failed; skipping this sample");
                    if timed_out {
                        warn!(camera_id,
                              "cooldown backstop expired without a readable CCDTemperature; switching the cooler off");
                        self.cooler_off_and_clear(camera_id, cam).await;
                        return;
                    }
                    continue;
                }
            };
            let power = if power_readable {
                cam.cooler_power().await.ok()
            } else {
                None
            };
            phase.record_sample(now, temp, power);
            debug!(camera_id, temp_c = temp, power_pct = ?power, target_c = phase.target, "cooldown poll");

            if phase.stabilized(now) {
                self.emit_stabilized(camera_id, &phase);
                return;
            }
            if timed_out {
                debug!(
                    camera_id,
                    temp_c = temp,
                    "cooldown backstop expired; treating the current temperature as the floor"
                );
            }
            // Tonight's floor, when one is detected: snap up to the
            // lowest rung clearing it by the regulation margin —
            // selection only moves up.
            let Some(floor) = phase.detect_floor(now, temp, timed_out) else {
                continue;
            };
            if let Some(next_rung) = phase.snap_target(ladder, floor) {
                debug!(
                    camera_id,
                    floor_c = floor,
                    from_c = phase.target,
                    to_c = next_rung,
                    "floor detected; snapping up to the lowest rung above it"
                );
                if let Err(e) = cam.set_set_ccd_temperature(f64::from(next_rung)).await {
                    warn!(camera_id, error = %e,
                          "SetCCDTemperature failed mid-pass; switching the cooler off");
                    self.cooler_off_and_clear(camera_id, cam).await;
                    return;
                }
                self.set_rung(camera_id, next_rung);
                phase.advance(next_rung, now);
            } else {
                warn!(camera_id, floor_c = floor, warmest_target_c = ?ladder.last(),
                      "no dark-library rung reachable tonight; switching the cooler off — the night proceeds uncooled");
                self.cooler_off_and_clear(camera_id, cam).await;
                self.event_bus.emit(
                    "cooler_unreachable",
                    serde_json::json!({
                        "camera_id": camera_id,
                        "floor_c": floor,
                        "warmest_target_c": ladder.last(),
                    }),
                );
                return;
            }
        }
    }

    /// Shared failure exit for the cooldown pass: best-effort
    /// `CoolerOn(false)`, then clear the camera's cooling state.
    async fn cooler_off_and_clear(&self, camera_id: &str, cam: &Arc<dyn Camera>) {
        if let Err(e) = cam.set_cooler_on(false).await {
            warn!(camera_id, error = %e, "CoolerOn(false) failed");
        }
        self.clear_state(camera_id);
    }

    /// Emit `cooler_stabilized` for a converged cooldown pass, with the
    /// measured floor and latest power reading when available.
    fn emit_stabilized(&self, camera_id: &str, phase: &CooldownPhase<'_>) {
        let power_pct = phase.last_power();
        info!(camera_id, target_c = phase.target, floor_c = ?phase.floor_c, power_pct = ?power_pct,
              "cooler stabilized at a dark-library rung; holding it until start_warmup");
        let mut payload = serde_json::json!({
            "camera_id": camera_id,
            "target_c": phase.target,
        });
        // `payload` is a `json!({...})` object literal, so
        // `as_object_mut` always succeeds.
        let map = payload.as_object_mut();
        debug_assert!(map.is_some(), "cooler payload must be a JSON object");
        if let Some(map) = map {
            if let Some(f) = phase.floor_c {
                map.insert("floor_c".to_owned(), serde_json::json!(f));
            }
            if let Some(p) = power_pct {
                map.insert("power_pct".to_owned(), serde_json::json!(p));
            }
        }
        self.event_bus.emit("cooler_stabilized", payload);
    }

    /// Ramp the setpoint from `from_c` up to the warm target
    /// (`HeatSinkTemperature` when readable, else the configured
    /// endpoint) in +5 °C steps, then switch the cooler off.
    async fn run_warmup(self: &Arc<Self>, camera_id: &str, from_c: f64) {
        let Some(cam) = self.device(camera_id) else {
            debug!(camera_id, "camera not connected; cannot warm up");
            self.clear_state(camera_id);
            return;
        };
        let warm_target = cam
            .heat_sink_temperature()
            .await
            .unwrap_or(self.config.warm_target_c);
        info!(
            camera_id,
            from_c,
            target_c = warm_target,
            "ramping the cooler warm"
        );
        self.event_bus.emit(
            "cooler_warmup_started",
            serde_json::json!({
                "camera_id": camera_id,
                "from_c": from_c,
                "target_c": warm_target,
            }),
        );
        // Each rung adds +5 °C and clamps to `warm_target`; the clamp lands
        // the last rung exactly on the target, which ends the sequence. A
        // NaN endpoint on either side yields no rungs. The ticker paces the
        // rungs on an absolute schedule, so late wakeups under load don't
        // stretch the ramp, and skips missed ticks rather than bursting
        // them; its period must be non-zero or `interval` panics.
        let rungs = successors(Some(from_c), |&prev| {
            (prev < warm_target).then(|| (prev + WARMUP_STEP_C).min(warm_target))
        });
        let mut ticker = tokio::time::interval(
            self.config
                .warmup_step_interval
                .max(Duration::from_millis(1)),
        );
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await;
        for setpoint in rungs.skip(1) {
            debug!(camera_id, setpoint_c = setpoint, "warm-up step");
            if let Err(e) = cam.set_set_ccd_temperature(setpoint).await {
                warn!(camera_id, error = %e, "SetCCDTemperature failed during warm-up; switching the cooler off now");
                break;
            }
            self.set_commanded(camera_id, setpoint);
            ticker.tick().await;
        }
        if let Err(e) = cam.set_cooler_on(false).await {
            warn!(camera_id, error = %e, "CoolerOn(false) failed at the end of warm-up");
        }
        self.clear_state(camera_id);
        debug!(camera_id, "warm-up complete; cooler off");
        self.event_bus.emit(
            "cooler_warmup_complete",
            serde_json::json!({ "camera_id": camera_id }),
        );
    }

    /// Every configured camera with a non-empty ladder, ladder sorted
    /// ascending (grid membership and uniqueness were validated at
    /// config load).
    fn ladder_cameras(&self) -> Vec<(String, Vec<i32>)> {
        self.equipment
            .cameras
            .iter()
            .filter(|c| !c.config.cooler_targets_c.is_empty())
            .map(|c| {
                let mut ladder = c.config.cooler_targets_c.clone();
                ladder.sort_unstable();
                (c.id.clone(), ladder)
            })
            .collect()
    }

    fn device(&self, camera_id: &str) -> Option<Arc<dyn Camera>> {
        self.equipment
            .find_camera(camera_id)
            .and_then(crate::equipment::CameraEntry::device)
    }

    fn lock_states(&self) -> std::sync::MutexGuard<'_, HashMap<String, CameraCooling>> {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The kind of task still running for the camera, if any.
    fn running_task(&self, camera_id: &str) -> Option<TaskKind> {
        self.lock_states()
            .get(camera_id)
            .and_then(CameraCooling::running)
    }

    fn abort_task(&self, camera_id: &str) {
        if let Some(entry) = self.lock_states().get_mut(camera_id) {
            entry.abort_task();
        }
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the map entry borrows the guard for the whole mutation; the scope is already minimal"
    )]
    fn store_task(&self, camera_id: &str, kind: TaskKind, task: tokio::task::JoinHandle<()>) {
        let mut states = self.lock_states();
        let entry = states.entry(camera_id.to_string()).or_default();
        entry.abort_task();
        entry.task = Some((kind, task));
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the map entry borrows the guard for the whole mutation; the scope is already minimal"
    )]
    fn set_rung(&self, camera_id: &str, rung: i32) {
        let mut states = self.lock_states();
        let entry = states.entry(camera_id.to_string()).or_default();
        entry.rung_c = Some(rung);
        entry.commanded_c = Some(f64::from(rung));
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the map entry borrows the guard for the whole mutation; the scope is already minimal"
    )]
    fn set_commanded(&self, camera_id: &str, setpoint: f64) {
        let mut states = self.lock_states();
        let entry = states.entry(camera_id.to_string()).or_default();
        entry.commanded_c = Some(setpoint);
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the map entry borrows the guard for the whole mutation; the scope is already minimal"
    )]
    fn clear_state(&self, camera_id: &str) {
        let mut states = self.lock_states();
        let entry = states.entry(camera_id.to_string()).or_default();
        entry.rung_c = None;
        entry.commanded_c = None;
    }
}

/// Shared cooler-camera stub fixtures (`CoolerSim` + `CoolingController`
/// builders) used by this module's tests and the cooling tools' — same
/// pattern as [`crate::equipment::test_support`].
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    use std::collections::HashMap;
    use std::time::Duration;

    use axum::extract::{Form, State};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    /// Scriptable cooler model behind the stub Alpaca camera. The
    /// response to a setpoint is instantaneous — with the cooler on,
    /// the temperature sits at `max(setpoint, floor)` — because the
    /// controller's plateau logic only needs *time-stable* readings —
    /// the fast timing profile below keeps each pass to a few hundred
    /// milliseconds of real time (`start_paused` virtual time would
    /// fire the Alpaca connect timeout before real socket I/O
    /// completes). The trajectory shape itself is the simulator's job
    /// (BDD, `camera_cooling.feature`).
    pub struct CoolerSim {
        pub(crate) can_set: bool,
        pub(crate) can_get_power: bool,
        /// When false the stub answers every `CCDTemperature` read with
        /// an ASCOM error — the backstop-with-no-reading regression.
        pub(crate) temp_readable: bool,
        /// `Some` makes `HeatSinkTemperature` readable (warm-up ramps to
        /// it); `None` answers `NOT_IMPLEMENTED` (fallback to config).
        pub(crate) heatsink_c: Option<f64>,
        /// Fail `SetCCDTemperature` writes once this many have
        /// succeeded (the mid-pass command-failure branch).
        pub(crate) fail_setpoint_after: Option<u32>,
        ambient_c: f64,
        /// The coldest temperature the model reaches with the cooler on
        /// (`max(setpoint, floor)`); raise it above a setpoint to model a
        /// rung the cooler was commanded to but cannot hold.
        pub(crate) floor_c: f64,
        pub(crate) setpoint_c: f64,
        pub(crate) cooler_on: bool,
        pub(crate) set_setpoint_calls: u32,
    }

    impl CoolerSim {
        pub(crate) fn new() -> Self {
            Self {
                can_set: true,
                can_get_power: true,
                temp_readable: true,
                heatsink_c: None,
                fail_setpoint_after: None,
                ambient_c: 10.0,
                floor_c: -30.0,
                setpoint_c: 0.0,
                cooler_on: false,
                set_setpoint_calls: 0,
            }
        }

        fn temperature(&self) -> f64 {
            if self.cooler_on {
                self.setpoint_c.max(self.floor_c)
            } else {
                self.ambient_c
            }
        }

        /// Linear power model: fraction of the achievable delta in
        /// use. A setpoint at (or below) the floor reads 100 %.
        fn power(&self) -> f64 {
            if !self.cooler_on {
                return 0.0;
            }
            ((self.ambient_c - self.temperature()) / (self.ambient_c - self.floor_c) * 100.0)
                .clamp(0.0, 100.0)
        }
    }

    pub type Sim = Arc<Mutex<CoolerSim>>;

    fn ok_value(value: serde_json::Value) -> Json<serde_json::Value> {
        Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
    }

    pub fn stub_router(sim: Sim) -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(json!({
                        "Value": [{
                            "DeviceName": "Camera 0",
                            "DeviceType": "Camera",
                            "DeviceNumber": 0,
                            "UniqueID": "cooler-sim-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                axum::routing::put(|| async {
                    Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                }),
            )
            .route(
                "/api/v1/camera/0/cansetccdtemperature",
                get(|State(sim): State<Sim>| async move {
                    ok_value(json!(sim.lock().unwrap().can_set))
                }),
            )
            .route(
                "/api/v1/camera/0/cangetcoolerpower",
                get(|State(sim): State<Sim>| async move {
                    ok_value(json!(sim.lock().unwrap().can_get_power))
                }),
            )
            .route(
                "/api/v1/camera/0/ccdtemperature",
                get(|State(sim): State<Sim>| async move {
                    let sim = sim.lock().unwrap();
                    if sim.temp_readable {
                        ok_value(json!(sim.temperature()))
                    } else {
                        Json(json!({ "ErrorNumber": 1024, "ErrorMessage": "not implemented" }))
                    }
                }),
            )
            .route(
                "/api/v1/camera/0/coolerpower",
                get(|State(sim): State<Sim>| async move {
                    ok_value(json!(sim.lock().unwrap().power()))
                }),
            )
            .route(
                "/api/v1/camera/0/cooleron",
                get(|State(sim): State<Sim>| async move {
                    ok_value(json!(sim.lock().unwrap().cooler_on))
                })
                .put(
                    |State(sim): State<Sim>, Form(form): Form<HashMap<String, String>>| async move {
                        let on = form
                            .get("CoolerOn")
                            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                        sim.lock().unwrap().cooler_on = on;
                        Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                    },
                ),
            )
            .route(
                "/api/v1/camera/0/setccdtemperature",
                get(|State(sim): State<Sim>| async move {
                    ok_value(json!(sim.lock().unwrap().setpoint_c))
                })
                .put(
                    |State(sim): State<Sim>, Form(form): Form<HashMap<String, String>>| async move {
                        let value: f64 = form
                            .get("SetCCDTemperature")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(f64::NAN);
                        let mut sim = sim.lock().unwrap();
                        if sim
                            .fail_setpoint_after
                            .is_some_and(|n| sim.set_setpoint_calls >= n)
                        {
                            return Json(
                                json!({ "ErrorNumber": 1035, "ErrorMessage": "simulated setpoint failure" }),
                            );
                        }
                        sim.setpoint_c = value;
                        sim.set_setpoint_calls += 1;
                        Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                    },
                ),
            )
            // HeatSinkTemperature answers NOT_IMPLEMENTED (0x400) unless
            // the sim sets `heatsink_c` — both warm-up target sources.
            .route(
                "/api/v1/camera/0/heatsinktemperature",
                get(|State(sim): State<Sim>| async move {
                    match sim.lock().unwrap().heatsink_c {
                        Some(t) => ok_value(json!(t)),
                        None => Json(
                            json!({ "ErrorNumber": 1024, "ErrorMessage": "not implemented" }),
                        ),
                    }
                }),
            )
            .with_state(sim)
    }

    /// Fast timing profile — every wait collapses under
    /// `start_paused` virtual time.
    pub fn fast_config() -> CoolingConfig {
        CoolingConfig {
            poll_interval: Duration::from_millis(50),
            plateau_window: Duration::from_millis(200),
            plateau_threshold_c: 0.5,
            tolerance_c: 1.0,
            max_cooler_power_pct: 90.0,
            regulation_margin_c: 3.0,
            max_cooldown: Duration::from_secs(10),
            warmup_step_interval: Duration::from_millis(50),
            warm_target_c: 10.0,
        }
    }

    pub async fn controller_for(
        url: &str,
        ladder: &[i32],
    ) -> (
        Arc<CoolingController>,
        tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
    ) {
        controller_with_config(url, ladder, fast_config()).await
    }

    pub async fn controller_with_config(
        url: &str,
        ladder: &[i32],
        config: CoolingConfig,
    ) -> (
        Arc<CoolingController>,
        tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
    ) {
        let equipment_config: crate::config::EquipmentConfig = serde_json::from_value(json!({
            "cameras": [{
                "id": "main-cam",
                "alpaca_url": url,
                "cooler_targets_c": ladder,
            }]
        }))
        .unwrap();
        let registry = EquipmentRegistry::new(&equipment_config, None).await;
        assert!(
            registry.cameras[0].is_connected(),
            "stub camera must connect for the test to be meaningful"
        );
        let bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let rx = bus.subscribe();
        let ctrl = Arc::new(CoolingController::new(Arc::new(registry), bus, config));
        (ctrl, rx)
    }

    pub fn drain(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
    ) -> Vec<crate::events::EventEnvelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::equipment::test_support::spawn_stub;

    use std::time::Duration;

    use serde_json::json;

    /// The stored task handle for a camera, taken out so a test can
    /// await it deterministically.
    fn take_task(ctrl: &CoolingController, camera_id: &str) -> Option<tokio::task::JoinHandle<()>> {
        ctrl.lock_states()
            .get_mut(camera_id)
            .and_then(|entry| entry.task.take())
            .map(|(_, task)| task)
    }

    #[tokio::test]
    async fn stabilizes_at_the_lowest_reachable_rung() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10, 5]).await;

        ctrl.run_cooldown("main-cam", &[-10, 5]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        {
            let sim = sim.lock().unwrap();
            assert!(sim.cooler_on, "cooler must be left on");
            assert_eq!(sim.setpoint_c, -10.0);
        }
        let events = drain(&mut rx);
        let stabilized = events
            .iter()
            .find(|e| e.event == "cooler_stabilized")
            .expect("cooler_stabilized must be emitted");
        assert_eq!(stabilized.payload["target_c"], json!(-10));
        assert_eq!(stabilized.payload["camera_id"], json!("main-cam"));
        assert!(
            stabilized.payload.get("floor_c").is_none(),
            "no floor was measured when the lowest rung stabilized directly"
        );
        assert_eq!(stabilized.payload["power_pct"], json!(50.0));
    }

    /// The rung is *held* (temperature at -30) but only at 100 % power —
    /// no regulation authority left, so -30 is tonight's floor and the
    /// controller snaps up to -10.
    #[tokio::test]
    async fn a_rung_held_only_at_pegged_power_snaps_up() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-30, -10]).await;

        ctrl.run_cooldown("main-cam", &[-30, -10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(sim.lock().unwrap().setpoint_c, -10.0);
        let events = drain(&mut rx);
        let stabilized = events
            .iter()
            .find(|e| e.event == "cooler_stabilized")
            .expect("cooler_stabilized must be emitted");
        assert_eq!(stabilized.payload["target_c"], json!(-10));
        assert_eq!(stabilized.payload["floor_c"], json!(-30.0));
    }

    /// The trajectory plateaus at the floor while still warmer than the
    /// commanded rung — the temperature-plateau branch of floor
    /// detection (the setpoint is below what the cooler can reach).
    #[tokio::test]
    async fn a_plateau_above_the_rung_snaps_up() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-40, -10]).await;

        ctrl.run_cooldown("main-cam", &[-40, -10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        let events = drain(&mut rx);
        let stabilized = events
            .iter()
            .find(|e| e.event == "cooler_stabilized")
            .expect("cooler_stabilized must be emitted");
        assert_eq!(stabilized.payload["floor_c"], json!(-30.0));
    }

    #[tokio::test]
    async fn no_reachable_rung_switches_the_cooler_off() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-30]).await;

        ctrl.run_cooldown("main-cam", &[-30]).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(
            !sim.lock().unwrap().cooler_on,
            "the cooler must be off — rp never regulates off-grid"
        );
        let events = drain(&mut rx);
        let unreachable = events
            .iter()
            .find(|e| e.event == "cooler_unreachable")
            .expect("cooler_unreachable must be emitted");
        assert_eq!(unreachable.payload["floor_c"], json!(-30.0));
        assert_eq!(unreachable.payload["warmest_target_c"], json!(-30));
        assert!(
            !events.iter().any(|e| e.event == "cooler_stabilized"),
            "an uncooled pass must not also claim stabilization"
        );
    }

    #[tokio::test]
    async fn a_camera_without_the_capability_is_skipped() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().can_set = false;
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        ctrl.run_cooldown("main-cam", &[-10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert_eq!(
            sim.lock().unwrap().set_setpoint_calls,
            0,
            "no cooler command may be issued to a camera that cannot cool"
        );
        assert!(drain(&mut rx).is_empty(), "skipping emits no events");
    }

    #[tokio::test]
    async fn an_empty_ladder_spawns_no_cooldown() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, _rx) = controller_for(&stub.url(), &[]).await;

        ctrl.start_cooldown();
        tokio::task::yield_now().await;

        assert!(
            ctrl.lock_states().is_empty(),
            "no per-camera state may be created for an empty ladder"
        );
        assert_eq!(sim.lock().unwrap().set_setpoint_calls, 0);
    }

    #[tokio::test]
    async fn warmup_ramps_in_five_degree_steps_and_switches_off() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;
        ctrl.run_cooldown("main-cam", &[-10]).await;
        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));

        ctrl.run_warmup("main-cam", -10.0).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        {
            let sim = sim.lock().unwrap();
            assert!(!sim.cooler_on, "cooler must be off after the ramp");
            // HeatSinkTemperature is unimplemented on the stub, so the
            // ramp ends at warm_target_c: -10 → -5 → 0 → 5 → 10.
            assert_eq!(sim.setpoint_c, 10.0);
        }
        let events = drain(&mut rx);
        let started = events
            .iter()
            .find(|e| e.event == "cooler_warmup_started")
            .expect("cooler_warmup_started must be emitted");
        assert_eq!(started.payload["from_c"], json!(-10.0));
        assert_eq!(started.payload["target_c"], json!(10.0));
        assert!(
            events.iter().any(|e| e.event == "cooler_warmup_complete"),
            "cooler_warmup_complete must be emitted"
        );
    }

    /// A cooler the driver still holds at a configured rung (an rp
    /// restart, or a re-issued `start_cooldown`) — at the rung, with
    /// power headroom — is adopted without commanding the device: no
    /// re-selection, which would risk splitting the night across dark
    /// libraries.
    #[tokio::test]
    async fn run_cooldown_adopts_an_on_grid_setpoint_without_commanding() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        {
            let mut sim = sim.lock().unwrap();
            sim.cooler_on = true;
            sim.setpoint_c = -10.0;
        }
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10, 5]).await;

        ctrl.run_cooldown("main-cam", &[-10, 5]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(
            sim.lock().unwrap().set_setpoint_calls,
            0,
            "re-adoption must not command the device"
        );
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| e.event == "cooler_stabilized"),
            "re-adoption must not re-announce stabilization"
        );
    }

    /// A camera whose `CCDTemperature` never reads can select nothing —
    /// the `max_cooldown` backstop must still end the pass and switch
    /// the cooler off rather than leaving it commanded indefinitely.
    #[tokio::test]
    async fn unreadable_temperature_hits_the_backstop_and_switches_off() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().temp_readable = false;
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let mut config = fast_config();
        config.max_cooldown = Duration::from_millis(200);
        let (ctrl, mut rx) = controller_with_config(&stub.url(), &[-10], config).await;

        ctrl.run_cooldown("main-cam", &[-10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(
            !sim.lock().unwrap().cooler_on,
            "the cooler must be switched off when the backstop expires without a reading"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "no selection outcome can be announced without a temperature"
        );
    }

    /// The public entry points spawn (and supersede) the per-camera
    /// tasks. Drive a whole start → hold → warm-up cycle through them,
    /// awaiting each stored task handle for determinism.
    #[tokio::test]
    async fn spawned_cooldown_then_warmup_cycle_completes() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        assert_eq!(ctrl.start_cooldown(), vec!["main-cam".to_string()]);
        let task =
            take_task(&ctrl, "main-cam").expect("start_cooldown must store the camera's task");
        task.await.unwrap();
        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));

        assert_eq!(ctrl.start_warmup(), vec!["main-cam".to_string()]);
        let task = take_task(&ctrl, "main-cam").expect("start_warmup must store the camera's task");
        task.await.unwrap();
        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(!sim.lock().unwrap().cooler_on);
        let events = drain(&mut rx);
        for expected in [
            "cooler_stabilized",
            "cooler_warmup_started",
            "cooler_warmup_complete",
        ] {
            assert!(
                events.iter().any(|e| e.event == expected),
                "missing {expected}: {:?}",
                events.iter().map(|e| &e.event).collect::<Vec<_>>()
            );
        }
    }

    /// `start_cooldown` (the spawn wrapper) adopts through a stored
    /// task, and `start_warmup` on a camera with nothing commanded is a
    /// no-op that lists no camera.
    #[tokio::test]
    async fn spawned_cooldown_adopts_and_uncommanded_warmup_is_a_noop() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        {
            let mut sim = sim.lock().unwrap();
            sim.cooler_on = true;
            sim.setpoint_c = -10.0;
        }
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10, 5]).await;

        assert_eq!(ctrl.start_cooldown(), vec!["main-cam".to_string()]);
        let task =
            take_task(&ctrl, "main-cam").expect("start_cooldown must store the camera's task");
        task.await.unwrap();
        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(
            sim.lock().unwrap().set_setpoint_calls,
            0,
            "adoption must not command the device"
        );

        // Clear the commanded state to model "nothing commanded yet":
        // warm-up must skip the camera entirely.
        ctrl.clear_state("main-cam");
        assert!(
            ctrl.start_warmup().is_empty(),
            "nothing commanded, nothing warming"
        );
        assert!(
            ctrl.lock_states()
                .get("main-cam")
                .and_then(|entry| entry.task.as_ref())
                .is_none(),
            "no warm-up task may be spawned for a camera with nothing commanded"
        );
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| e.event.starts_with("cooler_warmup")),
            "no warm-up events for a camera with nothing commanded"
        );
    }

    /// A camera that never connected is skipped by both the cooldown
    /// and the warm-up paths (nothing to command).
    #[tokio::test]
    async fn a_disconnected_camera_is_skipped() {
        let equipment_config: crate::config::EquipmentConfig = serde_json::from_value(json!({
            "cameras": [{
                "id": "main-cam",
                "alpaca_url": "http://127.0.0.1:1",
                "cooler_targets_c": [-10],
            }]
        }))
        .unwrap();
        let registry = EquipmentRegistry::new(&equipment_config, None).await;
        let bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let mut rx = bus.subscribe();
        let ctrl = Arc::new(CoolingController::new(
            Arc::new(registry),
            bus,
            fast_config(),
        ));

        ctrl.run_cooldown("main-cam", &[-10]).await;
        assert_eq!(ctrl.rung_for("main-cam"), None);

        ctrl.set_commanded("main-cam", -10.0);
        ctrl.run_warmup("main-cam", -10.0).await;
        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(drain(&mut rx).is_empty(), "a skipped camera emits nothing");
    }

    /// A failing initial `SetCCDTemperature` aborts the pass and clears
    /// the commanded state (nothing was established to warm up from).
    #[tokio::test]
    async fn a_failing_initial_setpoint_command_clears_state() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().fail_setpoint_after = Some(0);
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        ctrl.run_cooldown("main-cam", &[-10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(
            ctrl.lock_states()
                .get("main-cam")
                .and_then(|entry| entry.commanded_c)
                .is_none(),
            "a failed command sequence must not leave a commanded setpoint behind"
        );
        assert!(drain(&mut rx).is_empty());
    }

    /// A `SetCCDTemperature` failure at the snap-up point switches the
    /// cooler off instead of leaving it chasing the unreachable rung.
    #[tokio::test]
    async fn a_failing_snap_up_command_switches_the_cooler_off() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().fail_setpoint_after = Some(1);
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-30, -10]).await;

        ctrl.run_cooldown("main-cam", &[-30, -10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(
            !sim.lock().unwrap().cooler_on,
            "the cooler must be off after a failed mid-pass command"
        );
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|e| e.event == "cooler_stabilized"),
            "no stabilization may be announced after an aborted pass"
        );
    }

    /// With `CanGetCoolerPower == false` the power criterion is skipped:
    /// the rung stabilizes on temperature alone and the event carries no
    /// `power_pct`.
    #[tokio::test]
    async fn stabilizes_without_a_readable_cooler_power() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().can_get_power = false;
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        ctrl.run_cooldown("main-cam", &[-10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        let events = drain(&mut rx);
        let stabilized = events
            .iter()
            .find(|e| e.event == "cooler_stabilized")
            .expect("cooler_stabilized must be emitted");
        assert!(
            stabilized.payload.get("power_pct").is_none(),
            "no power_pct without a readable CoolerPower: {}",
            stabilized.payload
        );
    }

    /// A readable `HeatSinkTemperature` is the warm-up endpoint (the
    /// configured fallback only applies when the read fails).
    #[tokio::test]
    async fn warmup_ramps_to_the_heat_sink_temperature_when_readable() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        sim.lock().unwrap().heatsink_c = Some(20.0);
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;
        ctrl.run_cooldown("main-cam", &[-10]).await;

        ctrl.run_warmup("main-cam", -10.0).await;

        assert_eq!(sim.lock().unwrap().setpoint_c, 20.0);
        let events = drain(&mut rx);
        let started = events
            .iter()
            .find(|e| e.event == "cooler_warmup_started")
            .expect("cooler_warmup_started must be emitted");
        assert_eq!(started.payload["target_c"], json!(20.0));
    }

    /// An rp restart mid-pass: the driver shows the lowest rung
    /// commanded, but the sensor has not reached it (the model's floor
    /// sits above the rung). Adopting would skip floor detection, so
    /// the pass runs instead — and snaps up past the floor, announcing
    /// the rung it actually selects.
    #[tokio::test]
    async fn a_commanded_but_unreached_rung_is_not_adopted() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        {
            let mut sim = sim.lock().unwrap();
            sim.cooler_on = true;
            sim.setpoint_c = -10.0;
            sim.floor_c = -5.0;
        }
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10, 5]).await;

        ctrl.run_cooldown("main-cam", &[-10, 5]).await;

        assert_eq!(
            ctrl.rung_for("main-cam"),
            Some(5),
            "the pass must detect the floor and snap up, not adopt -10"
        );
        assert!(
            sim.lock().unwrap().set_setpoint_calls > 0,
            "the pass re-commands the device"
        );
        let events = drain(&mut rx);
        let stabilized = events
            .iter()
            .find(|e| e.event == "cooler_stabilized")
            .expect("the selecting pass announces its rung");
        assert_eq!(stabilized.payload["target_c"], 5);
    }

    /// A rung the cooler holds only at pegged power is not adopted
    /// either: the pass runs and snaps up.
    #[tokio::test]
    async fn a_rung_held_at_pegged_power_is_not_adopted() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        {
            let mut sim = sim.lock().unwrap();
            sim.cooler_on = true;
            sim.setpoint_c = -30.0;
        }
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-30, -10]).await;

        ctrl.run_cooldown("main-cam", &[-30, -10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert!(
            drain(&mut rx)
                .iter()
                .any(|e| e.event == "cooler_stabilized"),
            "the selecting pass announces its rung"
        );
    }

    /// A cooler that is on but regulating off the ladder is not
    /// adopted: the pass re-selects from the lowest rung.
    #[tokio::test]
    async fn an_off_grid_setpoint_is_not_adopted() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        {
            let mut sim = sim.lock().unwrap();
            sim.cooler_on = true;
            sim.setpoint_c = -7.0;
        }
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        ctrl.run_cooldown("main-cam", &[-10]).await;

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(
            sim.lock().unwrap().setpoint_c,
            -10.0,
            "the pass must re-command the rung"
        );
        assert!(
            drain(&mut rx)
                .iter()
                .any(|e| e.event == "cooler_stabilized"),
            "a fresh pass announces its rung"
        );
    }

    /// A re-issued `start_cooldown` leaves a running pass alone: one
    /// task, one `cooler_stabilized`, and the second call still lists
    /// the camera it is driving.
    #[tokio::test]
    async fn a_second_start_cooldown_leaves_a_running_pass_alone() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        assert_eq!(ctrl.start_cooldown(), vec!["main-cam".to_string()]);
        assert_eq!(ctrl.running_task("main-cam"), Some(TaskKind::Cooldown));
        assert_eq!(ctrl.start_cooldown(), vec!["main-cam".to_string()]);
        let task =
            take_task(&ctrl, "main-cam").expect("the first pass's task must still be stored");
        task.await.unwrap();

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|e| e.event == "cooler_stabilized")
                .count(),
            1,
            "the pass must run — and announce — exactly once"
        );
    }

    /// A `start_cooldown` after the pass has stabilized adopts the rung
    /// the driver still regulates at: no second pass, no second
    /// announcement, no device command.
    #[tokio::test]
    async fn a_start_cooldown_after_stabilization_adopts_silently() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;

        ctrl.start_cooldown();
        take_task(&ctrl, "main-cam").unwrap().await.unwrap();
        let commands = sim.lock().unwrap().set_setpoint_calls;
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|e| e.event == "cooler_stabilized")
                .count(),
            1
        );

        ctrl.start_cooldown();
        take_task(&ctrl, "main-cam").unwrap().await.unwrap();

        assert_eq!(ctrl.rung_for("main-cam"), Some(-10));
        assert_eq!(
            sim.lock().unwrap().set_setpoint_calls,
            commands,
            "adoption commands nothing"
        );
        assert!(drain(&mut rx).is_empty(), "adoption announces nothing");
    }

    /// A re-issued `start_warmup` leaves a running ramp alone (one
    /// `cooler_warmup_started`) while still listing the camera.
    #[tokio::test]
    async fn a_second_start_warmup_leaves_a_running_ramp_alone() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[-10]).await;
        ctrl.start_cooldown();
        take_task(&ctrl, "main-cam").unwrap().await.unwrap();

        assert_eq!(ctrl.start_warmup(), vec!["main-cam".to_string()]);
        assert_eq!(ctrl.running_task("main-cam"), Some(TaskKind::Warmup));
        assert_eq!(ctrl.start_warmup(), vec!["main-cam".to_string()]);
        take_task(&ctrl, "main-cam").unwrap().await.unwrap();

        assert_eq!(ctrl.rung_for("main-cam"), None);
        assert!(!sim.lock().unwrap().cooler_on);
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|e| e.event == "cooler_warmup_started")
                .count(),
            1,
            "the ramp must start exactly once"
        );
    }

    /// A camera with an empty ladder is neither driven nor listed.
    #[tokio::test]
    async fn start_cooldown_lists_only_ladder_cameras() {
        let sim: Sim = Arc::new(Mutex::new(CoolerSim::new()));
        let stub = spawn_stub(stub_router(sim.clone())).await;
        let (ctrl, mut rx) = controller_for(&stub.url(), &[]).await;

        assert!(ctrl.start_cooldown().is_empty());
        assert!(
            ctrl.lock_states().is_empty(),
            "no task for a ladder-less camera"
        );
        assert!(drain(&mut rx).is_empty());
    }
}

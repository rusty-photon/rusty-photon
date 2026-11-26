//! Dusk (design § Night Schedule → A night, steps 1 to 5, and
//! § Restarts, cases 4 and 5).
//!
//! Dusk waits for the roof to read safe and for the run slot, then holds
//! the slot through the power-up: it records the daily outputs that do
//! not read on, warms any cooler that runs on a daily output, has `rp`
//! power the rig up, checks each camera for a wedge and cycles a wedged
//! camera's outputs once, opens the covers and starts the cooldown.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::clock::NightWindow;
use super::dawn::{cut_outcome, dedup, later, CutOutcome, MapOutput};
use super::rules::{self, When};
use super::schedule::{until, RunSlot, Schedule};
use super::state::{NightState, OutputRef, Phase};
use crate::engine::{Clock, ToolClient};

/// How often dusk looks whether a run an operator started has let the
/// run slot go.
pub(super) const SLOT_POLL: Duration = Duration::from_secs(5);
/// How long after a failed open a cover is tried again.
pub(super) const COVER_RETRY: Duration = Duration::from_mins(5);

/// What `check_camera` said of a camera.
enum Verdict {
    Ok,
    Wedged,
    /// Not wedged and not checked: the camera is not connected, or the
    /// exposure failed some other way.
    Unavailable(String),
}

impl<C: ToolClient + Sync, K: Clock + Sync, S: RunSlot> Schedule<'_, C, K, S> {
    /// Dusk from its safety wait: once the roof reads safe and the run
    /// slot is free, the power-up and the steps that follow it. A roof
    /// that stays shut, or a slot an operator's run holds, until dawn
    /// powers nothing and starts no run.
    pub(super) async fn dusk(&mut self, ahead: NightWindow, mut state: NightState) -> Duration {
        if state.phase != Phase::Dusk {
            state.phase = Phase::Dusk;
            self.save(&state).await;
        }
        info!(night = %ahead.night, "dusk: waiting for the roof to read safe");
        if !self.wait_for_roof_and_slot(ahead.dawn, true).await {
            info!(night = %ahead.night, "dusk: the roof did not read safe, or a run held the run slot, until dawn; the night powered nothing and started no run");
            return Duration::ZERO;
        }
        self.power_up(ahead, state, true).await
    }

    /// Dusk after a restart in phase `power_up` or `camera_check`: once a
    /// run an operator started has ended, the power-up from the top and
    /// the steps that follow it. The outputs recorded before the first
    /// power-up stay recorded.
    pub(super) async fn power_up_again(
        &mut self,
        ahead: NightWindow,
        state: NightState,
    ) -> Duration {
        info!(night = %ahead.night, phase = ?state.phase, "dusk: taking the power-up up again from the top");
        if !self.wait_for_roof_and_slot(ahead.dawn, false).await {
            info!(night = %ahead.night, "dusk: a run held the run slot until dawn; the power-up did not run again and no run started");
            return Duration::ZERO;
        }
        self.power_up(ahead, state, false).await
    }

    /// Wait until the roof reads safe, when `roof`, and the schedule
    /// holds the run slot, until `dawn`: `get_safety_status` every
    /// `safety_poll_interval`, woken early by `safety_changed` to `safe`,
    /// and the slot every [`SLOT_POLL`] while a run holds it. Whether
    /// the schedule holds the slot.
    async fn wait_for_roof_and_slot(&mut self, dawn: DateTime<Utc>, roof: bool) -> bool {
        let mut logged_wait_for: Option<String> = None;
        loop {
            let now = self.clock.now();
            if now >= dawn {
                return false;
            }
            if roof {
                let overall = self
                    .call("get_safety_status", json!({}), dawn)
                    .await
                    .map(|status| {
                        status
                            .get("overall")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned()
                    });
                if overall.as_deref() != Ok("safe") {
                    debug!(?overall, "dusk: the roof does not read safe");
                    self.sleep_or_safe_again(self.safety_poll.min(until(now, dawn)))
                        .await;
                    continue;
                }
            }
            if self.slot.hold() {
                debug!("dusk: the schedule holds the run slot");
                return true;
            }
            while let Some(run_id) = self.slot.active_run() {
                if logged_wait_for.as_deref() != Some(run_id.as_str()) {
                    info!(
                        run_id,
                        "dusk: waiting for the active run to end before the power-up"
                    );
                    logged_wait_for = Some(run_id);
                }
                let now = self.clock.now();
                if now >= dawn {
                    return false;
                }
                self.clock.sleep(SLOT_POLL.min(until(now, dawn))).await;
            }
        }
    }

    /// Sleep `wait`, or less if the roof reads safe again first.
    async fn sleep_or_safe_again(&self, wait: Duration) {
        tokio::select! {
            () = self.clock.sleep(wait) => {}
            () = self.signals.safe_again.notified() => {}
        }
    }

    /// Steps 2 to 5 of dusk, the run slot held: record the daily outputs
    /// that do not read on (`first` only), warm a running cooler,
    /// `power_up`, the camera check, the covers and the cooldown. Leaves
    /// the night in phase `runs`, and lets the slot go when no run is
    /// left to start.
    async fn power_up(
        &mut self,
        ahead: NightWindow,
        mut state: NightState,
        first: bool,
    ) -> Duration {
        let dawn = ahead.dawn;
        // No run starts before phase `runs`: an index past the list here
        // is an earlier power-up's, refused as a whole, and this one may
        // go through.
        state.next_run = 0;
        let map = self.power_map(dawn).await;
        if first {
            state.outputs_off_before_dusk = map
                .iter()
                .filter(|o| o.daily && o.on != Some(true))
                .map(|o| o.output.clone())
                .collect();
        }
        state.phase = Phase::PowerUp;
        self.save(&state).await;
        self.warm_first(&map, dawn).await;

        info!(night = %ahead.night, "dusk: powering the rig up");
        let settle = humantime::format_duration(self.night.settle).to_string();
        let cameras = match self
            .call("power_up", json!({ "settle": settle }), dawn)
            .await
        {
            Ok(answer) => connected_cameras(&answer),
            Err(e) => {
                warn!(error = %e, "dusk: power_up failed as a whole; the night goes on without its runs");
                state.next_run = self.night.runs.len();
                Vec::new()
            }
        };
        state.phase = Phase::CameraCheck;
        self.save(&state).await;
        self.check_cameras(&cameras, &mut state, dawn).await;
        self.open_covers(&mut state, dawn).await;
        self.step("start_cooldown", json!({}), dawn).await;
        state.phase = Phase::Runs;
        self.save(&state).await;
        if state.next_run < self.night.runs.len() {
            info!(night = %ahead.night, "dusk: done; the night's runs start");
        } else {
            self.slot.release();
            info!(night = %ahead.night, "dusk: done; no run to start, so the run slot is free");
        }
        Duration::ZERO
    }

    /// Warm a camera on a daily output whose cooler runs before the
    /// power-up reconnects it: a `zwo-camera` connect stops a running TEC
    /// abruptly. Waits for the coolers to stop for at most
    /// `warmup_timeout`, and the power-up goes ahead either way.
    async fn warm_first(&mut self, map: &[MapOutput], dawn: DateTime<Utc>) {
        if self.running_coolers(map, dawn).await.is_empty() {
            return;
        }
        info!("dusk: a cooler runs on a daily output; warming the cameras before the power-up");
        let warmed_by = later(self.clock.now(), self.night.warmup_timeout).min(dawn);
        self.step("start_warmup", json!({}), dawn).await;
        let running = self.wait_for_coolers(map, warmed_by).await;
        if !running.is_empty() {
            warn!(cameras = ?running, "dusk: coolers still run after warmup_timeout; the power-up goes ahead");
        }
    }

    /// Check each camera the power-up connected, but those given up. A
    /// wedged camera has its outputs cycled once, and every camera on
    /// them is checked again; a second wedge, or a cycle the cut rules
    /// hold, gives a camera up for the night.
    async fn check_cameras(
        &mut self,
        cameras: &[String],
        state: &mut NightState,
        dawn: DateTime<Utc>,
    ) {
        let mut checked = BTreeSet::new();
        for camera in cameras {
            if state.cameras_given_up.contains(camera) || !checked.insert(camera.clone()) {
                continue;
            }
            match self.check_camera(camera, dawn).await {
                Verdict::Ok => debug!(camera, "camera check: ok"),
                Verdict::Unavailable(error) => {
                    warn!(camera, error, "camera check: the camera is unavailable; the night goes on without a retry");
                }
                Verdict::Wedged => {
                    warn!(
                        camera,
                        "camera check: the camera is wedged; cycling its outputs"
                    );
                    match self.cycle(camera, state, dawn).await {
                        Err(reason) => self.give_up(state, camera, &reason).await,
                        Ok(cycled) => {
                            for other in cycled {
                                if state.cameras_given_up.contains(&other) {
                                    continue;
                                }
                                checked.insert(other.clone());
                                match self.check_camera(&other, dawn).await {
                                    Verdict::Ok => {
                                        info!(camera = %other, "camera check: ok after the power cycle");
                                    }
                                    Verdict::Unavailable(error) => {
                                        warn!(camera = %other, error, "camera check: the camera is unavailable after the power cycle");
                                    }
                                    Verdict::Wedged => {
                                        self.give_up(
                                            state,
                                            &other,
                                            "it is wedged again after its outputs were cycled",
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn check_camera(&mut self, camera: &str, dawn: DateTime<Utc>) -> Verdict {
        match self
            .call("check_camera", json!({ "camera_id": camera }), dawn)
            .await
        {
            Ok(answer) => match answer.get("verdict").and_then(Value::as_str) {
                Some("ok") => Verdict::Ok,
                Some("wedged") => Verdict::Wedged,
                _ => Verdict::Unavailable(
                    answer
                        .get("error")
                        .and_then(Value::as_str)
                        .map_or_else(|| answer.to_string(), str::to_owned),
                ),
            },
            Err(e) => Verdict::Unavailable(e),
        }
    }

    /// Cycle `camera`'s daily outputs under the cut rules: `power_down`,
    /// then `power_up`, with exactly those outputs. Returns the cameras on
    /// them, each of which the cycle power-cycled, or why the camera is
    /// given up.
    async fn cycle(
        &mut self,
        camera: &str,
        state: &NightState,
        dawn: DateTime<Utc>,
    ) -> Result<Vec<String>, String> {
        let outputs: Vec<MapOutput> = self
            .power_map(dawn)
            .await
            .into_iter()
            .filter(|o| o.daily && o.devices.iter().any(|d| d.device_id == camera))
            .collect();
        if outputs.is_empty() {
            return Err("no daily output feeds it, so its power cannot be cycled".to_owned());
        }
        if let Some(o) = outputs.iter().find(|o| o.on.is_none()) {
            return Err(format!(
                "the power box `{}` cannot be read",
                o.output.switch_id
            ));
        }
        let readings = self.readings(&outputs, dawn).await;
        let off_before_dusk: BTreeSet<OutputRef> =
            state.outputs_off_before_dusk.iter().cloned().collect();
        let ctx = rules::Context {
            when: When::WedgeRetry,
            now: self.clock.now(),
            warmup_started_at: None,
            warmup_timeout: self.night.warmup_timeout,
            off_before_dusk: &off_before_dusk,
        };
        for output in &outputs {
            if let Some(reason) = rules::hold_reason(&output.mapped(), &readings, &ctx) {
                return Err(format!(
                    "the cut rules hold `{}`: {reason}",
                    output.output.output
                ));
            }
        }
        let targets: Vec<OutputRef> = outputs.iter().map(|o| o.output.clone()).collect();
        let cut = self
            .call("power_down", json!({ "outputs": targets }), dawn)
            .await;
        // Whatever the cut came to, the power-up connects again every
        // device `power_down` held out.
        let settle = humantime::format_duration(self.night.settle).to_string();
        let up = self
            .call(
                "power_up",
                json!({ "outputs": targets, "settle": settle }),
                dawn,
            )
            .await;
        let cut = cut.map_err(|e| format!("power_down failed: {e}"))?;
        for target in &targets {
            match cut_outcome(&cut, target) {
                CutOutcome::Off => {}
                CutOutcome::Failed(error) => {
                    return Err(format!("the cut of `{}` failed: {error}", target.output));
                }
                CutOutcome::NotOff => {
                    return Err(format!(
                        "`{}` does not read off after its cut",
                        target.output
                    ));
                }
            }
        }
        up.map_err(|e| format!("power_up failed: {e}"))?;
        Ok(dedup(outputs.iter().flat_map(|o| o.of_kind("camera")))
            .into_iter()
            .map(|d| d.device_id)
            .collect())
    }

    async fn give_up(&self, state: &mut NightState, camera: &str, reason: &str) {
        warn!(
            camera,
            reason, "camera check: the camera is given up for the night"
        );
        state.cameras_given_up.push(camera.to_owned());
        self.save(state).await;
    }

    /// `open_cover` for each calibrator the power map lists on an output
    /// that does not read off. A cover that fails to open is recorded,
    /// and the runs phase tries it again every 5 minutes.
    async fn open_covers(&mut self, state: &mut NightState, dawn: DateTime<Utc>) {
        let calibrators: BTreeSet<String> = self
            .power_map(dawn)
            .await
            .iter()
            .filter(|o| o.on != Some(false))
            .flat_map(|o| o.of_kind("cover_calibrator"))
            .map(|d| d.device_id.clone())
            .collect();
        let mut to_open = Vec::new();
        for calibrator in calibrators {
            // A move counts from this open on.
            self.signals.forget_moves(&calibrator);
            match self
                .call("open_cover", json!({ "calibrator_id": calibrator }), dawn)
                .await
            {
                Ok(_) => debug!(calibrator, "dusk: opened the cover"),
                Err(e) => {
                    warn!(calibrator, error = %e, "dusk: a cover did not open; it is tried again every 5 minutes until dawn, or until it moves");
                    to_open.push(calibrator);
                }
            }
        }
        if !to_open.is_empty() {
            self.memory.next_cover_retry = Some(later(self.clock.now(), COVER_RETRY));
        }
        state.covers_to_open = to_open;
        self.save(state).await;
    }
}

/// The cameras a `power_up` answer reports connected. A device that did
/// not connect, and an output that did not switch on, are logged at
/// `warn!`: the night goes on without it, while `rp`'s reconnect
/// supervisor goes on trying the device.
fn connected_cameras(answer: &Value) -> Vec<String> {
    let list = |key: &str| {
        answer
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    for output in list("outputs") {
        if output.get("error").is_some_and(|e| !e.is_null()) {
            warn!(switch_id = %text(&output, "switch_id"), output = %text(&output, "output"), error = %text(&output, "error"), "dusk: an output did not switch on");
        }
    }
    for service in list("services") {
        if service.get("error").is_some_and(|e| !e.is_null()) {
            debug!(service = %text(&service, "service"), error = %text(&service, "error"), "dusk: a service did not restart; its devices connect to the process that serves them");
        }
    }
    let mut cameras = Vec::new();
    for device in list("devices") {
        let device_id = text(&device, "device_id");
        if device.get("connected") == Some(&Value::Bool(true)) {
            if device.get("kind").and_then(Value::as_str) == Some("camera") {
                cameras.push(device_id);
            }
        } else {
            warn!(device_id, error = %text(&device, "error"), "dusk: a device did not connect; the night goes on without it while rp keeps trying");
        }
    }
    cameras
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;

    use super::super::test_rig::{at, date, done_up_to, Harness, Rig};
    use super::*;

    /// 01:00 on the 24th: night 23's dusk (00:58) has passed and its dawn
    /// (14:31) has not. The rig is as dawn left it.
    fn at_dusk() -> Harness {
        Harness::with_runs(at(24, 1), &[])
    }

    fn state(h: &Harness) -> NightState {
        h.status.snapshot().unwrap()
    }

    fn position(steps: &[String], tool: &str) -> usize {
        steps
            .iter()
            .position(|s| s == tool)
            .unwrap_or_else(|| panic!("{tool} was not called: {steps:?}"))
    }

    fn camera_output() -> Value {
        json!({ "outputs": [ { "switch_id": "power-box", "output": "Camera Power" } ] })
    }

    #[tokio::test]
    async fn dusk_waits_for_a_safe_roof_then_powers_up_checks_opens_and_cools() {
        let h = at_dusk();
        h.rp.rig.lock().unwrap().unsafe_reads = 2;
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            h.rp.steps(),
            [
                "get_safety_status",
                "get_safety_status",
                "get_safety_status",
                "get_power_map",
                "power_up",
                "check_camera",
                "get_power_map",
                "open_cover",
                "start_cooldown",
            ]
        );
        assert_eq!(
            h.clock.now(),
            at(24, 1) + chrono::TimeDelta::seconds(20),
            "two safety polls"
        );
        assert_eq!(h.rp.args_of("power_up"), [json!({ "settle": "30s" })]);
        let rig = h.rp.rig.lock().unwrap();
        assert!(rig
            .outputs
            .iter()
            .filter(|o| o.daily)
            .all(|o| o.on == Some(true)));
        assert_eq!(rig.covers["flat-panel"], "Open");
        drop(rig);
        assert_eq!(state(&h).phase, Phase::Runs);
        assert!(
            !h.slot.held.load(Ordering::SeqCst),
            "no run to start: the slot is free once the cooldown has begun"
        );
    }

    #[tokio::test]
    async fn a_roof_shut_until_dawn_powers_nothing_and_dawn_still_runs() {
        let h = at_dusk();
        h.rp.rig.lock().unwrap().unsafe_reads = u32::MAX;
        let mut schedule = h.schedule();

        schedule.tick().await;
        assert!(h.clock.now() >= at(24, 14) + chrono::TimeDelta::minutes(31));
        assert_eq!(state(&h).phase, Phase::Dusk);
        assert_eq!(h.rp.called("power_up"), 0);
        assert!(!h.slot.held.load(Ordering::SeqCst));

        schedule.tick().await;
        assert_eq!(h.rp.called("park"), 1, "dawn ran");
    }

    #[tokio::test]
    async fn dusk_waits_for_an_operators_run_before_it_takes_the_slot() {
        let h = at_dusk();
        h.slot.operator_run("operator-run", 3);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert!(h.clock.now() > at(24, 1), "dusk waited");
        assert_eq!(h.rp.called("power_up"), 1);
        assert_eq!(h.slot.active_run(), None);
    }

    #[tokio::test]
    async fn a_running_cooler_is_warmed_before_the_power_up() {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            *rig = Rig::default();
            rig.covers.insert("flat-panel".into(), "Closed".into());
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        let steps = h.rp.steps();
        let warmup = position(&steps, "start_warmup");
        let power_up = position(&steps, "power_up");
        assert!(warmup < power_up, "{steps:?}");
        assert_eq!(
            steps[warmup + 1..power_up]
                .iter()
                .filter(|s| *s == "get_camera_info")
                .count(),
            3,
            "read until the cooler stops: {steps:?}"
        );
        assert_eq!(h.rp.rig.lock().unwrap().coolers["main-cam"], Some(false));
    }

    #[tokio::test]
    async fn a_cooler_that_keeps_running_is_waited_for_until_warmup_timeout_and_the_power_up_goes_on(
    ) {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.output_mut("Camera Power").on = Some(true);
            rig.held_out.remove("main-cam");
            rig.coolers.insert("main-cam".into(), Some(true));
            rig.reads_to_cool = None;
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("start_warmup"), 1);
        assert_eq!(h.rp.called("power_up"), 1);
        assert!(h.clock.now() >= at(24, 1) + chrono::TimeDelta::minutes(45));
    }

    #[tokio::test]
    async fn the_daily_outputs_that_do_not_read_on_before_the_power_up_are_recorded() {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.output_mut("Panel Power").on = Some(true);
            rig.output_mut("Mount Power").on = None;
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        let off: Vec<String> = state(&h)
            .outputs_off_before_dusk
            .into_iter()
            .map(|o| o.output)
            .collect();
        assert_eq!(off, ["Camera Power", "Mount Power"]);
    }

    #[tokio::test]
    async fn a_power_up_refused_as_a_whole_leaves_the_night_without_its_runs() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        h.rp.rig.lock().unwrap().power_up_refused = true;
        let mut schedule = h.schedule();

        schedule.tick().await;
        schedule.tick().await;

        assert_eq!(h.rp.called("check_camera"), 0);
        assert_eq!(
            h.rp.called("open_cover"),
            0,
            "the panel's output reads off: no power to open with"
        );
        assert_eq!(h.rp.called("start_cooldown"), 1, "the night goes on");
        assert_eq!(h.slot.sessions(), Vec::<String>::new());
        assert!(!h.slot.held.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_power_up_taken_up_again_after_one_refused_starts_the_runs() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        h.rp.rig.lock().unwrap().power_up_refused = true;
        let mut schedule = h.schedule();
        schedule.tick().await;
        let mut restarted = state(&h);
        assert_eq!(restarted.next_run, 1, "no run is left to start");

        h.rp.rig.lock().unwrap().power_up_refused = false;
        restarted.phase = Phase::CameraCheck;
        h.status.publish(restarted);
        let mut schedule = h.schedule();
        schedule.tick().await;
        schedule.tick().await;

        assert_eq!(h.slot.sessions(), ["night-2026-11-23-0"]);
    }

    #[tokio::test]
    async fn a_wedged_camera_has_exactly_its_outputs_cycled_and_is_checked_again() {
        let h = at_dusk();
        h.rp.rig
            .lock()
            .unwrap()
            .verdicts
            .insert("main-cam".into(), vec!["wedged"]);
        let mut schedule = h.schedule();

        schedule.tick().await;

        let steps = h.rp.steps();
        let first = position(&steps, "check_camera");
        assert_eq!(
            steps[first..first + 6],
            [
                "check_camera",
                "get_power_map",
                "get_camera_info",
                "power_down",
                "power_up",
                "check_camera"
            ]
        );
        assert_eq!(h.rp.args_of("power_down"), [camera_output()]);
        let mut cycle_up = camera_output();
        cycle_up["settle"] = json!("30s");
        assert_eq!(h.rp.args_of("power_up")[1], cycle_up);
        assert_eq!(state(&h).cameras_given_up, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_camera_wedged_after_its_cycle_is_given_up_and_not_checked_after_a_restart() {
        let h = at_dusk();
        h.rp.rig
            .lock()
            .unwrap()
            .verdicts
            .insert("main-cam".into(), vec!["wedged", "wedged"]);
        let mut schedule = h.schedule();

        schedule.tick().await;
        assert_eq!(state(&h).cameras_given_up, ["main-cam"]);
        assert_eq!(h.rp.called("check_camera"), 2);

        let mut restarted = state(&h);
        restarted.phase = Phase::CameraCheck;
        h.status.publish(restarted);
        let mut schedule = h.schedule();
        schedule.tick().await;

        assert_eq!(h.rp.called("power_up"), 3, "the power-up ran again");
        assert_eq!(h.rp.called("check_camera"), 2, "the camera stays given up");
    }

    #[tokio::test]
    async fn a_wedged_camera_whose_output_the_cut_rules_hold_is_given_up_uncycled() {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.output_mut("Camera Power")
                .devices
                .push(("mount", "mount"));
            rig.pose_kept = Some(false);
            rig.verdicts.insert("main-cam".into(), vec!["wedged"]);
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("power_down"), 0);
        assert_eq!(state(&h).cameras_given_up, ["main-cam"]);
    }

    #[tokio::test]
    async fn on_an_output_off_before_dusk_a_wedge_retry_cuts_an_unparked_mount_that_keeps_its_pose()
    {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.output_mut("Camera Power")
                .devices
                .push(("mount", "mount"));
            rig.at_park = false;
            rig.verdicts.insert("main-cam".into(), vec!["wedged"]);
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.args_of("power_down"), [camera_output()]);
        assert_eq!(state(&h).cameras_given_up, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_wedge_retry_whose_cut_fails_gives_the_camera_up_and_connects_its_devices_again() {
        let h = at_dusk();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.cuts_fail.insert("Camera Power".into());
            rig.verdicts.insert("main-cam".into(), vec!["wedged"]);
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(state(&h).cameras_given_up, ["main-cam"]);
        assert_eq!(
            h.rp.called("power_up"),
            2,
            "the cycle's power-up ran all the same"
        );
        assert!(
            !h.rp.rig.lock().unwrap().held_out.contains("main-cam"),
            "the camera power_down held out is connected again"
        );
    }

    #[tokio::test]
    async fn an_unavailable_camera_is_not_cycled() {
        let h = at_dusk();
        h.rp.rig
            .lock()
            .unwrap()
            .verdicts
            .insert("main-cam".into(), vec!["unavailable"]);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("check_camera"), 1);
        assert_eq!(h.rp.called("power_down"), 0);
        assert_eq!(state(&h).cameras_given_up, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_camera_the_power_up_did_not_connect_takes_no_check() {
        let h = at_dusk();
        h.rp.rig
            .lock()
            .unwrap()
            .connect_fails
            .insert("main-cam".into());
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("check_camera"), 0);
        assert_eq!(state(&h).phase, Phase::Runs);
    }

    #[tokio::test]
    async fn a_cover_that_fails_to_open_is_recorded_as_still_to_open() {
        let h = at_dusk();
        h.rp.rig.lock().unwrap().opens_fail = true;
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(state(&h).covers_to_open, ["flat-panel"]);
        assert_eq!(h.rp.called("start_cooldown"), 1, "the night goes on");
    }

    #[tokio::test]
    async fn a_restart_in_phase_dusk_goes_back_to_the_safety_wait() {
        let h = at_dusk();
        let mut dusk = done_up_to(23);
        dusk.phase = Phase::Dusk;
        h.status.publish(dusk);
        h.rp.rig.lock().unwrap().unsafe_reads = 1;
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            h.rp.steps()[..2],
            ["get_safety_status", "get_safety_status"]
        );
        assert_eq!(h.rp.called("power_up"), 1);
    }

    #[tokio::test]
    async fn a_restart_in_the_power_up_runs_it_from_the_top_with_the_record_kept() {
        let h = at_dusk();
        h.rp.rig.lock().unwrap().unsafe_reads = u32::MAX;
        for output in &mut h.rp.rig.lock().unwrap().outputs {
            output.on = Some(true);
        }
        let mut power_up = done_up_to(23);
        power_up.phase = Phase::PowerUp;
        power_up.outputs_off_before_dusk = vec![OutputRef {
            switch_id: "power-box".into(),
            output: "Camera Power".into(),
        }];
        h.status.publish(power_up.clone());
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("get_safety_status"), 0, "no safety wait");
        assert_eq!(h.rp.called("power_up"), 1);
        assert_eq!(
            state(&h).outputs_off_before_dusk,
            power_up.outputs_off_before_dusk
        );
        assert_eq!(state(&h).night, date(23));
    }
}

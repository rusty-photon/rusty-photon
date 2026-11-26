//! Dawn and the after-dawn checks (design § Night Schedule → A night,
//! steps 7 and 8, and § The cut rules).
//!
//! Dawn ends the active run, warms the cameras, parks the mount, closes
//! the covers, waits for the coolers to stop and has `rp` cut every daily
//! output the cut rules allow. The checks that follow look at each output
//! dawn left on every 5 minutes until the next dusk, and cut it once its
//! reason has cleared.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use super::clock::NightWindow;
use super::rules::{
    self, CameraReading, CoverReading, MappedDevice, MappedOutput, ParkReading, Readings, When,
};
use super::schedule::{until, NightMemory, Schedule, INITIAL_BACKOFF, MAX_BACKOFF};
use super::state::{LeftOn, NightState, OutputRef, Phase};
use crate::engine::{Clock, ToolCallError, ToolClient};

/// How long dawn waits for the active run to end once asked to stop.
const RUN_END_WAIT: Duration = Duration::from_mins(10);
/// How often dawn looks whether the stopped run has ended.
const RUN_END_POLL: Duration = Duration::from_secs(1);
/// How often dawn reads the coolers while it waits for them to stop.
const COOLER_POLL: Duration = Duration::from_secs(30);
/// How often the after-dawn checks run.
pub(super) const CHECK_EVERY: Duration = Duration::from_mins(5);
/// How many times the after-dawn checks call `park` again, and
/// `close_cover` again for one cover.
const MAX_RETRIES: u32 = 3;

/// The run slot as the schedule uses it.
pub trait RunSlot: Sync {
    /// The run that holds the slot, if any.
    fn active_run(&self) -> Option<String>;
    /// End a run as `POST /runs/{id}/stop` ends it.
    fn stop_run(&self, run_id: &str);
    /// Take the slot; `false` while a run holds it.
    fn hold(&self) -> bool;
    fn release(&self);
}

impl RunSlot for crate::runs::RunRegistry {
    fn active_run(&self) -> Option<String> {
        self.active()
    }

    fn stop_run(&self, run_id: &str) {
        if let Err(e) = self.stop(run_id) {
            debug!(run_id, error = ?e, "the run ended before dawn could stop it");
        }
    }

    fn hold(&self) -> bool {
        self.hold_for_night()
    }

    fn release(&self) {
        self.release_night();
    }
}

/// One output of `get_power_map`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MapOutput {
    pub(super) output: OutputRef,
    pub(super) daily: bool,
    /// `None` when `rp` cannot read the box.
    pub(super) on: Option<bool>,
    pub(super) devices: Vec<MapDevice>,
}

/// One device on a [`MapOutput`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MapDevice {
    pub(super) device_id: String,
    pub(super) kind: String,
    pub(super) connected: bool,
    pub(super) held_out: bool,
}

impl MapOutput {
    fn mapped(&self) -> MappedOutput {
        MappedOutput {
            output: self.output.clone(),
            devices: self
                .devices
                .iter()
                .map(|d| MappedDevice {
                    device_id: d.device_id.clone(),
                    kind: d.kind.clone(),
                })
                .collect(),
        }
    }

    fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a MapDevice> + 'a {
        self.devices.iter().filter(move |d| d.kind == kind)
    }
}

/// Read a `get_power_map` answer.
///
/// # Errors
///
/// A message when the answer carries no `outputs` array, or an output
/// without its switch id or name.
pub(super) fn parse_power_map(answer: &Value) -> Result<Vec<MapOutput>, String> {
    let outputs = answer
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("get_power_map answered without outputs: {answer}"))?;
    outputs
        .iter()
        .map(|o| {
            let text = |key: &str| {
                o.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("a power map output without {key}: {o}"))
            };
            let devices = o
                .get("devices")
                .and_then(Value::as_array)
                .map_or_default(|devices| {
                    devices
                        .iter()
                        .filter_map(|d| {
                            Some(MapDevice {
                                device_id: d.get("device_id")?.as_str()?.to_owned(),
                                kind: d.get("kind")?.as_str()?.to_owned(),
                                connected: d.get("connected").and_then(Value::as_bool)
                                    == Some(true),
                                held_out: d.get("held_out").and_then(Value::as_bool) == Some(true),
                            })
                        })
                        .collect()
                });
            Ok(MapOutput {
                output: OutputRef {
                    switch_id: text("switch_id")?,
                    output: text("output")?,
                },
                daily: o.get("daily").and_then(Value::as_bool) == Some(true),
                on: o.get("on").and_then(Value::as_bool),
                devices,
            })
        })
        .collect()
}

/// `then` moved on by `d`, or `then` itself past chrono's range.
fn later(then: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    chrono::TimeDelta::from_std(d)
        .ok()
        .and_then(|d| then.checked_add_signed(d))
        .unwrap_or(then)
}

impl<C: ToolClient + Sync, K: Clock + Sync, S: RunSlot> Schedule<'_, C, K, S> {
    /// Run dawn for `latest`: end the active run, warm the cameras, park,
    /// close the covers, wait for the coolers, and cut what the rules
    /// allow. Every step is an ungated tool, retried while `rp` is
    /// unreachable until `ahead`'s dusk.
    pub(super) async fn dawn(&mut self, latest: NightWindow, ahead: NightWindow) {
        let deadline = ahead.dusk;
        let mut state = match self.status.snapshot() {
            Some(s) if s.night == latest.night => s,
            _ => NightState::ahead(latest.night, latest.dusk, latest.dawn),
        };
        self.memory = NightMemory::default();
        // A comeback counts from this dawn on: one noticed on an earlier
        // day says nothing about the coolers this dawn leaves running.
        self.rp_came_back = false;
        self.rp_was_down = false;
        self.seen_connects = self.connects.load(std::sync::atomic::Ordering::SeqCst);
        state.phase = Phase::Dawn;
        state.outputs_left_on.clear();
        self.save(&state).await;
        info!(night = %latest.night, "dawn: ending the night");

        let held = self.end_active_run().await;
        state.warmup_started_at = Some(self.clock.now());
        self.save(&state).await;
        self.step("start_warmup", json!({}), deadline).await;
        self.step("park", json!({}), deadline).await;
        let map = self.power_map(deadline).await;
        let calibrators: BTreeSet<String> = map
            .iter()
            .filter(|o| o.on != Some(false))
            .flat_map(|o| o.of_kind("cover_calibrator"))
            .map(|d| d.device_id.clone())
            .collect();
        for calibrator in calibrators {
            self.step(
                "close_cover",
                json!({ "calibrator_id": calibrator }),
                deadline,
            )
            .await;
        }
        let warmed_by = later(
            state.warmup_started_at.unwrap_or_else(|| self.clock.now()),
            self.night.warmup_timeout,
        );
        self.wait_for_coolers(&map, warmed_by).await;

        let candidates: Vec<MapOutput> = self
            .power_map(deadline)
            .await
            .into_iter()
            .filter(|o| o.daily && o.on != Some(false))
            .collect();
        state.outputs_left_on = self.cut(&candidates, When::Dawn, &state, deadline).await;
        state.phase = Phase::AfterDawn;
        self.save(&state).await;
        if held {
            self.slot.release();
        }
        self.memory.next_check = Some(later(self.clock.now(), CHECK_EVERY));
        if state.outputs_left_on.is_empty() {
            info!(night = %latest.night, "dawn: every daily output is off");
            self.keep_on(ahead).await;
        } else {
            info!(
                night = %latest.night,
                left_on = state.outputs_left_on.len(),
                "dawn: done; the outputs left on are checked every 5 minutes until dusk"
            );
        }
    }

    /// The after-dawn checks, when they are due: each output dawn left on
    /// is cut once its reason has cleared. Returns how long until the next
    /// pass.
    pub(super) async fn after_dawn(&mut self, ahead: NightWindow) -> Duration {
        let now = self.clock.now();
        let deadline = ahead.dusk;
        let Some(mut state) = self.status.snapshot() else {
            return Duration::ZERO;
        };
        if state.outputs_left_on.is_empty() {
            self.keep_on(ahead).await;
            return Duration::ZERO;
        }
        if let Some(next) = self.memory.next_check {
            if now < next {
                return until(now, next).min(until(now, deadline));
            }
        }
        self.memory.next_check = Some(later(now, CHECK_EVERY));
        self.note_rp_restart();

        let (until_dusk, watched): (Vec<LeftOn>, Vec<LeftOn>) =
            state.outputs_left_on.iter().cloned().partition(|l| {
                self.memory.until_dusk.contains(&OutputRef {
                    switch_id: l.switch_id.clone(),
                    output: l.output.clone(),
                })
            });
        let mut candidates = self.watched_outputs(&watched, deadline).await;
        let reconnect: Vec<MapOutput> = candidates
            .iter()
            .filter(|o| self.memory.reconnect_first.contains(&o.output))
            .cloned()
            .collect();
        if self.reconnect_held_out(&reconnect, deadline).await {
            candidates = self.watched_outputs(&watched, deadline).await;
        }
        self.retry_park_and_covers(&candidates, deadline).await;
        let cut_from: Vec<MapOutput> = candidates
            .into_iter()
            .filter(|o| !self.memory.until_dusk.contains(&o.output))
            .collect();
        let mut left_on: Vec<LeftOn> = until_dusk;
        left_on.extend(watched.into_iter().filter(|l| {
            self.memory.until_dusk.contains(&OutputRef {
                switch_id: l.switch_id.clone(),
                output: l.output.clone(),
            })
        }));
        left_on.extend(self.cut(&cut_from, When::AfterDawn, &state, deadline).await);
        state.outputs_left_on = left_on;
        self.save(&state).await;
        if state.outputs_left_on.is_empty() {
            info!("after dawn: every output dawn left on is off");
            self.keep_on(ahead).await;
            return Duration::ZERO;
        }
        CHECK_EVERY.min(until(self.clock.now(), deadline))
    }

    /// The power map's view of the outputs left on that still need a
    /// check: one no longer in the map, or one that reads off, is done.
    async fn watched_outputs(
        &mut self,
        watched: &[LeftOn],
        deadline: DateTime<Utc>,
    ) -> Vec<MapOutput> {
        let map = self.power_map(deadline).await;
        watched
            .iter()
            .filter_map(|l| {
                let found = map
                    .iter()
                    .find(|o| o.output.switch_id == l.switch_id && o.output.output == l.output);
                match found {
                    None => {
                        debug!(switch_id = %l.switch_id, output = %l.output, "an output dawn left on has left the power map");
                        None
                    }
                    Some(o) if o.on == Some(false) => {
                        info!(switch_id = %l.switch_id, output = %l.output, "an output dawn left on reads off");
                        None
                    }
                    Some(o) => Some(o.clone()),
                }
            })
            .collect()
    }

    /// Connect again every held-out device on `outputs`, the outputs whose
    /// cut failed or whose box could not be read: `power_down` held their
    /// devices out before the cut, and a mount still held out fails every
    /// park and every pose read. Whether any was connected.
    async fn reconnect_held_out(&mut self, outputs: &[MapOutput], deadline: DateTime<Utc>) -> bool {
        let held_out: BTreeSet<String> = outputs
            .iter()
            .flat_map(|o| o.devices.iter())
            .filter(|d| d.held_out)
            .map(|d| d.device_id.clone())
            .collect();
        for device_id in &held_out {
            match self
                .call(
                    "connect_device",
                    json!({ "device_id": device_id }),
                    deadline,
                )
                .await
            {
                Ok(_) => debug!(device_id, "after dawn: connected a held-out device again"),
                Err(e) => {
                    debug!(device_id, error = %e, "after dawn: a held-out device did not connect");
                }
            }
        }
        !held_out.is_empty()
    }

    /// Park again while the mount on an output left on does not read
    /// parked, and close again a cover that does not read closed, three
    /// times each at most; after that the output stays on until dusk.
    /// After an `rp` restart, warm once the cameras whose cooler runs.
    async fn retry_park_and_covers(&mut self, outputs: &[MapOutput], deadline: DateTime<Utc>) {
        let readings = self.readings(outputs, deadline).await;
        let mount_outputs: Vec<OutputRef> = outputs
            .iter()
            .filter(|o| o.of_kind("mount").next().is_some())
            .map(|o| o.output.clone())
            .collect();
        let parked = matches!(readings.park, Some(ParkReading::Read { at_park: true, .. }));
        if !mount_outputs.is_empty() && !parked {
            if self.memory.parks < MAX_RETRIES {
                self.memory.parks = self.memory.parks.saturating_add(1);
                debug!(
                    attempt = self.memory.parks,
                    "after dawn: parking the mount again"
                );
                if let Err(e) = self.call("park", json!({}), deadline).await {
                    debug!(error = %e, "after dawn: the park failed");
                }
            } else if mount_outputs
                .iter()
                .any(|o| !self.memory.until_dusk.contains(o))
            {
                warn!("the mount stays unparked after three more parks; its output stays on until dusk");
                self.memory.until_dusk.extend(mount_outputs);
            }
        }
        for output in outputs {
            for cover in output.of_kind("cover_calibrator") {
                let closed = matches!(
                    readings.covers.get(&cover.device_id),
                    Some(CoverReading::State(s)) if s == "Closed" || s == "NotPresent"
                );
                if closed {
                    continue;
                }
                let tries = self
                    .memory
                    .closes
                    .entry(cover.device_id.clone())
                    .or_default();
                if *tries < MAX_RETRIES {
                    *tries = tries.saturating_add(1);
                    debug!(calibrator_id = %cover.device_id, attempt = *tries, "after dawn: closing the cover again");
                    if let Err(e) = self
                        .call(
                            "close_cover",
                            json!({ "calibrator_id": cover.device_id }),
                            deadline,
                        )
                        .await
                    {
                        debug!(calibrator_id = %cover.device_id, error = %e, "after dawn: the close failed");
                    }
                } else if self.memory.until_dusk.insert(output.output.clone()) {
                    warn!(calibrator_id = %cover.device_id, "the cover stays open after three more closes; its output stays on until dusk");
                }
            }
        }
        let cooling = readings
            .cameras
            .values()
            .any(|r| *r == CameraReading::Cooler(Some(true)));
        if self.rp_came_back && cooling {
            info!("rp is back from a restart while a cooler holds an output; warming the cameras again");
            self.step("start_warmup", json!({}), deadline).await;
        }
        self.rp_came_back = false;
    }

    /// Stop the active run, wait 10 minutes at most for it to end, then
    /// take the run slot. Whether the slot is held.
    async fn end_active_run(&self) -> bool {
        if let Some(run_id) = self.slot.active_run() {
            info!(run_id, "dawn: stopping the active run");
            self.slot.stop_run(&run_id);
            let asked = self.clock.now();
            while self.slot.active_run().is_some() {
                if until(asked, self.clock.now()) >= RUN_END_WAIT {
                    warn!(run_id, "dawn: the run has not ended 10 minutes after it was stopped; dawn goes on and leaves it to end");
                    return false;
                }
                self.clock.sleep(RUN_END_POLL).await;
            }
        }
        self.slot.hold()
    }

    /// Wait until no camera on a daily output reads its cooler running,
    /// polled every 30 s and woken early by `cooler_warmup_complete`,
    /// until `warmed_by`.
    async fn wait_for_coolers(&mut self, map: &[MapOutput], warmed_by: DateTime<Utc>) {
        let cameras: BTreeSet<String> = map
            .iter()
            .filter(|o| o.daily && o.on != Some(false))
            .flat_map(|o| o.of_kind("camera"))
            .map(|d| d.device_id.clone())
            .collect();
        loop {
            let mut running = Vec::new();
            for camera in &cameras {
                let info = self
                    .call("get_camera_info", json!({ "camera_id": camera }), warmed_by)
                    .await;
                if info.is_ok_and(|v| v.get("cooler_running") == Some(&Value::Bool(true))) {
                    running.push(camera.clone());
                }
            }
            if running.is_empty() {
                debug!("dawn: no cooler on a daily output runs");
                return;
            }
            let now = self.clock.now();
            if now >= warmed_by {
                warn!(cameras = ?running, "dawn: coolers still run after warmup_timeout; the cut rules hold their outputs");
                return;
            }
            debug!(cameras = ?running, "dawn: waiting for the coolers to stop");
            self.sleep_or_warmup_done(COOLER_POLL.min(until(now, warmed_by)))
                .await;
        }
    }

    /// Sleep `wait`, or less if a `cooler_warmup_complete` arrives first.
    async fn sleep_or_warmup_done(&self, wait: Duration) {
        tokio::select! {
            () = self.clock.sleep(wait) => {}
            () = self.warmup_done.notified() => {}
        }
    }

    /// Apply the cut rules to `outputs`, hand `power_down` the ones they
    /// allow, and return the outputs left on with their reasons.
    async fn cut(
        &mut self,
        outputs: &[MapOutput],
        when: When,
        state: &NightState,
        deadline: DateTime<Utc>,
    ) -> Vec<LeftOn> {
        if outputs.is_empty() {
            return Vec::new();
        }
        let readings = self.readings(outputs, deadline).await;
        let off_before_dusk: BTreeSet<OutputRef> =
            state.outputs_off_before_dusk.iter().cloned().collect();
        let ctx = rules::Context {
            when,
            now: self.clock.now(),
            warmup_started_at: state.warmup_started_at,
            warmup_timeout: self.night.warmup_timeout,
            off_before_dusk: &off_before_dusk,
        };
        let mut left_on = Vec::new();
        let mut allowed = Vec::new();
        for output in outputs {
            if output.on.is_none() {
                self.memory.reconnect_first.insert(output.output.clone());
                left_on.push(leave(
                    &output.output,
                    format!("the power box `{}` cannot be read", output.output.switch_id),
                ));
                continue;
            }
            match rules::hold_reason(&output.mapped(), &readings, &ctx) {
                Some(reason) => left_on.push(leave(&output.output, reason)),
                None => allowed.push(output.output.clone()),
            }
        }
        if !allowed.is_empty() {
            match self
                .call("power_down", json!({ "outputs": allowed }), deadline)
                .await
            {
                Ok(result) => {
                    for output in &allowed {
                        let reported =
                            result
                                .get("outputs")
                                .and_then(Value::as_array)
                                .and_then(|outs| {
                                    outs.iter().find(|o| {
                                        o.get("switch_id").and_then(Value::as_str)
                                            == Some(output.switch_id.as_str())
                                            && o.get("output").and_then(Value::as_str)
                                                == Some(output.output.as_str())
                                    })
                                });
                        match reported {
                            Some(o) if o.get("error").is_some_and(|e| !e.is_null()) => {
                                let error = o["error"].as_str().unwrap_or("an error");
                                self.memory.reconnect_first.insert(output.clone());
                                left_on.push(leave(output, format!("its cut failed: {error}")));
                            }
                            Some(o) if o.get("on") == Some(&Value::Bool(false)) => {
                                self.memory.logged_left_on.remove(output);
                                self.memory.reconnect_first.remove(output);
                                info!(switch_id = %output.switch_id, output = %output.output, "cut a daily output");
                            }
                            _ => {
                                self.memory.reconnect_first.insert(output.clone());
                                left_on.push(leave(
                                    output,
                                    "it does not read off after its cut".to_owned(),
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    for output in &allowed {
                        self.memory.reconnect_first.insert(output.clone());
                        left_on.push(leave(output, format!("power_down failed: {e}")));
                    }
                }
            }
        }
        for l in &left_on {
            let output = OutputRef {
                switch_id: l.switch_id.clone(),
                output: l.output.clone(),
            };
            if self.memory.logged_left_on.insert(output) {
                warn!(switch_id = %l.switch_id, output = %l.output, reason = %l.reason, "an output stays on");
            } else {
                debug!(switch_id = %l.switch_id, output = %l.output, reason = %l.reason, "an output still stays on");
            }
        }
        left_on
    }

    /// What the cut rules read of the devices on `outputs`. A camera `rp`
    /// holds no session with is connected first; one that will not
    /// connect, or whose `get_camera_info` fails, is unreadable.
    async fn readings(&mut self, outputs: &[MapOutput], deadline: DateTime<Utc>) -> Readings {
        let mut readings = Readings::default();
        let cameras: Vec<MapDevice> = dedup(outputs.iter().flat_map(|o| o.of_kind("camera")));
        for camera in cameras {
            if !camera.connected {
                let connected = self
                    .call(
                        "connect_device",
                        json!({ "device_id": camera.device_id }),
                        deadline,
                    )
                    .await
                    .is_ok_and(|v| v.get("connected") == Some(&Value::Bool(true)));
                if !connected {
                    readings
                        .cameras
                        .insert(camera.device_id, CameraReading::Unreadable);
                    continue;
                }
            }
            let reading = self
                .call(
                    "get_camera_info",
                    json!({ "camera_id": camera.device_id }),
                    deadline,
                )
                .await
                .map_or(CameraReading::Unreadable, |info| {
                    CameraReading::Cooler(info.get("cooler_running").and_then(Value::as_bool))
                });
            readings.cameras.insert(camera.device_id, reading);
        }
        if outputs.iter().any(|o| o.of_kind("mount").next().is_some()) {
            let park = self.call("get_park_state", json!({}), deadline).await;
            readings.park = Some(park.map_or(ParkReading::Failed, |park| {
                park.get("at_park").and_then(Value::as_bool).map_or(
                    ParkReading::Failed,
                    |at_park| ParkReading::Read {
                        at_park,
                        pose_kept: park
                            .get("park_matches_power_up_pose")
                            .and_then(Value::as_bool),
                    },
                )
            }));
        }
        for cover in dedup(outputs.iter().flat_map(|o| o.of_kind("cover_calibrator"))) {
            let reading = self
                .call(
                    "get_cover_state",
                    json!({ "calibrator_id": cover.device_id }),
                    deadline,
                )
                .await
                .map_or(CoverReading::Failed, |state| {
                    state
                        .get("cover_state")
                        .and_then(Value::as_str)
                        .map_or(CoverReading::Failed, |s| CoverReading::State(s.to_owned()))
                });
            readings.covers.insert(cover.device_id, reading);
        }
        readings
    }

    /// `rp`'s power map, or none when it cannot be read.
    async fn power_map(&mut self, deadline: DateTime<Utc>) -> Vec<MapOutput> {
        match self.call("get_power_map", json!({}), deadline).await {
            Ok(answer) => parse_power_map(&answer).unwrap_or_else(|e| {
                warn!(error = %e, "rp's power map does not read; nothing is cut");
                Vec::new()
            }),
            Err(e) => {
                warn!(error = %e, "cannot read rp's power map; nothing is cut");
                Vec::new()
            }
        }
    }

    /// A step of dawn: a failure is logged at `warn!` and dawn goes on.
    async fn step(&mut self, tool: &str, args: Value, deadline: DateTime<Utc>) {
        if let Err(e) = self.call(tool, args, deadline).await {
            warn!(tool, error = %e, "a step of dawn failed; dawn goes on");
        }
    }

    /// Call `tool`, retrying while `rp` is unreachable, backing off from
    /// 1 s to 30 s, until `deadline`.
    ///
    /// # Errors
    ///
    /// The tool's error, or `rp` still unreachable at `deadline`.
    async fn call(
        &mut self,
        tool: &str,
        args: Value,
        deadline: DateTime<Utc>,
    ) -> Result<Value, String> {
        let args = match args {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match self.rp.call(tool, args.clone()).await {
                Ok(answer) => {
                    if self.rp_was_down {
                        self.rp_was_down = false;
                        self.rp_came_back = true;
                    }
                    return Ok(answer);
                }
                Err(ToolCallError::Unavailable(message)) => {
                    self.rp_was_down = true;
                    let now = self.clock.now();
                    if now >= deadline {
                        return Err(format!("rp unreachable: {message}"));
                    }
                    debug!(tool, error = %message, wait = ?backoff, "rp unreachable; retrying the step");
                    self.clock.sleep(backoff.min(until(now, deadline))).await;
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    /// Note an event-stream reconnect as `rp` back from a restart.
    fn note_rp_restart(&mut self) {
        let connects = self.connects.load(std::sync::atomic::Ordering::SeqCst);
        if self.seen_connects > 0 && connects > self.seen_connects {
            self.rp_came_back = true;
        }
        self.seen_connects = connects;
    }
}

fn leave(output: &OutputRef, reason: String) -> LeftOn {
    LeftOn {
        switch_id: output.switch_id.clone(),
        output: output.output.clone(),
        reason,
    }
}

/// The devices in `devices`, each once, in their first order.
fn dedup<'a>(devices: impl Iterator<Item = &'a MapDevice>) -> Vec<MapDevice> {
    let mut seen = BTreeSet::new();
    devices
        .filter(|d| seen.insert(d.device_id.clone()))
        .cloned()
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;

    use chrono::TimeZone;

    use super::super::test_rig::{at, date, Harness, TestClock};
    use super::*;
    use crate::night::test_rig::{FakeRp, FakeSlot};

    type TestSchedule<'a> = Schedule<'a, FakeRp, TestClock, FakeSlot>;

    /// Run the loop as `Schedule::run` does until `done` holds, at most
    /// `passes` passes.
    async fn run_until(
        h: &Harness,
        schedule: &mut TestSchedule<'_>,
        passes: usize,
        done: impl Fn(&Harness) -> bool,
    ) {
        for _ in 0..passes {
            let wait = schedule.tick().await;
            if done(h) {
                return;
            }
            h.clock.sleep(wait).await;
        }
        panic!(
            "not done after {passes} passes; steps: {:?}; state: {:?}",
            h.rp.steps(),
            h.status.snapshot()
        );
    }

    fn position(steps: &[String], tool: &str) -> usize {
        steps
            .iter()
            .position(|s| s == tool)
            .unwrap_or_else(|| panic!("{tool} was not called: {steps:?}"))
    }

    fn left_on(h: &Harness) -> Vec<(String, String)> {
        h.status
            .snapshot()
            .unwrap()
            .outputs_left_on
            .into_iter()
            .map(|l| (l.output, l.reason))
            .collect()
    }

    /// The day after night 23: its dawn at 14:31 has passed.
    fn morning() -> Harness {
        Harness::new(at(24, 15))
    }

    #[tokio::test]
    async fn dawn_ends_the_run_warms_parks_closes_waits_and_cuts_every_daily_output() {
        let h = morning();
        *h.slot.active.lock().unwrap() = Some("run-7".into());
        let mut schedule = h.schedule();

        assert_eq!(schedule.tick().await, Duration::ZERO);

        assert_eq!(*h.slot.stopped.lock().unwrap(), ["run-7"]);
        assert!(
            !h.slot.held.load(Ordering::SeqCst),
            "the slot is let go once dawn is done"
        );
        let steps = h.rp.steps();
        assert!(position(&steps, "start_warmup") < position(&steps, "park"));
        assert!(position(&steps, "park") < position(&steps, "close_cover"));
        assert!(position(&steps, "close_cover") < position(&steps, "power_down"));
        assert_eq!(
            h.rp.args_of("power_down"),
            [json!({ "outputs": [
                { "switch_id": "power-box", "output": "Camera Power" },
                { "switch_id": "power-box", "output": "Mount Power" },
                { "switch_id": "power-box", "output": "Panel Power" },
            ] })]
        );
        {
            let rig = h.rp.rig.lock().unwrap();
            assert_eq!(rig.output("Camera Power").on, Some(false));
            assert_eq!(rig.output("Host Power").on, Some(true), "not daily");
        }
        let state = h.status.snapshot().unwrap();
        assert_eq!(
            state.night,
            date(24),
            "nothing left on: the file is on the night ahead"
        );
        assert_eq!(state.phase, Phase::Day);
        assert_eq!(NightState::load(h.dir.path()).await, Some(state));
    }

    #[tokio::test]
    async fn dawn_waits_for_the_coolers_before_it_applies_the_rules() {
        let h = morning();
        let started = h.clock.now();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("power_down"), 1);
        let waited = until(started, h.clock.now());
        assert_eq!(
            waited,
            Duration::from_mins(1),
            "two 30 s polls until the cooler stopped"
        );
    }

    #[tokio::test]
    async fn a_cooler_still_running_after_warmup_timeout_holds_its_output_alone() {
        let h = morning();
        h.rp.rig.lock().unwrap().reads_to_cool = None;
        let started = h.clock.now();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(until(started, h.clock.now()), Duration::from_mins(45));
        assert_eq!(
            left_on(&h),
            [("Camera Power".into(), "main-cam's cooler is running".into())]
        );
        let state = h.status.snapshot().unwrap();
        assert_eq!((state.night, state.phase), (date(23), Phase::AfterDawn));
        let rig = h.rp.rig.lock().unwrap();
        assert_eq!(rig.output("Mount Power").on, Some(false));
        assert_eq!(rig.output("Camera Power").on, Some(true));
    }

    #[tokio::test]
    async fn a_mount_with_no_word_on_its_pose_keeps_its_output() {
        let h = morning();
        h.rp.rig.lock().unwrap().pose_kept = None;
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            left_on(&h),
            [(
                "Mount Power".into(),
                "the mount does not say a cut keeps its pose".into()
            )]
        );
    }

    #[tokio::test]
    async fn a_run_that_has_not_ended_in_ten_minutes_is_left_to_end() {
        let h = morning();
        *h.slot.active.lock().unwrap() = Some("run-7".into());
        h.slot.never_ends.store(true, Ordering::SeqCst);
        let started = h.clock.now();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(*h.slot.stopped.lock().unwrap(), ["run-7"]);
        assert!(until(started, h.clock.now()) >= Duration::from_mins(10));
        assert!(
            !h.slot.held.load(Ordering::SeqCst),
            "the run still holds the slot"
        );
        assert_eq!(h.rp.called("power_down"), 1, "dawn went on");
    }

    #[tokio::test]
    async fn an_unreadable_box_and_a_failed_cut_join_the_outputs_left_on() {
        let h = morning();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.outputs
                .iter_mut()
                .find(|o| o.output == "Panel Power")
                .unwrap()
                .on = None;
            rig.cuts_fail.insert("Camera Power".into());
        }
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            left_on(&h),
            [
                (
                    "Panel Power".into(),
                    "the power box `power-box` cannot be read".into()
                ),
                (
                    "Camera Power".into(),
                    "its cut failed: the box refused".into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn an_unreadable_camera_holds_its_output_until_warmup_timeout_has_passed() {
        let h = morning();
        h.rp.rig
            .lock()
            .unwrap()
            .camera_info_fails
            .insert("main-cam".into());
        let dawn_started = h.clock.now();
        let mut schedule = h.schedule();

        run_until(&h, &mut schedule, 40, |h| {
            h.status.snapshot().is_some_and(|s| s.night == date(24))
        })
        .await;

        let cut_at = until(dawn_started, h.clock.now());
        assert!(
            cut_at >= Duration::from_mins(45),
            "cut {cut_at:?} after dawn"
        );
        assert!(
            cut_at <= Duration::from_mins(50),
            "cut {cut_at:?} after dawn"
        );
        assert_eq!(
            h.rp.rig.lock().unwrap().output("Camera Power").on,
            Some(false)
        );
    }

    #[tokio::test]
    async fn the_after_dawn_checks_cut_an_output_once_its_cooler_has_stopped() {
        let h = morning();
        h.rp.rig.lock().unwrap().reads_to_cool = None;
        let mut schedule = h.schedule();
        schedule.tick().await;
        assert_eq!(left_on(&h).len(), 1);

        h.rp.rig
            .lock()
            .unwrap()
            .coolers
            .insert("main-cam".into(), Some(false));
        run_until(&h, &mut schedule, 5, |h| {
            h.status.snapshot().is_some_and(|s| s.night == date(24))
        })
        .await;

        assert_eq!(
            h.rp.rig.lock().unwrap().output("Camera Power").on,
            Some(false)
        );
        assert_eq!(h.status.snapshot().unwrap().phase, Phase::Day);
    }

    #[tokio::test]
    async fn the_mount_is_parked_again_three_times_then_left_on_until_dusk() {
        let h = morning();
        h.rp.rig.lock().unwrap().parks_fail = true;
        let mut schedule = h.schedule();

        run_until(&h, &mut schedule, 200, |h| {
            h.status.snapshot().is_some_and(|s| s.night == date(24))
        })
        .await;

        assert_eq!(h.rp.called("park"), 4, "dawn's park and three more");
        let dusk = h.status.snapshot().unwrap();
        assert!(
            h.clock.now() >= at(25, 0),
            "the file moved at dusk, {}",
            h.clock.now()
        );
        assert_eq!(dusk.phase, Phase::Day);
        assert_eq!(
            h.rp.rig.lock().unwrap().output("Mount Power").on,
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_cover_that_will_not_close_is_closed_again_three_times() {
        let h = morning();
        h.rp.rig.lock().unwrap().closes_fail = true;
        let mut schedule = h.schedule();

        run_until(&h, &mut schedule, 200, |h| {
            h.status.snapshot().is_some_and(|s| s.night == date(24))
        })
        .await;

        assert_eq!(h.rp.called("close_cover"), 4, "dawn's close and three more");
        assert_eq!(
            h.rp.rig.lock().unwrap().output("Panel Power").on,
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_failed_cut_connects_its_devices_again_before_the_next_try() {
        let h = morning();
        h.rp.rig
            .lock()
            .unwrap()
            .cuts_fail
            .insert("Mount Power".into());
        let mut schedule = h.schedule();
        schedule.tick().await;
        assert!(h.rp.rig.lock().unwrap().held_out.contains("mount"));

        h.rp.rig.lock().unwrap().cuts_fail.clear();
        run_until(&h, &mut schedule, 5, |h| {
            h.status.snapshot().is_some_and(|s| s.night == date(24))
        })
        .await;

        let connects = h.rp.args_of("connect_device");
        assert!(
            connects.contains(&json!({ "device_id": "mount" })),
            "{connects:?}"
        );
        assert_eq!(
            h.rp.rig.lock().unwrap().output("Mount Power").on,
            Some(false)
        );
    }

    #[tokio::test]
    async fn a_device_held_out_on_an_output_held_for_another_reason_stays_held_out() {
        let h = morning();
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.pose_kept = None;
            rig.held_out.insert("mount".into());
        }
        let mut schedule = h.schedule();
        schedule.tick().await;
        assert_eq!(left_on(&h).len(), 1, "{:?}", left_on(&h));

        for _ in 0..3 {
            let wait = schedule.tick().await;
            h.clock.sleep(wait).await;
        }

        assert!(
            !h.rp
                .args_of("connect_device")
                .contains(&json!({ "device_id": "mount" })),
            "{:?}",
            h.rp.args_of("connect_device")
        );
        assert!(h.rp.rig.lock().unwrap().held_out.contains("mount"));
    }

    #[tokio::test]
    async fn a_comeback_noticed_one_morning_warms_nothing_the_next() {
        let h = morning();
        h.rp.rig
            .lock()
            .unwrap()
            .unavailable_for
            .insert("start_warmup".into(), 1);
        let mut schedule = h.schedule();
        schedule.tick().await;
        assert_eq!(left_on(&h), [], "the first morning cuts everything");
        assert_eq!(
            h.rp.called("start_warmup"),
            2,
            "one failed call and its retry"
        );

        // The next morning, with the rig powered again overnight, a
        // cooler keeps running past warmup_timeout.
        {
            let mut rig = h.rp.rig.lock().unwrap();
            for output in &mut rig.outputs {
                output.on = Some(true);
            }
            rig.held_out.clear();
            rig.reads_to_cool = None;
            rig.warming = false;
            rig.coolers.insert("main-cam".into(), Some(true));
        }
        h.clock.sleep(until(h.clock.now(), at(25, 15))).await;
        schedule.tick().await;
        assert_eq!(left_on(&h).len(), 1);
        assert_eq!(h.rp.called("start_warmup"), 3, "the second dawn's own");

        let wait = schedule.tick().await;
        h.clock.sleep(wait).await;
        schedule.tick().await;
        assert_eq!(
            h.rp.called("start_warmup"),
            3,
            "no rp restart since that dawn"
        );
    }

    #[tokio::test]
    async fn rp_back_from_a_restart_has_the_cameras_warmed_once_more() {
        let h = morning();
        h.rp.rig.lock().unwrap().reads_to_cool = None;
        h.connects.store(1, Ordering::SeqCst);
        let mut schedule = h.schedule();
        schedule.tick().await;
        assert_eq!(h.rp.called("start_warmup"), 1);

        // The first check sees the stream as it is; a reconnect follows.
        let wait = schedule.tick().await;
        h.clock.sleep(wait).await;
        schedule.tick().await;
        h.connects.store(2, Ordering::SeqCst);
        let wait = schedule.tick().await;
        h.clock.sleep(wait).await;
        schedule.tick().await;

        assert_eq!(h.rp.called("start_warmup"), 2, "one more after the restart");
        let wait = schedule.tick().await;
        h.clock.sleep(wait).await;
        schedule.tick().await;
        assert_eq!(
            h.rp.called("start_warmup"),
            2,
            "and no more without another"
        );
    }

    #[tokio::test]
    async fn a_step_whose_call_fails_at_the_request_level_is_retried_backing_off() {
        let h = morning();
        h.rp.rig
            .lock()
            .unwrap()
            .unavailable_for
            .insert("start_warmup".into(), 3);
        let started = h.clock.now();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.rp.called("start_warmup"), 4);
        let steps = h.rp.steps();
        assert!(position(&steps, "start_warmup") < position(&steps, "park"));
        assert!(
            until(started, h.clock.now()) >= Duration::from_secs(1 + 2 + 4),
            "three back-offs before the warm-up answered"
        );
        assert_eq!(h.rp.called("power_down"), 1);
    }

    #[tokio::test]
    async fn a_step_rp_never_answers_is_given_up_at_dusk_and_dawn_goes_on() {
        let h = morning();
        h.rp.rig
            .lock()
            .unwrap()
            .unavailable_for
            .insert("park".into(), u32::MAX);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert!(
            h.clock.now() >= Utc.with_ymd_and_hms(2026, 11, 25, 0, 58, 0).unwrap(),
            "park was retried until the next dusk, {}",
            h.clock.now()
        );
        assert_eq!(h.rp.called("close_cover"), 1, "dawn went on after the park");
    }
}

//! A scripted `rp`, run slot and clock for the night schedule's tests.
//!
//! [`FakeRp`] answers `get_twilight` from a table of nights and the
//! power tools from a small [`Rig`]: a power box whose outputs carry a
//! camera, the mount and a cover, a cooler that stops a few reads after
//! `start_warmup`, a park and a cover close that work unless told not to.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeDelta, TimeZone, Utc};
use serde_json::{json, Map, Value};

use super::config::NightConfig;
use super::dawn::RunSlot;
use super::schedule::{Schedule, Seams};
use super::state::NightState;
use super::NightStatus;
use crate::engine::{Clock, ToolCallError, ToolClient};

pub fn date(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, day).unwrap()
}

pub fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
}

/// Civil darkness from 01:28 to 14:31 UTC after the night's date, as at
/// 47.6° N, 122° W in late November.
pub fn dark(day: u32) -> Value {
    json!({
        "kind": "civil",
        "begin_utc": format!("2026-11-{:02}T01:28:00+00:00", day + 1),
        "end_utc": format!("2026-11-{:02}T14:31:00+00:00", day + 1),
    })
}

/// night.json on `night` in phase `day`, with `dark`'s times and a
/// `-30m` dusk offset: the dawn before it done.
pub fn done_up_to(night: u32) -> NightState {
    NightState::ahead(
        date(night),
        Utc.with_ymd_and_hms(2026, 11, night + 1, 0, 58, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 11, night + 1, 14, 31, 0)
            .unwrap(),
    )
}

/// A clock whose `sleep` moves `now` on at once.
pub struct TestClock {
    now: Mutex<DateTime<Utc>>,
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }

    async fn sleep(&self, duration: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += TimeDelta::from_std(duration).unwrap();
    }

    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// One output of the rig's power box.
pub struct RigOutput {
    pub output: &'static str,
    pub daily: bool,
    pub on: Option<bool>,
    pub devices: Vec<(&'static str, &'static str)>,
}

/// The rig behind [`FakeRp`].
pub struct Rig {
    pub outputs: Vec<RigOutput>,
    pub held_out: BTreeSet<String>,
    /// Each camera's `cooler_running`.
    pub coolers: BTreeMap<String, Option<bool>>,
    /// Cooler reads after `start_warmup` before a running cooler stops;
    /// `None` keeps it running.
    pub reads_to_cool: Option<u32>,
    pub warming: bool,
    pub camera_info_fails: BTreeSet<String>,
    pub connect_fails: BTreeSet<String>,
    pub at_park: bool,
    pub pose_kept: Option<bool>,
    pub parks_fail: bool,
    pub covers: BTreeMap<String, String>,
    pub closes_fail: bool,
    /// Outputs whose cut `power_down` reports with an error.
    pub cuts_fail: BTreeSet<String>,
    /// Answer this many calls with a request-level failure.
    pub unavailable: u32,
    /// Answer this many calls to one tool with a request-level failure.
    pub unavailable_for: BTreeMap<String, u32>,
}

impl Default for Rig {
    fn default() -> Self {
        Self {
            outputs: vec![
                RigOutput {
                    output: "Camera Power",
                    daily: true,
                    on: Some(true),
                    devices: vec![("main-cam", "camera")],
                },
                RigOutput {
                    output: "Mount Power",
                    daily: true,
                    on: Some(true),
                    devices: vec![("mount", "mount")],
                },
                RigOutput {
                    output: "Panel Power",
                    daily: true,
                    on: Some(true),
                    devices: vec![("flat-panel", "cover_calibrator")],
                },
                RigOutput {
                    output: "Host Power",
                    daily: false,
                    on: Some(true),
                    devices: vec![],
                },
            ],
            held_out: BTreeSet::new(),
            coolers: [("main-cam".to_owned(), Some(true))].into(),
            reads_to_cool: Some(2),
            warming: false,
            camera_info_fails: BTreeSet::new(),
            connect_fails: BTreeSet::new(),
            at_park: false,
            pose_kept: Some(true),
            parks_fail: false,
            covers: [("flat-panel".to_owned(), "Open".to_owned())].into(),
            closes_fail: false,
            cuts_fail: BTreeSet::new(),
            unavailable: 0,
            unavailable_for: BTreeMap::new(),
        }
    }
}

impl Rig {
    pub fn output(&self, name: &str) -> &RigOutput {
        self.outputs.iter().find(|o| o.output == name).unwrap()
    }

    fn output_mut(&mut self, name: &str) -> &mut RigOutput {
        self.outputs.iter_mut().find(|o| o.output == name).unwrap()
    }

    fn connected(&self, device: &str) -> bool {
        let powered = self
            .outputs
            .iter()
            .find(|o| o.devices.iter().any(|(id, _)| *id == device))
            .is_none_or(|o| o.on == Some(true));
        powered && !self.held_out.contains(device)
    }
}

/// `rp` as the schedule sees it.
pub struct FakeRp {
    pub nights: BTreeMap<NaiveDate, Value>,
    pub rig: Mutex<Rig>,
    pub calls: Mutex<Vec<(String, Value)>>,
}

impl FakeRp {
    /// The tools called, in order, `get_twilight` left out.
    pub fn steps(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(tool, _)| tool.clone())
            .filter(|tool| tool != "get_twilight")
            .collect()
    }

    pub fn called(&self, tool: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .count()
    }

    /// The arguments of every call to `tool`.
    pub fn args_of(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn answer(&self, tool: &str, args: &Value) -> Result<Value, ToolCallError> {
        let mut rig = self.rig.lock().unwrap();
        if rig.unavailable > 0 {
            rig.unavailable -= 1;
            return Err(ToolCallError::Unavailable("connection refused".into()));
        }
        if let Some(left) = rig.unavailable_for.get_mut(tool).filter(|left| **left > 0) {
            *left -= 1;
            return Err(ToolCallError::Unavailable("connection refused".into()));
        }
        let failed = |message: &str| Err(ToolCallError::Failed(message.to_owned()));
        let id = |key: &str| args[key].as_str().unwrap_or_default().to_owned();
        match tool {
            "get_twilight" => {
                drop(rig);
                let night: NaiveDate = args["date"].as_str().unwrap().parse().unwrap();
                self.nights
                    .get(&night)
                    .cloned()
                    .ok_or_else(|| ToolCallError::Failed("site required".into()))
            }
            "get_power_map" => {
                let outputs: Vec<Value> = rig
                    .outputs
                    .iter()
                    .map(|o| {
                        let devices: Vec<Value> = o
                            .devices
                            .iter()
                            .map(|(id, kind)| {
                                json!({
                                    "device_id": id,
                                    "kind": kind,
                                    "connected": rig.connected(id),
                                    "held_out": rig.held_out.contains(*id),
                                })
                            })
                            .collect();
                        json!({ "switch_id": "power-box", "output": o.output, "daily": o.daily,
                                "protected": null, "on": o.on, "devices": devices })
                    })
                    .collect();
                Ok(json!({ "outputs": outputs }))
            }
            "start_warmup" => {
                rig.warming = true;
                Ok(json!({ "cameras": ["main-cam"] }))
            }
            "park" if rig.parks_fail => failed("park timed out"),
            "park" if !rig.connected("mount") => failed("mount not connected"),
            "park" => {
                rig.at_park = true;
                Ok(json!({}))
            }
            "close_cover" if rig.closes_fail => failed("cover did not answer"),
            "close_cover" => {
                rig.covers.insert(id("calibrator_id"), "Closed".into());
                Ok(json!({ "status": "closed" }))
            }
            "get_camera_info" => {
                let camera = id("camera_id");
                if rig.camera_info_fails.contains(&camera) || !rig.connected(&camera) {
                    return failed("camera not connected");
                }
                let running = rig.coolers.get(&camera).copied().flatten();
                if running == Some(true) && rig.warming {
                    match rig.reads_to_cool {
                        Some(0) => {
                            rig.coolers.insert(camera.clone(), Some(false));
                        }
                        Some(n) => rig.reads_to_cool = Some(n - 1),
                        None => {}
                    }
                }
                Ok(json!({ "cooler_running": rig.coolers.get(&camera).copied().flatten() }))
            }
            "get_park_state" if !rig.connected("mount") => failed("mount not connected"),
            "get_park_state" => Ok(json!({ "at_park": rig.at_park, "can_park": true,
                                           "can_unpark": true,
                                           "park_matches_power_up_pose": rig.pose_kept })),
            "get_cover_state" => {
                let calibrator = id("calibrator_id");
                Ok(json!({ "calibrator_id": calibrator,
                           "cover_state": rig.covers.get(&calibrator).cloned().unwrap_or_default() }))
            }
            "connect_device" => {
                let device = id("device_id");
                if rig.connect_fails.contains(&device) {
                    return failed("device did not connect");
                }
                rig.held_out.remove(&device);
                Ok(json!({ "device_id": device, "connected": rig.connected(&device) }))
            }
            "power_down" => {
                let mut outputs = Vec::new();
                for target in args["outputs"].as_array().unwrap() {
                    let name = target["output"].as_str().unwrap().to_owned();
                    let devices: Vec<String> = rig
                        .output(&name)
                        .devices
                        .iter()
                        .map(|(id, _)| (*id).to_owned())
                        .collect();
                    rig.held_out.extend(devices);
                    if rig.cuts_fail.contains(&name) {
                        outputs.push(json!({ "switch_id": "power-box", "output": name,
                                             "on": true, "error": "the box refused" }));
                    } else {
                        rig.output_mut(&name).on = Some(false);
                        outputs
                            .push(json!({ "switch_id": "power-box", "output": name, "on": false }));
                    }
                }
                Ok(json!({ "outputs": outputs, "devices": [] }))
            }
            other => Err(ToolCallError::Failed(format!(
                "the fake rp has no tool {other}"
            ))),
        }
    }
}

impl ToolClient for FakeRp {
    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ToolCallError> {
        let args = Value::Object(args);
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        self.answer(tool, &args)
    }
}

/// The run slot: a run that ends when stopped, unless told not to.
#[derive(Default)]
pub struct FakeSlot {
    pub active: Mutex<Option<String>>,
    pub stopped: Mutex<Vec<String>>,
    pub held: AtomicBool,
    /// The stopped run keeps going.
    pub never_ends: AtomicBool,
}

impl RunSlot for FakeSlot {
    fn active_run(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }

    fn stop_run(&self, run_id: &str) {
        self.stopped.lock().unwrap().push(run_id.to_owned());
        if !self.never_ends.load(Ordering::SeqCst) {
            *self.active.lock().unwrap() = None;
        }
    }

    fn hold(&self) -> bool {
        if self.active.lock().unwrap().is_some() {
            return false;
        }
        self.held.store(true, Ordering::SeqCst);
        true
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }
}

/// Everything a test schedule runs against.
pub struct Harness {
    pub dir: tempfile::TempDir,
    pub rp: FakeRp,
    pub clock: TestClock,
    pub slot: FakeSlot,
    pub night: NightConfig,
    pub status: NightStatus,
    pub connects: Arc<AtomicU64>,
}

impl Harness {
    /// The default rig at `now`, with civil darkness every night from the
    /// 18th to the 30th and a `-30m` dusk offset.
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            rp: FakeRp {
                nights: (18..=30).map(|d| (date(d), dark(d))).collect(),
                rig: Mutex::new(Rig::default()),
                calls: Mutex::new(Vec::new()),
            },
            clock: TestClock {
                now: Mutex::new(now),
            },
            slot: FakeSlot::default(),
            night: serde_json::from_value(json!({ "dusk_offset": "-30m" })).unwrap(),
            status: NightStatus::default(),
            connects: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn schedule(&self) -> Schedule<'_, FakeRp, TestClock, FakeSlot> {
        Schedule::new(Seams {
            rp: &self.rp,
            clock: &self.clock,
            slot: &self.slot,
            night: &self.night,
            state_dir: self.dir.path(),
            status: &self.status,
            warmup_done: Arc::new(tokio::sync::Notify::new()),
            connects: self.connects.clone(),
        })
    }
}

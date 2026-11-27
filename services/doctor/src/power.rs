//! Powered-off devices (docs/services/doctor.md §Powered-off devices).
//!
//! Before doctor reports a device missing, it reads the power-box output
//! that feeds the device: an output that reads off makes the check `ok`,
//! one that reads on or cannot be read leaves its verdict standing. Three
//! steps: [`PowerMap::needed`] finds, from `rp`'s roster and power map,
//! the outputs the checks marked [`Absent`] need; [`read_outputs`] reads
//! each once from its power box's driver; [`judge`] rewrites each check
//! by what was read. Only the middle step touches the network, and only
//! when a check needs an output.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::checks::Context;
use crate::report::{Absent, Check, Status};
use crate::scan::ClientAuthView;

/// Judge every check whose verdict is a missing device by the power-box
/// output that feeds it. Sends nothing when no check needs an output.
pub fn apply(ctx: &Context, checks: &mut [Check]) {
    let Some(map) = PowerMap::from_rp(ctx) else {
        return;
    };
    let needed = map.needed(checks);
    if needed.is_empty() {
        return;
    }
    let readings = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(read_outputs(ctx, &map, &needed)),
        Err(e) => needed
            .into_iter()
            .map(|key| {
                (
                    key,
                    Reading::Unread(format!("could not start the probe runtime: {e}")),
                )
            })
            .collect(),
    };
    judge(&map, &readings, checks);
}

/// A roster device: its `rp` id, the scanned service its `alpaca_url`
/// joins, and its ASCOM device type and number.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RosterDevice {
    id: String,
    service: &'static str,
    device_type: Option<&'static str>,
    device_number: Option<u32>,
}

/// A `switches[]` entry: how doctor reaches the power box's driver.
#[derive(Debug, Clone)]
struct PowerBox {
    id: String,
    alpaca_url: String,
    device_number: u32,
    auth: Option<ClientAuthView>,
}

/// One output of the power map: its power box, by index into
/// [`PowerMap::boxes`], and its name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OutputKey {
    power_box: usize,
    name: String,
}

/// What reading an output found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reading {
    On,
    Off,
    /// Why the output could not be read.
    Unread(String),
}

/// `rp`'s roster and power map, as doctor reads them from `rp.json`.
#[derive(Debug, Default)]
struct PowerMap {
    devices: Vec<RosterDevice>,
    boxes: Vec<PowerBox>,
    /// Each output and the roster ids its `devices` lists.
    outputs: Vec<(OutputKey, Vec<String>)>,
}

/// The ASCOM device type of a roster kind. A kind this table does not know
/// gives none: its devices count for a check about their service, and
/// match no check about one device.
fn device_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "cameras" => "Camera",
        "focusers" => "Focuser",
        "filter_wheels" => "FilterWheel",
        "cover_calibrators" => "CoverCalibrator",
        "safety_monitors" => "SafetyMonitor",
        "switches" => "Switch",
        "rotators" => "Rotator",
        "observing_conditions" => "ObservingConditions",
        "domes" => "Dome",
        "mount" => "Telescope",
        _ => return None,
    })
}

/// How a device stands against the outputs that feed it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeviceVerdict<'a> {
    /// An output that lists it reads off.
    Off(&'a OutputKey),
    /// Every output that lists it reads on.
    On(Vec<&'a OutputKey>),
    /// No output that lists it reads off, and this one could not be read.
    Unread(&'a OutputKey, &'a str),
    /// The power map puts it on no output.
    Unmapped,
}

impl PowerMap {
    /// The map in `rp`'s config, when `rp.json` holds a roster that
    /// doctor can read. `None` without one: there is nothing to look up.
    fn from_rp(ctx: &Context) -> Option<Self> {
        let rp = ctx.scans.iter().find(|s| s.entry.name == "rp")?;
        let equipment = crate::scan::view::<crate::scan::RpView>(rp)?
            .ok()?
            .equipment?;
        Some(Self::from_equipment(ctx, &equipment))
    }

    fn from_equipment(ctx: &Context, equipment: &Value) -> Self {
        let mut map = Self::default();
        let Value::Object(kinds) = equipment else {
            return map;
        };
        for (kind, value) in kinds {
            let entries: Vec<(String, &Value)> = match value {
                Value::Array(entries) => entries
                    .iter()
                    .filter_map(|e| Some((e.get("id")?.as_str()?.to_string(), e)))
                    .collect(),
                Value::Object(_) if kind == "mount" => vec![("mount".to_string(), value)],
                _ => Vec::new(),
            };
            for (id, entry) in entries {
                let Some(url) = entry.get("alpaca_url").and_then(Value::as_str) else {
                    continue;
                };
                let device_number = entry
                    .get("device_number")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok());
                if let Some(service) =
                    crate::checks::parse_target_url(url).and_then(|(_, host, port)| {
                        crate::checks::resolve_join_target(ctx, &host, port)
                    })
                {
                    map.devices.push(RosterDevice {
                        id: id.clone(),
                        service: service.entry.name,
                        device_type: device_type(kind),
                        device_number,
                    });
                }
                if kind == "switches" {
                    map.add_power_box(id, url, device_number.unwrap_or(0), entry);
                }
            }
        }
        map
    }

    fn add_power_box(&mut self, id: String, url: &str, device_number: u32, entry: &Value) {
        let power_box = self.boxes.len();
        self.boxes.push(PowerBox {
            id,
            alpaca_url: url.to_string(),
            device_number,
            auth: entry
                .get("auth")
                .and_then(|a| serde_json::from_value(a.clone()).ok()),
        });
        let outputs = entry
            .get("outputs")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for output in outputs {
            let Some(name) = output.get("name").and_then(Value::as_str) else {
                continue;
            };
            let devices =
                output
                    .get("devices")
                    .and_then(Value::as_array)
                    .map_or_else(Vec::new, |ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    });
            self.outputs.push((
                OutputKey {
                    power_box,
                    name: name.to_string(),
                },
                devices,
            ));
        }
    }

    /// The roster devices a check's verdict is about: every device on its
    /// service, or the one of the marked type at the marked number. Empty
    /// for a check that is not a missing device's.
    fn devices_for(&self, check: &Check) -> Vec<&RosterDevice> {
        let (Some(absent), Some(service)) = (&check.absent, &check.service) else {
            return Vec::new();
        };
        if check.status == Status::Ok {
            return Vec::new();
        }
        self.devices
            .iter()
            .filter(|d| d.service == service)
            .filter(|d| matches_device(absent, d))
            .collect()
    }

    /// The outputs whose `devices` list `id`.
    fn outputs_of(&self, id: &str) -> Vec<&OutputKey> {
        self.outputs
            .iter()
            .filter(|(_, devices)| devices.iter().any(|d| d == id))
            .map(|(key, _)| key)
            .collect()
    }

    /// Every output some check needs read.
    fn needed(&self, checks: &[Check]) -> BTreeSet<OutputKey> {
        checks
            .iter()
            .flat_map(|check| self.devices_for(check))
            .flat_map(|device| self.outputs_of(&device.id))
            .cloned()
            .collect()
    }

    fn verdict<'a>(
        &'a self,
        device: &RosterDevice,
        readings: &'a BTreeMap<OutputKey, Reading>,
    ) -> DeviceVerdict<'a> {
        let outputs = self.outputs_of(&device.id);
        if outputs.is_empty() {
            return DeviceVerdict::Unmapped;
        }
        if let Some(off) = outputs
            .iter()
            .find(|key| readings.get(key) == Some(&Reading::Off))
        {
            return DeviceVerdict::Off(off);
        }
        for key in &outputs {
            match readings.get(key) {
                Some(Reading::Unread(reason)) => return DeviceVerdict::Unread(key, reason),
                None => return DeviceVerdict::Unread(key, "it was not read"),
                Some(_) => {}
            }
        }
        DeviceVerdict::On(outputs)
    }

    /// How an output reads in a detail: `ppba's "Quad 12V Output"`.
    fn name_output(&self, key: &OutputKey) -> String {
        let power_box = self.boxes.get(key.power_box).map_or("?", |b| b.id.as_str());
        format!("{power_box}'s \"{}\"", key.name)
    }
}

/// Whether a device is the one an [`Absent`] names: any device on the
/// service when it names none, else the device of its type, ignoring ASCII
/// case, at its number.
fn matches_device(absent: &Absent, device: &RosterDevice) -> bool {
    let type_matches = absent.device_type.as_deref().is_none_or(|wanted| {
        device
            .device_type
            .is_some_and(|t| t.eq_ignore_ascii_case(wanted))
    });
    let number_matches = absent
        .device_number
        .is_none_or(|wanted| device.device_number == Some(wanted));
    type_matches && number_matches
}

/// Rewrite each check whose devices the power map puts on an output: `ok`
/// when every one of them reads powered off, else its verdict stands and
/// its detail says how each device reads.
fn judge(map: &PowerMap, readings: &BTreeMap<OutputKey, Reading>, checks: &mut [Check]) {
    for check in checks.iter_mut() {
        let devices = map.devices_for(check);
        let verdicts: Vec<(&RosterDevice, DeviceVerdict)> = devices
            .into_iter()
            .map(|device| (device, map.verdict(device, readings)))
            .collect();
        if verdicts
            .iter()
            .all(|(_, verdict)| *verdict == DeviceVerdict::Unmapped)
        {
            continue;
        }
        if verdicts
            .iter()
            .all(|(_, verdict)| matches!(verdict, DeviceVerdict::Off(_)))
        {
            powered_off(map, &verdicts, check);
        } else {
            let notes: Vec<String> = verdicts
                .iter()
                .map(|(device, verdict)| note(map, device, verdict))
                .collect();
            let _ = write!(check.detail, "; {}", notes.join("; "));
        }
    }
}

/// Turn `check` into the `ok` of a device that is powered off, naming each
/// output that reads off and the devices it feeds.
fn powered_off(map: &PowerMap, verdicts: &[(&RosterDevice, DeviceVerdict)], check: &mut Check) {
    let mut by_output: BTreeMap<&OutputKey, Vec<&str>> = BTreeMap::new();
    for (device, verdict) in verdicts {
        if let DeviceVerdict::Off(key) = verdict {
            by_output.entry(key).or_default().push(&device.id);
        }
    }
    let outputs: Vec<String> = by_output
        .iter()
        .map(|(key, ids)| {
            format!(
                "{} reads off, and feeds {}",
                map.name_output(key),
                and_list(ids)
            )
        })
        .collect();
    let mut detail = format!(
        "powered off, not missing: {} (the check found: {})",
        outputs.join("; "),
        check.detail
    );
    if check.name == "service.devices" {
        if let Some(service) = &check.service {
            let _ = write!(
                detail,
                "; dusk's power_up, which restarts {service} once the output is on, places a \
                 camera that was only off"
            );
        }
    }
    check.status = Status::Ok;
    check.detail = detail;
    check.suggestion = None;
}

/// How one device reads, for a verdict that stands.
fn note(map: &PowerMap, device: &RosterDevice, verdict: &DeviceVerdict) -> String {
    let id = &device.id;
    match verdict {
        DeviceVerdict::Off(key) => {
            format!("{id} is powered off: {} reads off", map.name_output(key))
        }
        DeviceVerdict::On(keys) => {
            let names: Vec<String> = keys.iter().map(|key| map.name_output(key)).collect();
            format!(
                "{id}'s output {} {}, so {id} should be there",
                and_list(&names),
                if names.len() == 1 {
                    "reads on"
                } else {
                    "read on"
                }
            )
        }
        DeviceVerdict::Unread(key, reason) => format!(
            "doctor could not read {id}'s output {}, so it cannot tell a powered-off \
             device from a missing one: {reason}",
            map.name_output(key)
        ),
        DeviceVerdict::Unmapped => format!("{id} is on no output of rp's power map"),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_string(),
        [rest @ .., last] => {
            let head: Vec<&str> = rest.iter().map(AsRef::as_ref).collect();
            format!("{} and {}", head.join(", "), last.as_ref())
        }
    }
}

/// An Alpaca response: its `Value`, or the error the driver answered with.
#[derive(Debug, Deserialize)]
struct AlpacaResponse<T> {
    #[serde(rename = "Value")]
    value: Option<T>,
    #[serde(rename = "ErrorNumber", default)]
    error_number: i64,
    #[serde(rename = "ErrorMessage", default)]
    error_message: String,
}

/// One power box's driver, reached as `rp` reaches it.
struct Driver {
    client: reqwest::Client,
    base: String,
    basic: Option<(String, String)>,
}

impl Driver {
    /// Connect as `rp` does: the entry's own `auth`, else the observatory
    /// credential; for `https`, doctor's CA on a self-signed install and
    /// the platform store on an ACME one, as the fake-mount probe does.
    fn new(ctx: &Context, power_box: &PowerBox, acme_domain: Option<&str>) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(&power_box.alpaca_url).map_err(|e| {
            format!(
                "its alpaca_url {} does not parse: {e}",
                power_box.alpaca_url
            )
        })?;
        let ca_path = (parsed.scheme() == "https" && acme_domain.is_none()).then(|| {
            rusty_photon_tls::config::ca_cert_path(&crate::provision::pki_dir(&ctx.config_dir))
        });
        let client = rusty_photon_tls::client::build_reqwest_client(ca_path.as_deref())
            .map_err(|e| format!("doctor could not build a client for it: {e}"))?;
        let basic = match &power_box.auth {
            Some(ClientAuthView {
                username: Some(username),
                password: Some(password),
            }) => Some((username.clone(), password.clone())),
            Some(_) => None,
            None => crate::provision::read_credential(&ctx.config_dir)
                .map(|password| (crate::provision::CREDENTIAL_USERNAME.to_string(), password)),
        };
        Ok(Self {
            client,
            base: format!(
                "{}/api/v1/switch/{}",
                power_box.alpaca_url.trim_end_matches('/'),
                power_box.device_number
            ),
            basic,
        })
    }

    /// `GET <member>[?Id=<id>]`, and its `Value`.
    async fn get<T: DeserializeOwned>(&self, member: &str, id: Option<usize>) -> Result<T, String> {
        let url = format!("{}/{member}", self.base);
        let mut request = self
            .client
            .get(&url)
            .timeout(crate::aggregate::HTTP_TIMEOUT);
        if let Some(id) = id {
            request = request.query(&[("Id", id)]);
        }
        if let Some((username, password)) = &self.basic {
            request = request.basic_auth(username, Some(password));
        }
        let response = request.send().await.map_err(|e| {
            format!(
                "{url} does not answer: {}",
                crate::aggregate::describe_send_error(e, crate::aggregate::HTTP_TIMEOUT)
            )
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url} answered HTTP {status}"));
        }
        let answer: AlpacaResponse<T> = response
            .json()
            .await
            .map_err(|e| format!("{url} answered with a payload that does not parse: {e}"))?;
        if answer.error_number != 0 {
            return Err(format!(
                "{member} answered ASCOM error {:#x}: {}",
                answer.error_number, answer.error_message
            ));
        }
        answer
            .value
            .ok_or_else(|| format!("{url} answered with no Value"))
    }
}

/// Read every output in `needed`, each power box's switch names once.
async fn read_outputs(
    ctx: &Context,
    map: &PowerMap,
    needed: &BTreeSet<OutputKey>,
) -> BTreeMap<OutputKey, Reading> {
    let acme_domain = crate::aggregate::acme_probe_domain(&ctx.config_dir);
    let mut readings = BTreeMap::new();
    for (index, power_box) in map.boxes.iter().enumerate() {
        let names: Vec<&str> = needed
            .iter()
            .filter(|key| key.power_box == index)
            .map(|key| key.name.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        debug!(power_box = power_box.id, outputs = ?names, "reading power-box outputs");
        let read = match Driver::new(ctx, power_box, acme_domain.as_deref()) {
            Ok(driver) => read_box(&driver, &names).await,
            Err(reason) => names
                .iter()
                .map(|_| Reading::Unread(reason.clone()))
                .collect(),
        };
        for (name, reading) in names.into_iter().zip(read) {
            readings.insert(
                OutputKey {
                    power_box: index,
                    name: name.to_string(),
                },
                reading,
            );
        }
    }
    readings
}

/// Read `names` from one driver, in order: `maxswitch`, the switch names
/// until each output is found, then `getswitch` for each.
async fn read_box(driver: &Driver, names: &[&str]) -> Vec<Reading> {
    let max = match driver.get::<usize>("maxswitch", None).await {
        Ok(max) => max,
        Err(reason) => {
            return names
                .iter()
                .map(|_| Reading::Unread(reason.clone()))
                .collect()
        }
    };
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    for id in 0..max {
        if names.iter().all(|name| ids.contains_key(name)) {
            break;
        }
        match driver.get::<String>("getswitchname", Some(id)).await {
            Ok(switch_name) => {
                if let Some(name) = names.iter().find(|name| **name == switch_name) {
                    ids.entry(name).or_insert(id);
                }
            }
            Err(reason) => {
                return names
                    .iter()
                    .map(|_| Reading::Unread(reason.clone()))
                    .collect()
            }
        }
    }
    let mut read = Vec::with_capacity(names.len());
    for name in names {
        read.push(match ids.get(name) {
            None => Reading::Unread(format!(
                "its driver reports no switch named \"{name}\" among its {max}"
            )),
            Some(id) => match driver.get::<bool>("getswitch", Some(*id)).await {
                Ok(true) => Reading::On,
                Ok(false) => Reading::Off,
                Err(reason) => Reading::Unread(reason),
            },
        });
    }
    read
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::facts::PlatformFacts;
    use std::path::Path;

    const QUAD: &str = "Quad 12V Output";
    const HUB: &str = "USB Hub";

    /// A config-only context over `dir`, where qhy-camera and zwo-camera
    /// participate at their default ports and rp's roster puts main-cam
    /// and main-fw on qhy-camera, guide-cam and side-cam on zwo-camera,
    /// and remote-cam on another host.
    fn rig(dir: &Path) -> Context {
        std::fs::write(dir.join("qhy-camera.json"), "{}").unwrap();
        std::fs::write(dir.join("zwo-camera.json"), "{}").unwrap();
        let rp = serde_json::json!({ "equipment": {
            "cameras": [
                { "id": "main-cam", "alpaca_url": "http://localhost:11121", "device_number": 0 },
                { "id": "guide-cam", "alpaca_url": "http://localhost:11122", "device_number": 0 },
                { "id": "side-cam", "alpaca_url": "http://127.0.0.1:11122", "device_number": 1 },
                { "id": "remote-cam", "alpaca_url": "http://rig2.example:11121", "device_number": 0 }
            ],
            "filter_wheels": [
                { "id": "main-fw", "alpaca_url": "http://localhost:11121", "device_number": 0 }
            ],
            "switches": [ {
                "id": "ppba",
                "alpaca_url": "http://localhost:11112",
                "device_number": 0,
                "outputs": [
                    { "name": QUAD, "devices": ["main-cam", "main-fw", "remote-cam"], "daily": true },
                    { "name": HUB, "devices": ["guide-cam", "main-cam"], "daily": true },
                    { "name": "Adjustable Output", "protected": "the host runs off it" }
                ]
            } ]
        } });
        std::fs::write(dir.join("rp.json"), rp.to_string()).unwrap();
        let facts: PlatformFacts =
            serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap();
        Context::gather(dir.to_path_buf(), facts)
    }

    fn key(name: &str) -> OutputKey {
        OutputKey {
            power_box: 0,
            name: name.to_string(),
        }
    }

    fn readings(quad: Reading, hub: Reading) -> BTreeMap<OutputKey, Reading> {
        BTreeMap::from([(key(QUAD), quad), (key(HUB), hub)])
    }

    fn usb_missing(service: &str) -> Check {
        Check::fail(
            "hardware.usb-device",
            Some(service.to_string()),
            "no USB device matching 1618:* is on the bus",
            Some("check the cable".to_string()),
        )
        .with_absent(Absent::service())
    }

    fn judged(ctx: &Context, readings: &BTreeMap<OutputKey, Reading>, check: Check) -> Check {
        let map = PowerMap::from_rp(ctx).unwrap();
        let mut checks = [check];
        judge(&map, readings, &mut checks);
        let [check] = checks;
        check
    }

    #[test]
    fn the_roster_joins_its_devices_to_the_services_doctor_scans() {
        let dir = tempfile::tempdir().unwrap();
        let map = PowerMap::from_rp(&rig(dir.path())).unwrap();
        let devices: Vec<(&str, &str, Option<&str>, Option<u32>)> = map
            .devices
            .iter()
            .map(|d| (d.id.as_str(), d.service, d.device_type, d.device_number))
            .collect();
        assert_eq!(
            devices,
            [
                ("main-cam", "qhy-camera", Some("Camera"), Some(0)),
                ("guide-cam", "zwo-camera", Some("Camera"), Some(0)),
                ("side-cam", "zwo-camera", Some("Camera"), Some(1)),
                ("main-fw", "qhy-camera", Some("FilterWheel"), Some(0)),
            ],
            "remote-cam is on another host, and ppba-driver has no config here"
        );
        assert_eq!(map.boxes.len(), 1, "a power box need not join a service");
        assert_eq!(map.outputs.len(), 3);
    }

    #[test]
    fn a_service_whose_devices_all_read_powered_off_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(Reading::Off, Reading::On),
            usb_missing("qhy-camera"),
        );
        assert_eq!(check.status, Status::Ok);
        assert_eq!(
            check.detail,
            "powered off, not missing: ppba's \"Quad 12V Output\" reads off, and feeds main-cam \
             and main-fw (the check found: no USB device matching 1618:* is on the bus)"
        );
        assert_eq!(check.suggestion, None);
        assert_eq!(check.absent, Some(Absent::service()), "still absent");
    }

    #[test]
    fn one_output_that_reads_off_powers_off_a_device_on_two() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(Reading::On, Reading::Off),
            Check::warn(
                "usb-devices.resolve",
                Some("qhy-camera".to_string()),
                "empty",
                None,
            )
            .with_absent(Absent::device("Camera", 0)),
        );
        assert_eq!(check.status, Status::Ok, "{}", check.detail);
        assert!(
            check.detail.starts_with(
                "powered off, not missing: ppba's \"USB Hub\" reads off, and feeds main-cam ("
            ),
            "{}",
            check.detail
        );
    }

    #[test]
    fn outputs_that_read_on_leave_the_verdict_and_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(Reading::On, Reading::On),
            usb_missing("qhy-camera"),
        );
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.detail,
            "no USB device matching 1618:* is on the bus; main-cam's output ppba's \"Quad 12V \
             Output\" and ppba's \"USB Hub\" read on, so main-cam should be there; main-fw's \
             output ppba's \"Quad 12V Output\" reads on, so main-fw should be there"
        );
        assert_eq!(check.suggestion.as_deref(), Some("check the cable"));
    }

    #[test]
    fn an_output_that_cannot_be_read_leaves_the_verdict_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(
                Reading::Unread("connection refused".to_string()),
                Reading::On,
            ),
            usb_missing("qhy-camera"),
        );
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.ends_with(
                "doctor could not read main-fw's output ppba's \"Quad 12V Output\", so it cannot \
                 tell a powered-off device from a missing one: connection refused"
            ),
            "{}",
            check.detail
        );
    }

    #[test]
    fn a_service_with_a_device_on_no_output_keeps_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(Reading::On, Reading::Off),
            usb_missing("zwo-camera"),
        );
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.ends_with(
                "; guide-cam is powered off: ppba's \"USB Hub\" reads off; side-cam is on no \
                 output of rp's power map"
            ),
            "{}",
            check.detail
        );
    }

    #[test]
    fn a_check_about_one_device_looks_up_that_device_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = rig(dir.path());
        let guide = Check::warn(
            "usb-devices.resolve",
            Some("zwo-camera".to_string()),
            "empty",
            None,
        )
        .with_absent(Absent::device("camera", 0));
        let check = judged(&ctx, &readings(Reading::On, Reading::Off), guide);
        assert_eq!(
            check.status,
            Status::Ok,
            "the type matches ignoring ASCII case: {}",
            check.detail
        );
        let side = Check::warn(
            "usb-devices.resolve",
            Some("zwo-camera".to_string()),
            "empty",
            None,
        )
        .with_absent(Absent::device("Camera", 1));
        let check = judged(&ctx, &readings(Reading::On, Reading::Off), side);
        assert_eq!(check.status, Status::Warn);
        assert_eq!(check.detail, "empty", "no output to read, nothing added");
    }

    #[test]
    fn a_placeholder_powered_off_says_dusks_restart_places_it() {
        let dir = tempfile::tempdir().unwrap();
        let check = judged(
            &rig(dir.path()),
            &readings(Reading::Off, Reading::On),
            Check::warn(
                "service.devices",
                Some("qhy-camera".to_string()),
                "placeholder",
                None,
            )
            .with_absent(Absent::device("FilterWheel", 0)),
        );
        assert_eq!(check.status, Status::Ok);
        assert!(
            check.detail.ends_with(
                "; dusk's power_up, which restarts qhy-camera once the output is on, places a \
                 camera that was only off"
            ),
            "{}",
            check.detail
        );
    }

    #[test]
    fn only_a_missing_device_that_is_not_ok_needs_an_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = rig(dir.path());
        let map = PowerMap::from_rp(&ctx).unwrap();
        let mut ok = usb_missing("qhy-camera");
        ok.status = Status::Ok;
        let unmarked = Check::fail(
            "hardware.usb-device",
            Some("qhy-camera".to_string()),
            "a fault",
            None,
        );
        let unscoped =
            Check::fail("hardware.usb-device", None, "x", None).with_absent(Absent::service());
        let elsewhere = usb_missing("dsd-fp2");
        assert!(map.needed(&[ok, unmarked, unscoped, elsewhere]).is_empty());
        let both = [usb_missing("qhy-camera"), usb_missing("zwo-camera")];
        assert_eq!(
            map.needed(&both).into_iter().collect::<Vec<_>>(),
            [key(QUAD), key(HUB)],
            "each output once, and never the one nothing missing sits on"
        );
    }

    #[test]
    fn no_roster_means_no_map() {
        let dir = tempfile::tempdir().unwrap();
        let facts: PlatformFacts =
            serde_json::from_value(serde_json::json!({ "platform": "linux" })).unwrap();
        let ctx = Context::gather(dir.path().to_path_buf(), facts);
        assert!(PowerMap::from_rp(&ctx).is_none());
        let mut checks = [usb_missing("qhy-camera")];
        apply(&ctx, &mut checks);
        assert_eq!(checks[0].status, Status::Fail);
    }

    #[test]
    fn lists_read_as_prose() {
        assert_eq!(and_list::<&str>(&[]), "");
        assert_eq!(and_list(&["a"]), "a");
        assert_eq!(and_list(&["a", "b"]), "a and b");
        assert_eq!(and_list(&["a", "b", "c"]), "a, b and c");
    }

    /// A stub power box at `/api/v1/switch/0`: three switches, `QUAD` off
    /// and `HUB` on, and `getswitch` on switch 2 answering ASCOM's
    /// not-connected error.
    async fn stub_power_box() -> u16 {
        use axum::extract::Query;
        use std::collections::HashMap;

        fn answer(value: &serde_json::Value) -> axum::Json<serde_json::Value> {
            axum::Json(serde_json::json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
        }
        fn id(query: &HashMap<String, String>) -> usize {
            query["Id"].parse().unwrap()
        }
        let router = axum::Router::new()
            .route(
                "/api/v1/switch/0/maxswitch",
                axum::routing::get(|| async { answer(&serde_json::json!(3)) }),
            )
            .route(
                "/api/v1/switch/0/getswitchname",
                axum::routing::get(|Query(q): Query<HashMap<String, String>>| async move {
                    answer(&serde_json::json!([QUAD, HUB, "Dew Heater A"][id(&q)]))
                }),
            )
            .route(
                "/api/v1/switch/0/getswitch",
                axum::routing::get(|Query(q): Query<HashMap<String, String>>| async move {
                    match id(&q) {
                        2 => axum::Json(serde_json::json!({
                            "Value": false, "ErrorNumber": 1031, "ErrorMessage": "Not connected" })),
                        n => answer(&serde_json::json!(n == 1)),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    fn driver(port: u16, number: u32) -> Driver {
        Driver {
            client: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}/api/v1/switch/{number}"),
            basic: None,
        }
    }

    #[tokio::test]
    async fn a_driver_reads_each_output_by_its_name() {
        let port = stub_power_box().await;
        let read = read_box(&driver(port, 0), &[HUB, QUAD, "Dew Heater A", "Nope"]).await;
        assert_eq!(read[0], Reading::On);
        assert_eq!(read[1], Reading::Off);
        assert_eq!(
            read[2],
            Reading::Unread("getswitch answered ASCOM error 0x407: Not connected".to_string())
        );
        assert_eq!(
            read[3],
            Reading::Unread("its driver reports no switch named \"Nope\" among its 3".to_string())
        );
    }

    #[tokio::test]
    async fn a_driver_that_does_not_answer_leaves_every_output_unread() {
        let port = stub_power_box().await;
        let read = read_box(&driver(port, 7), &[QUAD, HUB]).await;
        let [Reading::Unread(first), Reading::Unread(second)] = read.as_slice() else {
            panic!("{read:?}");
        };
        assert!(first.contains("answered HTTP 404"), "{first}");
        assert_eq!(first, second, "one failure of the box reads for both");
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let read = read_box(&driver(dead_port, 0), &[QUAD]).await;
        let [Reading::Unread(reason)] = read.as_slice() else {
            panic!("{read:?}");
        };
        assert!(reason.contains("does not answer"), "{reason}");
    }

    #[test]
    fn a_driver_presents_the_entrys_own_credential_else_the_observatorys() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = rig(dir.path());
        let mut power_box = PowerBox {
            id: "ppba".to_string(),
            alpaca_url: "http://localhost:11112/".to_string(),
            device_number: 2,
            auth: Some(ClientAuthView {
                username: Some("rig".to_string()),
                password: Some("secret".to_string()),
            }),
        };
        let driver = Driver::new(&ctx, &power_box, None).unwrap();
        assert_eq!(driver.base, "http://localhost:11112/api/v1/switch/2");
        assert_eq!(
            driver.basic,
            Some(("rig".to_string(), "secret".to_string()))
        );
        power_box.auth = None;
        assert_eq!(
            Driver::new(&ctx, &power_box, None).unwrap().basic,
            None,
            "no pki/credential here to fall back to"
        );
        power_box.alpaca_url = "not a url".to_string();
        let Err(reason) = Driver::new(&ctx, &power_box, None) else {
            panic!("a URL that does not parse builds no driver");
        };
        assert!(
            reason.starts_with("its alpaca_url not a url does not parse"),
            "{reason}"
        );
    }
}

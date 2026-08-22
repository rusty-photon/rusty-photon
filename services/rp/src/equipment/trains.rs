//! The derived optical-train coupling model (rp.md § Optical Trains).
//!
//! [`TrainModel::try_from_equipment`] is both the cross-array graph
//! validation for `equipment.optical_trains` (shared by `load_config`
//! and `PUT /api/config` via `validate_config`) and the builder of the
//! runtime model the derivation queries run against — one code path,
//! so validation and derivation cannot drift apart. Per-field
//! invariants (the purpose enum, focal-length positivity) are already
//! enforced in the config field types at deserialize.

use std::collections::{HashMap, HashSet};

use rusty_photon_config::actions::FieldError;

use crate::config::equipment::EquipmentConfig;
use crate::config::optical_train::{OpticalTrainConfig, TrainPurpose};

/// What kind of roster device a train entry resolved to. Only these
/// four kinds sit in a light path; everything else in the roster
/// (switches, safety monitors, ...) is rejected at validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrainDeviceKind {
    Camera,
    Focuser,
    Rotator,
    FilterWheel,
}

impl TrainDeviceKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Focuser => "focuser",
            Self::Rotator => "rotator",
            Self::FilterWheel => "filter wheel",
        }
    }
}

/// One resolved train entry: the roster id plus its resolved kind.
#[derive(Debug, Clone)]
pub struct TrainDevice {
    pub id: String,
    pub kind: TrainDeviceKind,
}

/// One validated train: ordered devices, objective side first, the
/// last always a camera.
#[derive(Debug, Clone)]
pub struct Train {
    pub id: String,
    pub purpose: TrainPurpose,
    pub focal_length_mm: Option<f64>,
    /// The train's default framing angle, degrees east of north —
    /// layer two of the effective position angle `get_next_target`
    /// resolves (rp.md § Target Store → Position angle).
    pub default_position_angle_degrees: Option<f64>,
    pub devices: Vec<TrainDevice>,
    /// The train's V-curve sweep parameters
    /// (`optical_trains[].auto_focus`), carried through for the
    /// train-addressed `auto_focus` fallback and `refocus_train`.
    pub auto_focus: Option<crate::config::TrainAutoFocusConfig>,
}

impl Train {
    /// The camera this train terminates in. Always `Some` for a
    /// validated train (never empty, last entry a camera); `Option`
    /// keeps the accessor total.
    #[must_use]
    pub fn camera_id(&self) -> Option<&str> {
        self.devices.last().map(|d| d.id.as_str())
    }

    /// The focuser that focuses this train's camera: the last focuser
    /// in the list. `None` for a train without focusers.
    #[must_use]
    pub fn terminal_focuser(&self) -> Option<&str> {
        self.devices
            .iter()
            .rev()
            .find(|d| d.kind == TrainDeviceKind::Focuser)
            .map(|d| d.id.as_str())
    }
}

/// One step of a dependency-ordered auto-focus sequence (rp.md
/// § Optical Trains, derivation rules).
///
/// Runs `focuser_id`'s AF in the context of `train_id` — capturing (or,
/// for the guiding train, reading PHD2 metrics) through that train's
/// camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfStep {
    pub focuser_id: String,
    pub train_id: String,
}

/// The validated, queryable train model. `Default` is the no-trains
/// model — every query returns "nothing", which is exactly the
/// pre-train behavior (trains are enrichment, not a gate).
#[derive(Debug, Clone, Default)]
pub struct TrainModel {
    trains: Vec<Train>,
}

/// Dotted config path of train `i`'s `suffix`
/// (`equipment.optical_trains.0.devices.2`).
fn train_path(i: usize, suffix: &str) -> String {
    format!("equipment.optical_trains.{i}{suffix}")
}

/// The four light-path kinds by roster id, plus the "what it actually
/// is" map for every roster id outside those kinds — so the error can
/// say "is a switch" instead of "not in the roster".
fn roster_kinds(
    equipment: &EquipmentConfig,
) -> (HashMap<&str, TrainDeviceKind>, HashMap<&str, &'static str>) {
    let mut kinds: HashMap<&str, TrainDeviceKind> = HashMap::new();
    for cam in &equipment.cameras {
        kinds.insert(&cam.id, TrainDeviceKind::Camera);
    }
    for f in &equipment.focusers {
        kinds.insert(&f.id, TrainDeviceKind::Focuser);
    }
    for r in &equipment.rotators {
        kinds.insert(&r.id, TrainDeviceKind::Rotator);
    }
    for fw in &equipment.filter_wheels {
        kinds.insert(&fw.id, TrainDeviceKind::FilterWheel);
    }
    let mut other_kinds: HashMap<&str, &'static str> = HashMap::new();
    for cc in &equipment.cover_calibrators {
        other_kinds.insert(&cc.id, "cover calibrator");
    }
    for sm in &equipment.safety_monitors {
        other_kinds.insert(&sm.id, "safety monitor");
    }
    for sw in &equipment.switches {
        other_kinds.insert(&sw.id, "switch");
    }
    for oc in &equipment.observing_conditions {
        other_kinds.insert(&oc.id, "observing-conditions device");
    }
    for d in &equipment.domes {
        other_kinds.insert(&d.id, "dome");
    }
    (kinds, other_kinds)
}

/// Purpose rules: at most one train may guide, and a guiding train
/// requires `equipment.mount.guiding`.
fn validate_purpose(
    train: &OpticalTrainConfig,
    i: usize,
    guiding_configured: bool,
    seen_guiding_train: &mut bool,
    errors: &mut Vec<FieldError>,
) {
    if train.purpose != TrainPurpose::Guiding {
        return;
    }
    if *seen_guiding_train {
        errors.push(FieldError {
            path: train_path(i, ".purpose"),
            msg: format!(
                "at most one train may have purpose \"guiding\" (train '{}')",
                train.id
            ),
        });
    }
    *seen_guiding_train = true;
    if !guiding_configured {
        errors.push(FieldError {
            path: train_path(i, ".purpose"),
            msg: format!(
                "a guiding train requires equipment.mount.guiding (train '{}')",
                train.id
            ),
        });
    }
}

/// Per-purpose `auto_focus` block fields (rp.md § Optical Trains): the
/// capture fields are required on imaging trains and rejected on the
/// guiding train (its sweep is metric-based); `frames_per_step` the
/// other way around.
fn validate_auto_focus(train: &OpticalTrainConfig, i: usize, errors: &mut Vec<FieldError>) {
    let Some(block) = &train.auto_focus else {
        return;
    };
    let block_path = |field: &str| train_path(i, &format!(".auto_focus.{field}"));
    match train.purpose {
        TrainPurpose::Imaging => {
            for (field, missing) in [
                ("duration", block.duration.is_none()),
                ("min_area", block.min_area.is_none()),
                ("max_area", block.max_area.is_none()),
            ] {
                if missing {
                    errors.push(FieldError {
                        path: block_path(field),
                        msg: format!(
                            "auto_focus.{field} is required for an imaging \
                             train's capture sweep (train '{}')",
                            train.id
                        ),
                    });
                }
            }
            if block.frames_per_step.is_some() {
                errors.push(FieldError {
                    path: block_path("frames_per_step"),
                    msg: format!(
                        "auto_focus.frames_per_step only applies to the guiding \
                         train's metric sweep (train '{}')",
                        train.id
                    ),
                });
            }
        }
        TrainPurpose::Guiding => {
            for (field, present) in [
                ("duration", block.duration.is_some()),
                ("min_area", block.min_area.is_some()),
                ("max_area", block.max_area.is_some()),
                ("threshold_sigma", block.threshold_sigma.is_some()),
            ] {
                if present {
                    errors.push(FieldError {
                        path: block_path(field),
                        msg: format!(
                            "auto_focus.{field} does not apply to the guiding \
                             train's metric sweep (train '{}')",
                            train.id
                        ),
                    });
                }
            }
        }
    }
}

/// Resolve `train.devices` against the roster and enforce the light-path
/// shape: no repeats, only the four kinds, camera terminal-only, and no
/// camera terminating two trains. Returns the resolved list plus
/// whether the train stayed structurally clean.
fn validate_devices<'a>(
    train: &'a OpticalTrainConfig,
    i: usize,
    kinds: &HashMap<&'a str, TrainDeviceKind>,
    other_kinds: &HashMap<&'a str, &'static str>,
    seen_terminal_cameras: &mut HashMap<&'a str, &'a str>,
    errors: &mut Vec<FieldError>,
) -> (Vec<TrainDevice>, bool) {
    let mut devices = Vec::new();
    let mut seen_in_train: HashSet<&str> = HashSet::new();
    let last = train.devices.len().saturating_sub(1);
    let mut train_ok = true;
    for (j, id) in train.devices.iter().enumerate() {
        let entry_path = train_path(i, &format!(".devices.{j}"));
        if !seen_in_train.insert(id) {
            errors.push(FieldError {
                path: entry_path,
                msg: format!("device '{id}' repeats within train '{}'", train.id),
            });
            train_ok = false;
            continue;
        }
        let Some(kind) = kinds.get(id.as_str()).copied() else {
            let msg = other_kinds.get(id.as_str()).map_or_else(
                || {
                    format!(
                        "'{id}' is not in the equipment roster (train '{}')",
                        train.id
                    )
                },
                |other| {
                    format!(
                        "'{id}' is a {other}; trains may only contain cameras, \
                         focusers, rotators, and filter wheels (train '{}')",
                        train.id
                    )
                },
            );
            errors.push(FieldError {
                path: entry_path,
                msg,
            });
            train_ok = false;
            continue;
        };
        match (kind, j == last) {
            (TrainDeviceKind::Camera, false) => {
                errors.push(FieldError {
                    path: entry_path,
                    msg: format!(
                        "camera '{id}' may only terminate a train (train '{}')",
                        train.id
                    ),
                });
                train_ok = false;
            }
            (TrainDeviceKind::Camera, true) => {
                if let Some(other) = seen_terminal_cameras.insert(id, &train.id) {
                    errors.push(FieldError {
                        path: entry_path,
                        msg: format!(
                            "camera '{id}' already terminates train '{other}' \
                             (train '{}')",
                            train.id
                        ),
                    });
                    train_ok = false;
                }
            }
            (_, true) => {
                errors.push(FieldError {
                    path: entry_path,
                    msg: format!(
                        "the last device must be a camera; got {} '{id}' (train '{}')",
                        kind.name(),
                        train.id
                    ),
                });
                train_ok = false;
            }
            (_, false) => {}
        }
        devices.push(TrainDevice {
            id: id.clone(),
            kind,
        });
    }
    (devices, train_ok)
}

impl TrainModel {
    /// Validate `equipment.optical_trains` against the roster and build
    /// the model.
    ///
    /// # Errors
    ///
    /// On any violation returns the full [`FieldError`] list with dotted
    /// paths (`equipment.optical_trains.0.devices.2`), in train order,
    /// for `validate_config` to surface: duplicate train ids, the
    /// purpose rules (one guiding train, and only with a guider
    /// configured), purpose-inconsistent `auto_focus` fields, an empty
    /// device list, a device that repeats or is not a train-eligible
    /// roster entry, a camera anywhere but last or a non-camera last, a
    /// camera terminating two trains, and shared devices in contradictory
    /// order across trains.
    pub fn try_from_equipment(equipment: &EquipmentConfig) -> Result<Self, Vec<FieldError>> {
        let mut errors = Vec::new();
        let (kinds, other_kinds) = roster_kinds(equipment);
        let guiding_configured = equipment
            .mount
            .as_ref()
            .is_some_and(|m| m.guiding.is_some());

        let mut trains = Vec::new();
        let mut seen_train_ids: HashSet<&str> = HashSet::new();
        // Camera id → id of the train that reserved it. Reservation is a
        // property of the *submitted* config, not the accepted model: a
        // train invalidated by an unrelated error still reserves its
        // terminal camera, so a double-termination conflict surfaces in
        // the same validation pass as that error instead of appearing
        // only after the operator fixes it and reloads.
        let mut seen_terminal_cameras: HashMap<&str, &str> = HashMap::new();
        let mut seen_guiding_train = false;

        for (i, train) in equipment.optical_trains.iter().enumerate() {
            if !seen_train_ids.insert(&train.id) {
                errors.push(FieldError {
                    path: train_path(i, ".id"),
                    msg: format!("duplicate train id '{}'", train.id),
                });
            }

            validate_purpose(
                train,
                i,
                guiding_configured,
                &mut seen_guiding_train,
                &mut errors,
            );
            validate_auto_focus(train, i, &mut errors);

            if train.devices.is_empty() {
                errors.push(FieldError {
                    path: train_path(i, ".devices"),
                    msg: format!(
                        "must not be empty; a train terminates in a camera (train '{}')",
                        train.id
                    ),
                });
                continue;
            }

            let (devices, train_ok) = validate_devices(
                train,
                i,
                &kinds,
                &other_kinds,
                &mut seen_terminal_cameras,
                &mut errors,
            );
            if train_ok {
                trains.push(Train {
                    id: train.id.clone(),
                    purpose: train.purpose,
                    focal_length_mm: train
                        .focal_length_mm
                        .map(super::super::config::optical_train::FocalLengthMm::value),
                    default_position_angle_degrees: train
                        .default_position_angle_degrees
                        .map(super::super::config::optical_train::PositionAngleDegrees::value),
                    devices,
                    auto_focus: train.auto_focus.clone(),
                });
            }
        }

        // Shared devices must appear in a consistent relative order
        // across trains: consecutive-pair edges over every clean
        // train's list must form an acyclic relation (adjacency chains
        // preserve reachability, so any pairwise contradiction closes
        // a cycle). Only structurally clean trains contribute —
        // roster/terminal errors above already invalidate the rest.
        if let Some(cycle) = order_cycle(&trains) {
            errors.push(FieldError {
                path: "equipment.optical_trains".to_string(),
                msg: format!(
                    "devices [{}] appear in contradictory order across trains",
                    cycle.join(", ")
                ),
            });
        }

        if errors.is_empty() {
            Ok(Self { trains })
        } else {
            Err(errors)
        }
    }

    /// Every validated train, in config order.
    #[must_use]
    pub fn trains(&self) -> &[Train] {
        &self.trains
    }

    #[must_use]
    pub fn train(&self, train_id: &str) -> Option<&Train> {
        self.trains.iter().find(|t| t.id == train_id)
    }

    /// The train terminating in `camera_id`, if any. At most one —
    /// validation rejects a camera terminating two trains.
    #[must_use]
    pub fn train_for_camera(&self, camera_id: &str) -> Option<&Train> {
        self.trains
            .iter()
            .find(|t| t.camera_id() == Some(camera_id))
    }

    /// The effective focal length of `camera_id`'s light path, feeding
    /// the exposure document's `optics` block. `None` when the camera
    /// terminates no train or its train omits `focal_length_mm`.
    #[must_use]
    pub fn focal_length_for_camera(&self, camera_id: &str) -> Option<f64> {
        self.train_for_camera(camera_id)?.focal_length_mm
    }

    /// The focuser that focuses `camera_id`: the last focuser in its
    /// train's list.
    #[must_use]
    pub fn focuser_for_camera(&self, camera_id: &str) -> Option<&str> {
        self.train_for_camera(camera_id)?.terminal_focuser()
    }

    /// Whether captures through this camera contend with mount
    /// motion: true iff the camera terminates a train with
    /// `purpose: "imaging"` (rp.md § Mount Motion Gate). Un-trained
    /// and guiding-train cameras bypass the gate.
    #[must_use]
    pub fn camera_in_imaging_train(&self, camera_id: &str) -> bool {
        self.train_for_camera(camera_id)
            .is_some_and(|t| t.purpose == TrainPurpose::Imaging)
    }

    /// The train with `purpose: "guiding"`, if any. At most one —
    /// validation rejects a second.
    #[must_use]
    pub fn guiding_train(&self) -> Option<&Train> {
        self.trains
            .iter()
            .find(|t| t.purpose == TrainPurpose::Guiding)
    }

    /// Every train containing `device_id` — the invalidation queries:
    /// moving focuser F invalidates focus of these trains, rotating
    /// rotator R rotates them, a filter change on wheel W touches them.
    #[must_use]
    pub fn trains_with_device(&self, device_id: &str) -> Vec<&Train> {
        self.trains
            .iter()
            .filter(|t| t.devices.iter().any(|d| d.id == device_id))
            .collect()
    }

    /// The dependency-ordered auto-focus sequence for a refocus
    /// trigger on `train_id`: shared focusers of the train
    /// upstream-first — each run in the train where it is terminal —
    /// then the train's own terminal focuser. `None` for an unknown
    /// train id; empty for a train without focusers.
    #[must_use]
    pub fn af_sequence(&self, train_id: &str) -> Option<Vec<AfStep>> {
        let train = self.train(train_id)?;
        let terminal = train.terminal_focuser();
        let mut steps = Vec::new();
        for device in &train.devices {
            if device.kind != TrainDeviceKind::Focuser || Some(device.id.as_str()) == terminal {
                continue;
            }
            if self.trains_with_device(&device.id).len() < 2 {
                // Not shared: no other train can run its AF, and this
                // train's own AF belongs to the terminal focuser.
                continue;
            }
            // Run the shared focuser in the train where it is terminal
            // (first in config order); a shared focuser terminal
            // nowhere falls back to this train.
            let run_train = self
                .trains
                .iter()
                .find(|t| t.terminal_focuser() == Some(device.id.as_str()))
                .map_or(train_id, |t| t.id.as_str());
            steps.push(AfStep {
                focuser_id: device.id.clone(),
                train_id: run_train.to_string(),
            });
        }
        if let Some(terminal) = terminal {
            steps.push(AfStep {
                focuser_id: terminal.to_string(),
                train_id: train_id.to_string(),
            });
        }
        Some(steps)
    }
}

/// Kahn's algorithm over the consecutive-pair order relation of the
/// clean trains. Returns the devices left on a cycle (in first-seen
/// order), or `None` when the merged relation is acyclic.
fn order_cycle(trains: &[Train]) -> Option<Vec<String>> {
    let mut nodes: Vec<&str> = Vec::new();
    let mut edges: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for train in trains {
        for device in &train.devices {
            if !in_degree.contains_key(device.id.as_str()) {
                nodes.push(&device.id);
                in_degree.insert(&device.id, 0);
            }
        }
        for pair in train.devices.windows(2) {
            let [from_dev, to_dev] = pair else { continue };
            let (from, to) = (from_dev.id.as_str(), to_dev.id.as_str());
            if edges.entry(from).or_default().insert(to) {
                let d = in_degree.entry(to).or_default();
                *d = d.saturating_add(1);
            }
        }
    }

    let mut queue: Vec<&str> = nodes
        .iter()
        .copied()
        .filter(|n| in_degree.get(n).is_some_and(|&d| d == 0))
        .collect();
    let mut removed = 0usize;
    while let Some(node) = queue.pop() {
        removed = removed.saturating_add(1);
        if let Some(next) = edges.get(node) {
            for &to in next {
                if let Some(d) = in_degree.get_mut(to) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(to);
                    }
                }
            }
        }
    }

    if removed == nodes.len() {
        None
    } else {
        Some(
            nodes
                .into_iter()
                .filter(|n| in_degree.get(n).is_some_and(|&d| d > 0))
                .map(str::to_string)
                .collect(),
        )
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Equipment JSON → `EquipmentConfig`, panicking on parse errors so
    /// tests fail loudly on typos.
    fn equipment(json: serde_json::Value) -> EquipmentConfig {
        serde_json::from_value(json).unwrap()
    }

    /// The reference rig from rp.md § Optical Trains: shared drawtube
    /// focuser (EAF), rotator behind the OAG pick-off (main train
    /// only), guide helical on the guide train.
    fn reference_rig() -> EquipmentConfig {
        equipment(serde_json::json!({
            "cameras": [
                {"id": "main-cam", "alpaca_url": "http://x"},
                {"id": "guide-cam", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "eaf", "alpaca_url": "http://x"},
                {"id": "scops-focuser", "alpaca_url": "http://x"}
            ],
            "rotators": [{"id": "falcon", "alpaca_url": "http://x"}],
            "filter_wheels": [{"id": "main-fw", "alpaca_url": "http://x"}],
            "mount": {
                "alpaca_url": "http://x",
                "guiding": {"url": "http://x"}
            },
            "optical_trains": [
                {"id": "main", "purpose": "imaging", "focal_length_mm": 360.0,
                 "devices": ["eaf", "main-fw", "falcon", "main-cam"]},
                {"id": "guide", "purpose": "guiding", "focal_length_mm": 360.0,
                 "devices": ["eaf", "scops-focuser", "guide-cam"]}
            ]
        }))
    }

    fn paths(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn an_imaging_block_missing_the_capture_fields_reports_every_gap() {
        let mut config = reference_rig();
        // Only the geometry: duration, min_area, and max_area are all
        // missing — every arm reports at once.
        config.optical_trains[0].auto_focus =
            serde_json::from_value(serde_json::json!({ "step_size": 10, "half_width": 50 }))
                .unwrap();
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.0.auto_focus.duration",
                "equipment.optical_trains.0.auto_focus.min_area",
                "equipment.optical_trains.0.auto_focus.max_area",
            ]
        );
        assert!(errors[0].msg.contains("required for an imaging train"));
    }

    #[test]
    fn an_imaging_block_rejects_frames_per_step() {
        let mut config = reference_rig();
        config.optical_trains[0].auto_focus = serde_json::from_value(serde_json::json!({
            "duration": "100ms", "step_size": 10, "half_width": 50,
            "min_area": 4, "max_area": 500, "frames_per_step": 3
        }))
        .unwrap();
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec!["equipment.optical_trains.0.auto_focus.frames_per_step"]
        );
        assert!(errors[0].msg.contains("only applies to the guiding train"));
    }

    #[test]
    fn a_guiding_block_rejects_every_capture_field() {
        let mut config = reference_rig();
        config.optical_trains[1].auto_focus = serde_json::from_value(serde_json::json!({
            "duration": "100ms", "step_size": 10, "half_width": 50,
            "min_area": 4, "max_area": 500, "threshold_sigma": 4.0
        }))
        .unwrap();
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.1.auto_focus.duration",
                "equipment.optical_trains.1.auto_focus.min_area",
                "equipment.optical_trains.1.auto_focus.max_area",
                "equipment.optical_trains.1.auto_focus.threshold_sigma",
            ]
        );
        assert!(errors[0]
            .msg
            .contains("does not apply to the guiding train"));
    }

    #[test]
    fn a_purpose_appropriate_block_passes_on_both_train_kinds() {
        let mut config = reference_rig();
        config.optical_trains[0].auto_focus = serde_json::from_value(serde_json::json!({
            "duration": "100ms", "step_size": 10, "half_width": 50,
            "min_area": 4, "max_area": 500
        }))
        .unwrap();
        config.optical_trains[1].auto_focus = serde_json::from_value(serde_json::json!({
            "step_size": 10, "half_width": 50, "frames_per_step": 2
        }))
        .unwrap();
        TrainModel::try_from_equipment(&config).unwrap();
    }

    #[test]
    fn auto_focus_block_is_carried_onto_the_validated_train() {
        let mut config = reference_rig();
        config.optical_trains[0].auto_focus = serde_json::from_value(serde_json::json!({
            "duration": "100ms", "step_size": 10, "half_width": 50,
            "min_area": 5, "max_area": 2000
        }))
        .unwrap();
        let model = TrainModel::try_from_equipment(&config).unwrap();
        let block = model.train("main").unwrap().auto_focus.as_ref().unwrap();
        assert_eq!(block.step_size.value(), 10);
        assert_eq!(block.half_width.value(), 50);
        assert!(model.train("guide").unwrap().auto_focus.is_none());
    }

    #[test]
    fn reference_rig_builds_and_answers_the_derivation_table() {
        let model = TrainModel::try_from_equipment(&reference_rig()).unwrap();

        // Which focuser focuses camera C? The last focuser in C's list.
        assert_eq!(model.focuser_for_camera("main-cam"), Some("eaf"));
        assert_eq!(model.focuser_for_camera("guide-cam"), Some("scops-focuser"));

        // Pixel-scale input.
        assert_eq!(model.focal_length_for_camera("main-cam"), Some(360.0));
        assert_eq!(model.focal_length_for_camera("unknown-cam"), None);

        // What does moving focuser F invalidate? Every train containing F.
        let invalidated: Vec<&str> = model
            .trains_with_device("eaf")
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(invalidated, vec!["main", "guide"]);

        // Rotation: the Falcon sits behind the pick-off, main only.
        let rotated: Vec<&str> = model
            .trains_with_device("falcon")
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(rotated, vec!["main"]);

        assert_eq!(model.guiding_train().unwrap().id, "guide");

        // Motion-gate scope: only the imaging terminal contends with
        // mount motion; the guide camera and un-trained cameras don't.
        assert!(model.camera_in_imaging_train("main-cam"));
        assert!(!model.camera_in_imaging_train("guide-cam"));
        assert!(!model.camera_in_imaging_train("unknown-cam"));
    }

    /// Requirement 1 of the plan: main AF first (shared EAF runs in the
    /// train where it is terminal), guide AF after.
    #[test]
    fn af_sequence_orders_shared_focusers_before_train_local_ones() {
        let model = TrainModel::try_from_equipment(&reference_rig()).unwrap();

        assert_eq!(
            model.af_sequence("guide").unwrap(),
            vec![
                AfStep {
                    focuser_id: "eaf".to_string(),
                    train_id: "main".to_string()
                },
                AfStep {
                    focuser_id: "scops-focuser".to_string(),
                    train_id: "guide".to_string()
                },
            ]
        );
        // The main train's own trigger only re-runs its terminal focuser.
        assert_eq!(
            model.af_sequence("main").unwrap(),
            vec![AfStep {
                focuser_id: "eaf".to_string(),
                train_id: "main".to_string()
            }]
        );
        assert!(model.af_sequence("nope").is_none());
    }

    /// The OAG-behind-rotator variant differs by one id in one list and
    /// flips every rotation derivation (plan, Decision 1).
    #[test]
    fn oag_behind_rotator_couples_rotation_to_the_guide_train() {
        let mut config = reference_rig();
        config.optical_trains[1]
            .devices
            .insert(1, "falcon".to_string());

        let model = TrainModel::try_from_equipment(&config).unwrap();
        let rotated: Vec<&str> = model
            .trains_with_device("falcon")
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(rotated, vec!["main", "guide"]);
    }

    /// Separate guide scope: trains sharing nothing degrade to
    /// independent trains (reference rig 3 in rp.md).
    #[test]
    fn disjoint_trains_derive_no_coupling() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "main-cam", "alpaca_url": "http://x"},
                {"id": "guide-cam", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "main-focuser", "alpaca_url": "http://x"},
                {"id": "guide-focuser", "alpaca_url": "http://x"}
            ],
            "optical_trains": [
                {"id": "main", "devices": ["main-focuser", "main-cam"]},
                {"id": "guide", "devices": ["guide-focuser", "guide-cam"]}
            ]
        }));
        let model = TrainModel::try_from_equipment(&config).unwrap();
        assert_eq!(model.trains_with_device("main-focuser").len(), 1);
        assert_eq!(
            model.af_sequence("guide").unwrap(),
            vec![AfStep {
                focuser_id: "guide-focuser".to_string(),
                train_id: "guide".to_string()
            }]
        );
        assert!(model.guiding_train().is_none());
    }

    #[test]
    fn no_trains_is_the_default_empty_model() {
        let config = equipment(serde_json::json!({
            "cameras": [{"id": "main-cam", "alpaca_url": "http://x"}]
        }));
        let model = TrainModel::try_from_equipment(&config).unwrap();
        assert!(model.trains().is_empty());
        assert!(model.focal_length_for_camera("main-cam").is_none());
    }

    #[test]
    fn camera_only_train_is_valid() {
        let config = equipment(serde_json::json!({
            "cameras": [{"id": "main-cam", "alpaca_url": "http://x"}],
            "optical_trains": [
                {"id": "main", "focal_length_mm": 1000.0, "devices": ["main-cam"]}
            ]
        }));
        let model = TrainModel::try_from_equipment(&config).unwrap();
        assert_eq!(model.focal_length_for_camera("main-cam"), Some(1000.0));
        assert!(model.focuser_for_camera("main-cam").is_none());
        assert_eq!(model.af_sequence("main").unwrap(), vec![]);
    }

    #[test]
    fn unknown_device_and_wrong_kind_are_rejected_with_entry_paths() {
        let config = equipment(serde_json::json!({
            "cameras": [{"id": "main-cam", "alpaca_url": "http://x"}],
            "switches": [{"id": "ppba", "alpaca_url": "http://x"}],
            "optical_trains": [
                {"id": "main", "devices": ["ghost", "ppba", "main-cam"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.0.devices.0",
                "equipment.optical_trains.0.devices.1",
            ]
        );
        assert!(errors[0].msg.contains("not in the equipment roster"));
        assert!(errors[1].msg.contains("is a switch"), "{}", errors[1].msg);
    }

    #[test]
    fn empty_and_camera_less_trains_are_rejected() {
        let config = equipment(serde_json::json!({
            "focusers": [{"id": "f1", "alpaca_url": "http://x"}],
            "optical_trains": [
                {"id": "empty", "devices": []},
                {"id": "no-cam", "devices": ["f1"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.0.devices",
                "equipment.optical_trains.1.devices.0",
            ]
        );
        assert!(errors[1].msg.contains("last device must be a camera"));
    }

    #[test]
    fn cameras_before_the_end_and_double_terminated_cameras_are_rejected() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "cam-a", "alpaca_url": "http://x"},
                {"id": "cam-b", "alpaca_url": "http://x"}
            ],
            "focusers": [{"id": "f1", "alpaca_url": "http://x"}],
            "optical_trains": [
                {"id": "one", "devices": ["cam-a", "cam-b"]},
                {"id": "two", "devices": ["f1", "cam-b"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.0.devices.0",
                "equipment.optical_trains.1.devices.1",
            ]
        );
        assert!(errors[0].msg.contains("may only terminate"));
        // The conflict names the train that reserved the camera, so the
        // operator knows both ends of it.
        assert!(
            errors[1].msg.contains("already terminates train 'one'"),
            "{}",
            errors[1].msg
        );
    }

    #[test]
    fn duplicate_train_ids_and_repeated_devices_are_rejected() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "cam-a", "alpaca_url": "http://x"},
                {"id": "cam-b", "alpaca_url": "http://x"}
            ],
            "focusers": [{"id": "f1", "alpaca_url": "http://x"}],
            "optical_trains": [
                {"id": "main", "devices": ["f1", "f1", "cam-a"]},
                {"id": "main", "devices": ["cam-b"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(
            paths(&errors),
            vec![
                "equipment.optical_trains.0.devices.1",
                "equipment.optical_trains.1.id",
            ]
        );
        assert!(errors[0].msg.contains("repeats within train"));
        assert!(errors[1].msg.contains("duplicate train id"));
    }

    #[test]
    fn second_guiding_train_is_rejected() {
        let mut config = reference_rig();
        config.optical_trains[0].purpose = TrainPurpose::Guiding;
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(paths(&errors), vec!["equipment.optical_trains.1.purpose"]);
        assert!(errors[0].msg.contains("at most one train"));
    }

    #[test]
    fn guiding_train_without_mount_guiding_is_rejected() {
        let mut config = reference_rig();
        config.mount.as_mut().unwrap().guiding = None;
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(paths(&errors), vec!["equipment.optical_trains.1.purpose"]);
        assert!(errors[0].msg.contains("requires equipment.mount.guiding"));

        // No mount at all is the same violation.
        config.mount = None;
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(paths(&errors), vec!["equipment.optical_trains.1.purpose"]);
    }

    #[test]
    fn contradictory_shared_order_is_rejected_as_a_cycle() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "cam-a", "alpaca_url": "http://x"},
                {"id": "cam-b", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "f1", "alpaca_url": "http://x"},
                {"id": "f2", "alpaca_url": "http://x"}
            ],
            "optical_trains": [
                {"id": "one", "devices": ["f1", "f2", "cam-a"]},
                {"id": "two", "devices": ["f2", "f1", "cam-b"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(paths(&errors), vec!["equipment.optical_trains"]);
        assert!(
            errors[0].msg.contains("contradictory order"),
            "{}",
            errors[0].msg
        );
        assert!(errors[0].msg.contains("f1") && errors[0].msg.contains("f2"));
    }

    /// A non-adjacent contradiction still closes a cycle through the
    /// adjacency chains.
    #[test]
    fn non_adjacent_order_contradiction_is_still_a_cycle() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "cam-a", "alpaca_url": "http://x"},
                {"id": "cam-b", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "f1", "alpaca_url": "http://x"},
                {"id": "f2", "alpaca_url": "http://x"}
            ],
            "rotators": [
                {"id": "r1", "alpaca_url": "http://x"},
                {"id": "r2", "alpaca_url": "http://x"}
            ],
            "optical_trains": [
                {"id": "one", "devices": ["f1", "r1", "f2", "cam-a"]},
                {"id": "two", "devices": ["f2", "r2", "f1", "cam-b"]}
            ]
        }));
        let errors = TrainModel::try_from_equipment(&config).unwrap_err();
        assert_eq!(paths(&errors), vec!["equipment.optical_trains"]);
    }

    /// Consistent shared order across trains is fine — the reference
    /// rig itself shares the EAF at the head of both lists.
    #[test]
    fn consistent_shared_order_is_accepted() {
        let model = TrainModel::try_from_equipment(&reference_rig());
        assert!(model.is_ok());
    }

    /// A shared focuser that is terminal nowhere falls back to running
    /// in the triggering train.
    #[test]
    fn af_sequence_falls_back_to_the_triggering_train() {
        let config = equipment(serde_json::json!({
            "cameras": [
                {"id": "cam-a", "alpaca_url": "http://x"},
                {"id": "cam-b", "alpaca_url": "http://x"}
            ],
            "focusers": [
                {"id": "shared", "alpaca_url": "http://x"},
                {"id": "local-a", "alpaca_url": "http://x"},
                {"id": "local-b", "alpaca_url": "http://x"}
            ],
            "optical_trains": [
                {"id": "a", "devices": ["shared", "local-a", "cam-a"]},
                {"id": "b", "devices": ["shared", "local-b", "cam-b"]}
            ]
        }));
        let model = TrainModel::try_from_equipment(&config).unwrap();
        assert_eq!(
            model.af_sequence("a").unwrap(),
            vec![
                AfStep {
                    focuser_id: "shared".to_string(),
                    train_id: "a".to_string()
                },
                AfStep {
                    focuser_id: "local-a".to_string(),
                    train_id: "a".to_string()
                },
            ]
        );
    }
}

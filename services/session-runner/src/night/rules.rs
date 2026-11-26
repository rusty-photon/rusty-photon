//! The cut rules (design § Night Schedule → The cut rules): which daily
//! outputs the schedule may hand `power_down`.
//!
//! The rules read what the schedule read from `rp` just before: each
//! camera's cooler, the mount's park state, each cover's state. They
//! hold an output while a device on it would come to harm from the cut:
//! a running TEC, a camera that cannot be read before the warm-up's time
//! is up, a mount away from its park or one whose power-up would lose its
//! pose, a cover that is not closed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::state::OutputRef;

/// When the rules run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Dawn,
    AfterDawn,
    /// A dusk wedge retry cycling a camera's outputs.
    WedgeRetry,
}

/// A daily output and the devices the power map puts on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedOutput {
    pub output: OutputRef,
    pub devices: Vec<MappedDevice>,
}

/// One device on an output: its roster id and `rp`'s kind name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedDevice {
    pub device_id: String,
    pub kind: String,
}

/// What `get_camera_info` said of a camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CameraReading {
    /// `cooler_running`: `None` for a camera with no cooler.
    Cooler(Option<bool>),
    /// No session in `rp`, or a failed read.
    Unreadable,
}

/// What `get_park_state` said of the mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParkReading {
    Read {
        at_park: bool,
        /// `park_matches_power_up_pose`.
        pose_kept: Option<bool>,
    },
    Failed,
}

/// What `get_cover_state` said of a calibrator's cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverReading {
    /// `cover_state`, e.g. `Closed` or `Open`.
    State(String),
    Failed,
}

/// Every reading the rules take, keyed by device id. A device with no
/// reading counts as one whose read failed.
#[derive(Clone, Debug, Default)]
pub struct Readings {
    pub cameras: BTreeMap<String, CameraReading>,
    pub park: Option<ParkReading>,
    pub covers: BTreeMap<String, CoverReading>,
}

/// What the rules need besides the readings.
#[derive(Clone, Debug)]
pub struct Context<'a> {
    pub when: When,
    pub now: DateTime<Utc>,
    /// When dawn called `start_warmup`.
    pub warmup_started_at: Option<DateTime<Utc>>,
    pub warmup_timeout: Duration,
    /// The daily outputs that did not read on before dusk's first
    /// `power_up`; a wedge retry treats them as powered up this dusk.
    pub off_before_dusk: &'a BTreeSet<OutputRef>,
}

/// Why `output` stays on, or `None` when the rules let it be cut.
#[must_use]
pub fn hold_reason(
    output: &MappedOutput,
    readings: &Readings,
    ctx: &Context<'_>,
) -> Option<String> {
    // An output every device on it was unpowered on all day: no running
    // cooler, no camera that warmed by day, and a mount the power-up
    // just seeded.
    let powered_up_this_dusk =
        ctx.when == When::WedgeRetry && ctx.off_before_dusk.contains(&output.output);
    let of_kind = |kind: &'static str| {
        output
            .devices
            .iter()
            .filter(move |d| d.kind == kind)
            .map(|d| d.device_id.as_str())
    };
    if !powered_up_this_dusk {
        for camera in of_kind("camera") {
            if let Some(reason) = camera_hold(camera, readings.cameras.get(camera), ctx) {
                return Some(reason);
            }
        }
    }
    if of_kind("mount").next().is_some() {
        if let Some(reason) = mount_hold(readings.park.as_ref(), powered_up_this_dusk) {
            return Some(reason);
        }
    }
    for calibrator in of_kind("cover_calibrator") {
        if let Some(reason) = cover_hold(calibrator, readings.covers.get(calibrator)) {
            return Some(reason);
        }
    }
    None
}

fn camera_hold(camera: &str, reading: Option<&CameraReading>, ctx: &Context<'_>) -> Option<String> {
    match reading {
        Some(CameraReading::Cooler(Some(true))) => Some(format!("{camera}'s cooler is running")),
        Some(CameraReading::Cooler(_)) => None,
        Some(CameraReading::Unreadable) | None => {
            // An unreadable camera has had the warm-up's whole time to
            // warm once it is up; at a wedge retry no warm-up has run.
            let warmed = ctx.when != When::WedgeRetry
                && ctx.warmup_started_at.is_some_and(|started| {
                    chrono::TimeDelta::from_std(ctx.warmup_timeout)
                        .ok()
                        .and_then(|timeout| started.checked_add_signed(timeout))
                        .is_some_and(|up| ctx.now >= up)
                });
            (!warmed).then(|| format!("{camera} cannot be read"))
        }
    }
}

fn mount_hold(park: Option<&ParkReading>, powered_up_this_dusk: bool) -> Option<String> {
    match park {
        Some(ParkReading::Read { at_park, pose_kept }) => {
            if !*at_park && !powered_up_this_dusk {
                return Some("the mount is not parked".to_owned());
            }
            match pose_kept {
                Some(true) => None,
                Some(false) => Some("a cut would lose the mount's pose".to_owned()),
                None => Some("the mount does not say a cut keeps its pose".to_owned()),
            }
        }
        Some(ParkReading::Failed) | None => {
            Some("the mount's park state cannot be read".to_owned())
        }
    }
}

fn cover_hold(calibrator: &str, reading: Option<&CoverReading>) -> Option<String> {
    match reading {
        Some(CoverReading::State(state)) if state == "Closed" || state == "NotPresent" => None,
        Some(CoverReading::State(state)) => Some(format!("{calibrator}'s cover reads {state}")),
        Some(CoverReading::Failed) | None => {
            Some(format!("{calibrator}'s cover state cannot be read"))
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn output(name: &str, devices: &[(&str, &str)]) -> MappedOutput {
        MappedOutput {
            output: OutputRef {
                switch_id: "ppba".into(),
                output: name.into(),
            },
            devices: devices
                .iter()
                .map(|(id, kind)| MappedDevice {
                    device_id: (*id).into(),
                    kind: (*kind).into(),
                })
                .collect(),
        }
    }

    fn dawn_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap()
    }

    fn ctx(when: When, off_before_dusk: &BTreeSet<OutputRef>) -> Context<'_> {
        Context {
            when,
            now: dawn_time(),
            warmup_started_at: Some(dawn_time()),
            warmup_timeout: Duration::from_mins(45),
            off_before_dusk,
        }
    }

    fn cameras(readings: &[(&str, CameraReading)]) -> Readings {
        Readings {
            cameras: readings
                .iter()
                .map(|(id, r)| ((*id).to_owned(), r.clone()))
                .collect(),
            ..Readings::default()
        }
    }

    fn parked(pose_kept: Option<bool>) -> Readings {
        Readings {
            park: Some(ParkReading::Read {
                at_park: true,
                pose_kept,
            }),
            ..Readings::default()
        }
    }

    const NONE: &BTreeSet<OutputRef> = &BTreeSet::new();

    #[test]
    fn an_output_with_no_device_is_cut() {
        assert_eq!(
            hold_reason(
                &output("Dew", &[]),
                &Readings::default(),
                &ctx(When::Dawn, NONE)
            ),
            None
        );
    }

    #[test]
    fn a_running_cooler_holds_its_output() {
        let out = output("Quad", &[("main-cam", "camera")]);
        let readings = cameras(&[("main-cam", CameraReading::Cooler(Some(true)))]);
        assert_eq!(
            hold_reason(&out, &readings, &ctx(When::Dawn, NONE)).as_deref(),
            Some("main-cam's cooler is running")
        );
    }

    #[test]
    fn a_stopped_cooler_and_a_camera_without_one_both_cut() {
        let out = output("Quad", &[("main-cam", "camera"), ("guide-cam", "camera")]);
        let readings = cameras(&[
            ("main-cam", CameraReading::Cooler(Some(false))),
            ("guide-cam", CameraReading::Cooler(None)),
        ]);
        assert_eq!(hold_reason(&out, &readings, &ctx(When::Dawn, NONE)), None);
    }

    #[test]
    fn an_unreadable_camera_holds_until_the_warmup_time_is_up() {
        let out = output("Quad", &[("main-cam", "camera")]);
        let readings = cameras(&[("main-cam", CameraReading::Unreadable)]);
        let mut early = ctx(When::AfterDawn, NONE);
        early.now = dawn_time() + chrono::TimeDelta::minutes(44);
        assert_eq!(
            hold_reason(&out, &readings, &early).as_deref(),
            Some("main-cam cannot be read")
        );
        let mut late = ctx(When::AfterDawn, NONE);
        late.now = dawn_time() + chrono::TimeDelta::minutes(45);
        assert_eq!(hold_reason(&out, &readings, &late), None);
    }

    #[test]
    fn a_camera_with_no_reading_counts_as_unreadable() {
        let out = output("Quad", &[("main-cam", "camera")]);
        assert_eq!(
            hold_reason(&out, &Readings::default(), &ctx(When::Dawn, NONE)).as_deref(),
            Some("main-cam cannot be read")
        );
    }

    #[test]
    fn an_unreadable_camera_holds_while_no_warmup_has_started() {
        let out = output("Quad", &[("main-cam", "camera")]);
        let readings = cameras(&[("main-cam", CameraReading::Unreadable)]);
        let mut no_warmup = ctx(When::Dawn, NONE);
        no_warmup.warmup_started_at = None;
        no_warmup.now = dawn_time() + chrono::TimeDelta::hours(3);
        assert!(hold_reason(&out, &readings, &no_warmup).is_some());
    }

    #[test]
    fn a_mount_cuts_only_parked_and_vouched_for() {
        let out = output("Quad", &[("mount", "mount")]);
        let c = ctx(When::Dawn, NONE);
        assert_eq!(hold_reason(&out, &parked(Some(true)), &c), None);
        assert_eq!(
            hold_reason(&out, &parked(Some(false)), &c).as_deref(),
            Some("a cut would lose the mount's pose")
        );
        assert_eq!(
            hold_reason(&out, &parked(None), &c).as_deref(),
            Some("the mount does not say a cut keeps its pose")
        );
        let unparked = Readings {
            park: Some(ParkReading::Read {
                at_park: false,
                pose_kept: Some(true),
            }),
            ..Readings::default()
        };
        assert_eq!(
            hold_reason(&out, &unparked, &c).as_deref(),
            Some("the mount is not parked")
        );
        let failed = Readings {
            park: Some(ParkReading::Failed),
            ..Readings::default()
        };
        assert_eq!(
            hold_reason(&out, &failed, &c).as_deref(),
            Some("the mount's park state cannot be read")
        );
    }

    #[test]
    fn a_cover_cuts_only_closed_or_not_present() {
        let out = output("Panel", &[("flat-panel", "cover_calibrator")]);
        let c = ctx(When::Dawn, NONE);
        for state in ["Closed", "NotPresent"] {
            let readings = Readings {
                covers: [("flat-panel".to_owned(), CoverReading::State(state.into()))].into(),
                ..Readings::default()
            };
            assert_eq!(hold_reason(&out, &readings, &c), None, "{state}");
        }
        let open = Readings {
            covers: [("flat-panel".to_owned(), CoverReading::State("Open".into()))].into(),
            ..Readings::default()
        };
        assert_eq!(
            hold_reason(&out, &open, &c).as_deref(),
            Some("flat-panel's cover reads Open")
        );
        let failed = Readings {
            covers: [("flat-panel".to_owned(), CoverReading::Failed)].into(),
            ..Readings::default()
        };
        assert_eq!(
            hold_reason(&out, &failed, &c).as_deref(),
            Some("flat-panel's cover state cannot be read")
        );
    }

    #[test]
    fn a_wedge_retry_on_an_output_powered_up_this_dusk_skips_the_cameras_and_at_park() {
        let out = output("Quad", &[("main-cam", "camera"), ("mount", "mount")]);
        let off: BTreeSet<OutputRef> = [out.output.clone()].into();
        let readings = Readings {
            cameras: [("main-cam".to_owned(), CameraReading::Unreadable)].into(),
            park: Some(ParkReading::Read {
                at_park: false,
                pose_kept: Some(true),
            }),
            covers: BTreeMap::new(),
        };
        assert_eq!(
            hold_reason(&out, &readings, &ctx(When::WedgeRetry, &off)),
            None
        );
        let unvouched = Readings {
            park: Some(ParkReading::Read {
                at_park: false,
                pose_kept: None,
            }),
            ..readings
        };
        assert_eq!(
            hold_reason(&out, &unvouched, &ctx(When::WedgeRetry, &off)).as_deref(),
            Some("the mount does not say a cut keeps its pose")
        );
    }

    #[test]
    fn dawn_applies_every_rule_to_an_output_powered_up_this_dusk() {
        let out = output("Quad", &[("main-cam", "camera"), ("mount", "mount")]);
        let off: BTreeSet<OutputRef> = [out.output.clone()].into();
        let cooling = cameras(&[("main-cam", CameraReading::Cooler(Some(true)))]);
        assert_eq!(
            hold_reason(&out, &cooling, &ctx(When::Dawn, &off)).as_deref(),
            Some("main-cam's cooler is running")
        );
        let unparked = Readings {
            cameras: [("main-cam".to_owned(), CameraReading::Cooler(Some(false)))].into(),
            park: Some(ParkReading::Read {
                at_park: false,
                pose_kept: Some(true),
            }),
            covers: BTreeMap::new(),
        };
        assert_eq!(
            hold_reason(&out, &unparked, &ctx(When::AfterDawn, &off)).as_deref(),
            Some("the mount is not parked")
        );
    }

    #[test]
    fn a_wedge_retry_on_an_output_already_on_applies_every_rule() {
        let out = output("Quad", &[("mount", "mount")]);
        let readings = Readings {
            park: Some(ParkReading::Read {
                at_park: false,
                pose_kept: Some(true),
            }),
            ..Readings::default()
        };
        assert_eq!(
            hold_reason(&out, &readings, &ctx(When::WedgeRetry, NONE)).as_deref(),
            Some("the mount is not parked")
        );
    }

    #[test]
    fn a_wedge_retry_holds_an_output_already_on_for_an_unreadable_camera_outright() {
        let out = output("Quad", &[("guide-cam", "camera")]);
        let readings = cameras(&[("guide-cam", CameraReading::Unreadable)]);
        let mut retry = ctx(When::WedgeRetry, NONE);
        retry.now = dawn_time() + chrono::TimeDelta::hours(5);
        assert_eq!(
            hold_reason(&out, &readings, &retry).as_deref(),
            Some("guide-cam cannot be read")
        );
    }

    #[test]
    fn a_wedge_retry_on_an_output_powered_up_this_dusk_still_holds_an_open_cover() {
        let out = output("Quad", &[("flat-panel", "cover_calibrator")]);
        let off: BTreeSet<OutputRef> = [out.output.clone()].into();
        let readings = Readings {
            covers: [("flat-panel".to_owned(), CoverReading::State("Open".into()))].into(),
            ..Readings::default()
        };
        assert!(hold_reason(&out, &readings, &ctx(When::WedgeRetry, &off)).is_some());
    }
}

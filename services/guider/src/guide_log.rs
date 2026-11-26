//! The guide log, in the format PHD2 writes (docs/services/guider.md
//! § Guide log).
//!
//! A file in `guide-logs/` of the state directory for each night: the first
//! calibration or guiding run after each local noon, and the first after
//! the service starts, opens a file, named by the local time it was opened.
//! The file is one block, from PHD2's version line to its summary and
//! `Log closed at` line. A calibration or a guiding run stays in the file it
//! began in. Each time it opens a file, the log removes the files older than
//! `guide_log.retention`.
//!
//! A log that cannot be written never stops guiding: the failure is
//! logged, and the log is off until the next file opens.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Days, FixedOffset, Local, NaiveTime};
use rp_guiding::{
    AxisOffset, Calibration, Direction, GuideRates, Leg, Offset, PierSide, Position, Pulse,
};
use tracing::{debug, warn};

use crate::config::GuideLogConfig;

/// The guide logs' directory in the state directory.
pub const DIR: &str = "guide-logs";

/// What the files' names begin and end with.
const PREFIX: &str = "PHD2_GuideLog_";
const SUFFIX: &str = ".txt";

/// The platform's line ending, as PHD2 writes it.
const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// The column heading of a guiding run.
const COLUMNS: &str = "Frame,Time,mount,dx,dy,RARawDistance,DECRawDistance,RAGuideDistance,DECGuideDistance,RADuration,RADirection,DECDuration,DECDirection,XStep,YStep,StarMass,SNR,ErrorCode";

/// The error code and the reason of a star-lost frame.
const STAR_LOST_CODE: u32 = 5;
const STAR_LOST: &str = "Too few guide stars usable";

/// The local time now.
pub type Clock = Box<dyn Fn() -> DateTime<FixedOffset> + Send + Sync>;

/// The guide log.
pub struct GuideLog {
    dir: PathBuf,
    config: GuideLogConfig,
    clock: Clock,
    file: Option<Open>,
    /// After a failure, the time the next file may open.
    off_until: Option<DateTime<FixedOffset>>,
}

/// The file being written.
struct Open {
    path: PathBuf,
    file: File,
    /// The first local noon after the file opened, from which the next
    /// calibration or guiding run opens a new file.
    next_noon: DateTime<FixedOffset>,
    /// The calibrations passed, the runs ended and the seconds they guided.
    calibrations: u32,
    runs: u32,
    guided: Duration,
    section: Section,
}

/// The section a file is in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Section {
    None,
    Calibration,
    /// A guiding run, begun at a time on the loop's clock, with the frames
    /// it has written.
    Guiding {
        began: Duration,
        frames: u64,
    },
}

/// The pointing a run's header gives, read when the run starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointing {
    pub ra_hours: f64,
    pub altitude_deg: f64,
    pub azimuth_deg: f64,
    /// The local sidereal time, in hours.
    pub sidereal_hours: f64,
}

impl Pointing {
    /// The hour angle, from -12 h up to 12 h.
    #[must_use]
    pub fn hour_angle(&self) -> f64 {
        (self.sidereal_hours - self.ra_hours + 12.0).rem_euclid(24.0) - 12.0
    }
}

/// What both kinds of header give.
#[derive(Debug, Clone)]
pub struct Common {
    pub pixel_scale: f64,
    pub binning: u32,
    pub focal_length_mm: f64,
    pub search_region_px: f64,
    /// `stars.mass_change`, as a fraction.
    pub mass_tolerance: f64,
    /// The guide stars selected.
    pub stars: usize,
    pub camera: String,
    pub gain: Option<i32>,
    pub width: u32,
    pub height: u32,
    pub pixel_size_um: f64,
    pub exposure: Duration,
    pub mount: String,
    pub guide_rates: Option<GuideRates>,
    pub pointing: Option<Pointing>,
    pub declination_deg: f64,
    pub pier_side: PierSide,
    /// The lock and the first guide star on the sensor.
    pub lock: Position,
    pub star: Position,
    pub hfd: Option<f64>,
}

/// The controllers' settings a guiding run's header gives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    pub ra_hysteresis: f64,
    pub ra_aggression: f64,
    pub dec_aggression: f64,
    pub dec_reversal_frames: u32,
    /// `control.dec.mode` as the header names it, `None` with Dec off.
    pub dec_mode: Option<&'static str>,
    pub min_move_px: f64,
    pub max_pulse: Duration,
}

/// A guide step's row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideRow {
    /// The stars' offset from the lock on the sensor.
    pub from_lock: Offset,
    /// The error on each axis, as the controller works it out.
    pub error: AxisOffset,
    pub ra: Option<Pulse>,
    pub dec: Option<Pulse>,
    /// What a second of each pulse moves the stars, in pixels.
    pub ra_rate_px_per_s: f64,
    pub dec_rate_px_per_s: f64,
    pub flux: Option<f64>,
    pub snr: Option<f64>,
}

/// A calibration step's row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationRow {
    pub leg: Leg,
    pub step: u32,
    /// Where the stars were when the leg began, and where they are.
    pub start: Offset,
    pub now: Offset,
    /// The first guide star on the sensor.
    pub star: Position,
}

impl GuideLog {
    /// The log in `state_dir`, on the local clock.
    #[must_use]
    pub fn new(state_dir: &Path, config: GuideLogConfig) -> Self {
        Self::with_clock(state_dir, config, Box::new(|| Local::now().fixed_offset()))
    }

    /// The log in `state_dir` on `clock`.
    #[must_use]
    pub fn with_clock(state_dir: &Path, config: GuideLogConfig, clock: Clock) -> Self {
        Self {
            dir: state_dir.join(DIR),
            config,
            clock,
            file: None,
            off_until: None,
        }
    }

    /// A calibration begins.
    pub fn begin_calibration(&mut self, common: &Common, step: Duration, distance_px: f64) {
        if !self.ready() {
            return;
        }
        let mut lines = vec![
            String::new(),
            format!("Calibration Begins at {}", self.time()),
        ];
        lines.extend(common_lines(common, |mount| {
            format!(
                "Mount = {mount}, Calibration Step = {} ms, Calibration Distance = {distance_px:.0} px, Assume orthogonal axes = no",
                step.as_millis()
            )
        }));
        lines.push("Direction,Step,dx,dy,x,y,Dist".to_owned());
        self.section(Section::Calibration, &lines);
    }

    /// A frame of a calibration leg.
    pub fn calibration_row(&mut self, row: &CalibrationRow) {
        if !self.is_in(|section| section == Section::Calibration) {
            return;
        }
        // PHD2 gives the displacement the way the mount moved: the leg's
        // start less where the stars are.
        let dx = row.start.x - row.now.x;
        let dy = row.start.y - row.now.y;
        let leg = match row.leg {
            Leg::West => "West",
            Leg::East => "East",
            Leg::ClearBacklash => "Backlash",
            Leg::North => "North",
            Leg::South => "South",
        };
        self.write(&[format!(
            "{leg},{},{:.3},{:.3},{:.3},{:.3},{:.3}",
            row.step,
            unsigned_zero(dx),
            unsigned_zero(dy),
            row.star.x,
            row.star.y,
            dx.hypot(dy)
        )]);
    }

    /// The West or the North leg has measured its axis: `angle_deg` the way
    /// a pulse moves the stars, and `rate_px_per_s` how fast.
    pub fn leg_complete(&mut self, leg: Leg, angle_deg: f64, rate_px_per_s: f64) {
        if !self.is_in(|section| section == Section::Calibration) {
            return;
        }
        let name = if leg == Leg::West { "West" } else { "North" };
        self.write(&[format!(
            "{name} calibration complete. Angle = {:.1} deg, Rate = {rate_px_per_s:.3} px/sec, Parity = N/A",
            mount_angle(angle_deg)
        )]);
    }

    /// A star-lost frame of a calibration.
    pub fn calibration_star_lost(&mut self, flux: Option<f64>, snr: Option<f64>) {
        self.note(&format!(
            "STAR LOST during calibration, Mass= {:.0}, SNR= {:.2}, Error= {STAR_LOST_CODE}, Status={STAR_LOST}",
            flux.unwrap_or(0.0),
            snr.unwrap_or(0.0)
        ));
    }

    /// A calibration passed.
    pub fn calibration_complete(&mut self, mount: &str) {
        if !self.is_in(|section| section == Section::Calibration) {
            return;
        }
        self.write(&[format!("Calibration complete, mount = {mount}.")]);
        if let Some(open) = &mut self.file {
            open.calibrations = open.calibrations.saturating_add(1);
            open.section = Section::None;
        }
    }

    /// A calibration failed.
    pub fn calibration_failed(&mut self, message: &str) {
        if !self.is_in(|section| section == Section::Calibration) {
            return;
        }
        self.note(&format!("CALIBRATION FAILED, {message}"));
        if let Some(open) = &mut self.file {
            open.section = Section::None;
        }
    }

    /// A guiding run begins at `at` on the loop's clock.
    pub fn begin_guiding(
        &mut self,
        common: &Common,
        calibration: &Calibration,
        control: &Control,
        at: Duration,
    ) {
        if !self.ready() {
            return;
        }
        let mut lines = vec![String::new(), format!("Guiding Begins at {}", self.time())];
        let scale = common.pixel_scale;
        let mut header = common_lines(common, |mount| {
            let y = calibration.dec.as_ref().map_or_else(String::new, |dec| {
                format!(
                    ", yAngle = {:.1}, yRate = {:.3}",
                    mount_angle(dec.axis.angle_deg),
                    dec.axis.rate_px_per_s
                )
            });
            format!(
                "Mount = {mount}, connected, guiding enabled, xAngle = {:.1}, xRate = {:.3}{y}, parity = N/A",
                mount_angle(calibration.ra.angle_deg),
                calibration.ra_rate_at(calibration.declination_deg)
            )
        });
        // After the mount line come the rates, the controllers, the pulse
        // bounds and the guide speeds, as PHD2 orders them.
        let at_mount = header
            .iter()
            .position(|line| line.starts_with("Mount = "))
            .map_or(header.len(), |at| at.saturating_add(1));
        let mut control_lines = vec![];
        let dec_rate = calibration.dec.as_ref().map_or_else(String::new, |dec| {
            format!(", Dec = {:.1}\"/s", dec.axis.rate_px_per_s * scale)
        });
        let ortho = calibration
            .orthogonality_error_deg()
            .map_or_else(String::new, |error| {
                format!("; ortho.err. = {error:.1} deg")
            });
        control_lines.push(format!(
            "Norm rates RA = {:.1}\"/s @ dec 0{dec_rate}{ortho}",
            calibration.ra.rate_px_per_s * scale
        ));
        control_lines.push(format!(
            "X guide algorithm = Hysteresis, Hysteresis = {:.3}, Aggression = {:.3}, Minimum move = {:.3}",
            control.ra_hysteresis, control.ra_aggression, control.min_move_px
        ));
        control_lines.push(match control.dec_mode {
            Some(_) if calibration.dec.is_some() => format!(
                "Y guide algorithm = Resist Switch, Minimum move = {:.3} Aggression = {:.0}% Reversal frames = {}",
                control.min_move_px,
                control.dec_aggression * 100.0,
                control.dec_reversal_frames
            ),
            _ => "Y guide algorithm = None".to_owned(),
        });
        control_lines.push("Backlash comp = disabled, pulse = 0 ms".to_owned());
        control_lines.push(format!(
            "Max RA duration = {max}, Max DEC duration = {max}, DEC guide mode = {}",
            control.dec_mode.unwrap_or("Off"),
            max = control.max_pulse.as_millis()
        ));
        header.splice(at_mount..at_mount, control_lines);
        if let Some(speeds) = header
            .iter_mut()
            .find(|line| line.starts_with("RA Guide Speed = "))
        {
            let issue = calibration
                .warnings
                .first()
                .map_or_else(|| "None".to_owned(), |warning| warning.check.to_string());
            *speeds = format!(
                "{speeds}, Cal Dec = {:.1}, Last Cal Issue = {issue}",
                calibration.declination_deg
            );
        }
        lines.extend(header);
        lines.push(COLUMNS.to_owned());
        self.section(
            Section::Guiding {
                began: at,
                frames: 0,
            },
            &lines,
        );
    }

    /// A guide step at `at` on the loop's clock.
    pub fn guide_step(&mut self, row: &GuideRow, at: Duration) {
        let Some((frame, time)) = self.next_frame(at) else {
            return;
        };
        let (ra_ms, ra_direction, ra_guide) = pulse_fields(row.ra, row.ra_rate_px_per_s);
        let (dec_ms, dec_direction, dec_guide) = pulse_fields(row.dec, row.dec_rate_px_per_s);
        // PHD2 corrects a positive RA distance West and a positive Dec
        // distance South; the controller corrects a positive RA error East
        // and a positive Dec error South.
        self.write(&[format!(
            "{frame},{time:.3},\"Mount\",{:.3},{:.3},{:.3},{:.3},{ra_guide:.3},{dec_guide:.3},{ra_ms},{ra_direction},{dec_ms},{dec_direction},,,{:.0},{:.2},0",
            unsigned_zero(row.from_lock.x),
            unsigned_zero(row.from_lock.y),
            unsigned_zero(-row.error.ra),
            unsigned_zero(row.error.dec),
            row.flux.unwrap_or(0.0),
            row.snr.unwrap_or(0.0)
        )]);
    }

    /// A star-lost frame of a guiding run at `at` on the loop's clock.
    pub fn star_lost(&mut self, flux: Option<f64>, snr: Option<f64>, at: Duration) {
        let Some((frame, time)) = self.next_frame(at) else {
            return;
        };
        self.write(&[format!(
            "{frame},{time:.3},\"DROP\",,,,,,,,,,,,,{:.0},{:.2},{STAR_LOST_CODE},\"{STAR_LOST}\"",
            flux.unwrap_or(0.0),
            snr.unwrap_or(0.0)
        )]);
    }

    /// A note, in a calibration or a guiding run.
    pub fn note(&mut self, note: &str) {
        if !self.is_in(|section| section != Section::None) {
            return;
        }
        self.write(&[format!("INFO: {note}")]);
    }

    /// The guiding run ends at `at` on the loop's clock.
    pub fn guiding_ends(&mut self, at: Duration) {
        let Some(open) = &mut self.file else {
            return;
        };
        let Section::Guiding { began, .. } = open.section else {
            return;
        };
        open.runs = open.runs.saturating_add(1);
        open.guided = open.guided.saturating_add(at.saturating_sub(began));
        open.section = Section::None;
        let line = format!("Guiding Ends at {}", self.time());
        self.write(&[line]);
    }

    /// The run in progress stopped at `at`, with `note` saying why.
    pub fn stopped(&mut self, note: &str, at: Duration) {
        self.note(note);
        match self.file.as_ref().map(|open| open.section) {
            Some(Section::Guiding { .. }) => self.guiding_ends(at),
            Some(Section::Calibration) => {
                if let Some(open) = &mut self.file {
                    open.section = Section::None;
                }
            }
            Some(Section::None) | None => {}
        }
    }

    /// Close the file, as the service stops.
    pub fn close(&mut self) {
        self.close_file();
    }

    /// The path of the file being written.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|open| open.path.as_path())
    }

    /// Whether a section may begin: the log is on and a file is open, a new
    /// one opened when the night has turned.
    fn ready(&mut self) -> bool {
        if !self.config.enabled {
            return false;
        }
        let now = (self.clock)();
        if let Some(until) = self.off_until {
            if now < until {
                return false;
            }
            self.off_until = None;
        }
        if self.file.as_ref().is_some_and(|open| now >= open.next_noon) {
            self.close_file();
        }
        if self.file.is_none() {
            self.open_file(now);
        }
        self.file.is_some()
    }

    /// Whether a file is open in a section that `wanted` takes.
    fn is_in(&self, wanted: impl Fn(Section) -> bool) -> bool {
        self.file.as_ref().is_some_and(|open| wanted(open.section))
    }

    /// The next frame's number and its seconds since the run began.
    fn next_frame(&mut self, at: Duration) -> Option<(u64, f64)> {
        let open = self.file.as_mut()?;
        let Section::Guiding { began, frames } = open.section else {
            return None;
        };
        let frame = frames.saturating_add(1);
        open.section = Section::Guiding {
            began,
            frames: frame,
        };
        Some((frame, at.saturating_sub(began).as_secs_f64()))
    }

    fn section(&mut self, section: Section, lines: &[String]) {
        if let Some(open) = &mut self.file {
            open.section = section;
        }
        self.write(lines);
    }

    fn time(&self) -> String {
        format_time((self.clock)())
    }

    fn open_file(&mut self, now: DateTime<FixedOffset>) {
        self.remove_old(now);
        let path = self
            .dir
            .join(format!("{PREFIX}{}{SUFFIX}", now.format("%Y-%m-%d_%H%M%S")));
        let opened = std::fs::create_dir_all(&self.dir)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&path));
        match opened {
            Ok(file) => {
                debug!(path = %path.display(), "guide log opened");
                self.file = Some(Open {
                    path,
                    file,
                    next_noon: next_noon(now),
                    calibrations: 0,
                    runs: 0,
                    guided: Duration::ZERO,
                    section: Section::None,
                });
                self.write(&[format!(
                    "PHD2 version rusty-photon guider {} [{}], Log version 2.5. Log enabled at {}",
                    env!("CARGO_PKG_VERSION"),
                    platform(),
                    format_time(now)
                )]);
            }
            Err(error) => self.fail(&path, &error),
        }
    }

    fn close_file(&mut self) {
        let Some(open) = &self.file else {
            return;
        };
        let summary = format!(
            "Log Summary: calcnt:{} gcnt:{} gdur:{} gacnt:0",
            open.calibrations,
            open.runs,
            open.guided.as_secs()
        );
        let closed = format!("Log closed at {}", self.time());
        self.write(&[String::new(), summary, closed]);
        if let Some(open) = self.file.take() {
            debug!(path = %open.path.display(), "guide log closed");
        }
    }

    /// Remove the files of the log's directory older than the retention.
    fn remove_old(&self, now: DateTime<FixedOffset>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let cutoff = SystemTime::from(now)
            .checked_sub(self.config.retention.get())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(PREFIX) && name.ends_with(SUFFIX)) {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified < cutoff);
            if old {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => debug!(file = %name, "old guide log removed"),
                    Err(error) => {
                        warn!(file = %name, %error, "an old guide log could not be removed");
                    }
                }
            }
        }
    }

    /// Write `lines`, each with the line ending; a failure turns the log off
    /// until the next file opens.
    fn write(&mut self, lines: &[String]) {
        let Some(open) = &mut self.file else {
            return;
        };
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push_str(EOL);
        }
        if let Err(error) = open.file.write_all(text.as_bytes()) {
            let path = open.path.clone();
            self.fail(&path, &error);
        }
    }

    fn fail(&mut self, path: &Path, error: &std::io::Error) {
        let now = (self.clock)();
        warn!(path = %path.display(), %error, "the guide log could not be written; it is off until the next file");
        self.file = None;
        self.off_until = Some(next_noon(now));
    }
}

/// Where an offset along the calibrated axes lies on the sensor: the
/// inverse of the controller's reading of a sensor offset along the axes.
#[must_use]
pub fn to_sensor(calibration: &Calibration, along: AxisOffset) -> Offset {
    let (ra_sin, ra_cos) = calibration.ra.angle_deg.to_radians().sin_cos();
    let (dec_x, dec_y) = calibration.dec.as_ref().map_or((0.0, 0.0), |dec| {
        let (sin, cos) = dec.axis.angle_deg.to_radians().sin_cos();
        (along.dec * cos, along.dec * sin)
    });
    Offset {
        x: along.ra.mul_add(ra_cos, dec_x),
        y: along.ra.mul_add(ra_sin, dec_y),
    }
}

/// The lines both kinds of header give, with `mount_line` the mount's.
fn common_lines(common: &Common, mount_line: impl FnOnce(&str) -> String) -> Vec<String> {
    let gain = common
        .gain
        .map_or_else(String::new, |gain| format!(", gain = {gain}"));
    let camera = format!(
        " Camera = {}{gain}, full size = {} x {}, no dark, no defect map, pixel size = {:.1} um",
        common.camera, common.width, common.height, common.pixel_size_um
    );
    let speeds = common.guide_rates.map_or_else(
        || "RA Guide Speed = Unknown, Dec Guide Speed = Unknown".to_owned(),
        |rates| {
            format!(
                "RA Guide Speed = {:.1} a-s/s, Dec Guide Speed = {:.1} a-s/s",
                rates.ra_deg_per_s * 3_600.0,
                rates.dec_deg_per_s * 3_600.0
            )
        },
    );
    let side = match common.pier_side {
        PierSide::East => "East",
        PierSide::West => "West",
        PierSide::Unknown => "Unknown",
    };
    let pointing = common.pointing.map_or_else(
        || format!("RA/Dec = Unknown, Hour angle = Unknown, Pier side = {side}, Rotator pos = N/A, Alt = Unknown, Az = Unknown"),
        |pointing| {
            format!(
                "RA = {:.2} hr, Dec = {:.1} deg, Hour angle = {:.2} hr, Pier side = {side}, Rotator pos = N/A, Alt = {:.1} deg, Az = {:.1} deg",
                pointing.ra_hours,
                common.declination_deg,
                pointing.hour_angle(),
                pointing.altitude_deg,
                pointing.azimuth_deg
            )
        },
    );
    let hfd = common
        .hfd
        .map_or_else(String::new, |hfd| format!(", HFD = {hfd:.2} px"));
    vec![
        "Equipment Profile = rusty-photon guider".to_owned(),
        format!(
            "Pixel scale = {:.2} arc-sec/px, Binning = {}, Focal length = {:.0} mm",
            common.pixel_scale, common.binning, common.focal_length_mm
        ),
        format!(
            "Search region = {:.0} px, Star mass tolerance = {:.1}%, Multi-star mode, list size = {}",
            common.search_region_px,
            common.mass_tolerance * 100.0,
            common.stars
        ),
        camera,
        format!("Exposure = {} ms", common.exposure.as_millis()),
        mount_line(&common.mount),
        speeds,
        pointing,
        format!(
            "Lock position = {:.3}, {:.3}, Star position = {:.3}, {:.3}{hfd}",
            common.lock.x, common.lock.y, common.star.x, common.star.y
        ),
    ]
}

/// A pulse's duration in milliseconds, its direction letter and the
/// distance it covers, signed as PHD2 signs it: West and South positive.
fn pulse_fields(pulse: Option<Pulse>, rate_px_per_s: f64) -> (u128, &'static str, f64) {
    let Some(pulse) = pulse else {
        return (0, "", 0.0);
    };
    let (letter, sign) = match pulse.direction {
        Direction::West => ("W", 1.0),
        Direction::East => ("E", -1.0),
        Direction::South => ("S", 1.0),
        Direction::North => ("N", -1.0),
    };
    let ms = pulse.duration.as_millis();
    if ms == 0 {
        return (0, "", 0.0);
    }
    (
        ms,
        letter,
        sign * pulse.duration.as_secs_f64() * rate_px_per_s,
    )
}

/// `value`, with a negative zero made positive so that it is written
/// `0.000`.
fn unsigned_zero(value: f64) -> f64 {
    value + 0.0
}

/// An angle the way the stars move as PHD2 gives it, the way the mount
/// moved: turned half a circle, from -180° up to 180°.
fn mount_angle(stars_deg: f64) -> f64 {
    (stars_deg + 360.0).rem_euclid(360.0) - 180.0
}

/// The first local noon after `now`.
fn next_noon(now: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    let noon = NaiveTime::from_hms_opt(12, 0, 0).unwrap_or_default();
    let day = if now.time() < noon {
        now.date_naive()
    } else {
        now.date_naive()
            .checked_add_days(Days::new(1))
            .unwrap_or_else(|| now.date_naive())
    };
    day.and_time(noon)
        .and_local_timezone(*now.offset())
        .single()
        .unwrap_or(now)
}

/// `YYYY-MM-DD HH:MM:SS`.
fn format_time(time: DateTime<FixedOffset>) -> String {
    time.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The platform, as PHD2 names its own.
const fn platform() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "OSX"
    } else {
        "Linux"
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).unwrap()
    }

    #[test]
    fn a_night_turns_at_the_first_local_noon_after_its_file_opened() {
        assert_eq!(
            next_noon(at("2026-11-25T21:00:00+01:00")),
            at("2026-11-26T12:00:00+01:00")
        );
        assert_eq!(
            next_noon(at("2026-11-26T03:00:00+01:00")),
            at("2026-11-26T12:00:00+01:00")
        );
        assert_eq!(
            next_noon(at("2026-11-26T12:00:00+01:00")),
            at("2026-11-27T12:00:00+01:00")
        );
    }

    #[test]
    fn an_angle_the_stars_moved_is_given_the_way_the_mount_moved() {
        assert_eq!(mount_angle(0.0), -180.0);
        assert_eq!(mount_angle(270.0), 90.0);
        assert_eq!(mount_angle(190.0), 10.0);
    }

    #[test]
    fn the_hour_angle_is_the_sidereal_time_less_the_right_ascension() {
        let pointing = |ra_hours, sidereal_hours| Pointing {
            ra_hours,
            altitude_deg: 0.0,
            azimuth_deg: 0.0,
            sidereal_hours,
        };
        assert_eq!(pointing(6.0, 7.5).hour_angle(), 1.5);
        assert_eq!(pointing(23.0, 1.0).hour_angle(), 2.0);
        assert_eq!(pointing(1.0, 23.0).hour_angle(), -2.0);
    }

    #[test]
    fn a_pulse_covers_its_distance_signed_west_and_south_positive() {
        let pulse = |direction| {
            Some(Pulse {
                direction,
                duration: Duration::from_millis(500),
            })
        };
        assert_eq!(pulse_fields(pulse(Direction::West), 4.0), (500, "W", 2.0));
        assert_eq!(pulse_fields(pulse(Direction::East), 4.0), (500, "E", -2.0));
        assert_eq!(pulse_fields(pulse(Direction::South), 4.0), (500, "S", 2.0));
        assert_eq!(pulse_fields(pulse(Direction::North), 4.0), (500, "N", -2.0));
        assert_eq!(pulse_fields(None, 4.0), (0, "", 0.0));
    }

    #[test]
    fn an_offset_along_the_axes_lies_on_the_sensor_where_the_controller_reads_it() {
        let calibration = Calibration {
            setup: rp_guiding::Setup {
                pier_side: PierSide::East,
                camera: rp_guiding::CameraGeometry {
                    pixel_size_um: 2.0,
                    width: 320,
                    height: 240,
                    binning: 1,
                },
                focal_length_mm: 200.0,
                guide_rates: None,
            },
            declination_deg: 0.0,
            ra: rp_guiding::Axis {
                angle_deg: 30.0,
                rate_px_per_s: 2.0,
                steps: 4,
                step: Duration::from_secs(1),
            },
            dec: Some(rp_guiding::DecAxis {
                axis: rp_guiding::Axis {
                    angle_deg: 120.0,
                    rate_px_per_s: 2.0,
                    steps: 4,
                    step: Duration::from_secs(1),
                },
                backlash: Duration::ZERO,
            }),
            warnings: Vec::new(),
        };
        let along = AxisOffset { ra: 2.0, dec: -1.0 };
        let on_sensor = to_sensor(&calibration, along);
        let controller = rp_guiding::Controller::new(
            &calibration,
            0.0,
            rp_guiding::ControlParams::default(),
            Offset::default(),
        );
        let read = controller.error(on_sensor);
        assert!(
            (read.ra - 2.0).abs() < 1e-9 && (read.dec + 1.0).abs() < 1e-9,
            "{read:?}"
        );
    }

    #[test]
    fn a_negative_zero_is_written_as_zero() {
        assert_eq!(format!("{:.3}", unsigned_zero(-0.0)), "0.000");
        assert_eq!(format!("{:.3}", unsigned_zero(-0.25)), "-0.250");
    }
}

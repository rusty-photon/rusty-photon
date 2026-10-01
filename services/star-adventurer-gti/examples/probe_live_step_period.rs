//! Live step-period probe: does the mount's motor board apply a `:I`
//! (set step period) sent to an RA axis that is already running in
//! tracking mode, and how fast?
//!
//! `PulseGuide` East/West changes only the RA step period of a tracking
//! axis. Sending that `:I` on the running motor, instead of stopping the
//! motor to re-send `:G` / `:I` / `:J`, depends on firmware behaviour
//! that neither the Sky-Watcher command set nor the in-repo mock can
//! settle: the mock applies a live `:I` by construction. This tool
//! measures it on the real board as motion, from dense `:j1` encoder
//! samples, because `:f` never reports the step period.
//!
//! Experiments, in order:
//!
//! * **E1 rate edges** — sidereal, then a live `:I1` at 0.5× or 1.5×
//!   sidereal, then a live `:I1` back to sidereal. Each edge is checked
//!   for its ack, whether the new rate took, its latency, and the net
//!   angle the shifted interval delivered against the commanded one.
//! * **E3 `:J1` on a running axis** — the reply, and any position step.
//! * **E3b live `:I1` + `:J1` re-latch** — the same edges with a `:J1`
//!   after each `:I1`.
//! * **E4a `:I1` on a stopped axis** and **E4b `:I1` during the
//!   deceleration after a `:K1`** — both must leave the axis stopped.
//!   E4b also times the stop from sidereal.
//!
//! Only RA is ever commanded to move, only in tracking / slow / CW, and
//! only by the commands in [`Op`]. Every exit path — a finished run, an
//! error, `SIGINT`, `SIGTERM` — ends with `:K1` and `:K2` and waits for
//! `:f1` to report the axis stopped, escalating to `:L1`.
//!
//! This is an operator-run bench tool, never part of the service: the
//! `rusty-photon-star-adventurer-gti` service must be stopped first so
//! the probe has the port to itself. Run it on the rig with the mount
//! parked and the operator present:
//!
//! ```text
//! cargo run --release -p star-adventurer-gti --example probe_live_step_period -- \
//!     --port /dev/serial/by-id/usb-STMicroelectronics_STM32_Virtual_ComPort_<id>-if00 \
//!     --out ~/probe-1299
//! ```
//!
//! A logic-only dry run against the in-repo mock (circular by
//! construction — it proves the tool, not the firmware):
//!
//! ```text
//! cargo run -p star-adventurer-gti --features mock --example probe_live_step_period -- \
//!     --mock --out /tmp/probe-mock
//! ```
//!
//! The pass criteria are fixed in [`Thresholds`] and printed with the
//! verdict. Every exchange is written to `<out>/exchanges.jsonl` and the
//! per-trial analysis to `<out>/summary.json` for re-analysis.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rusty_photon_shared_transport::{FrameTransport, TransportFactory};
use serde::Serialize;
use skywatcher_motor_protocol::codec::decode_u24;
use skywatcher_motor_protocol::{
    Axis, AxisStatus, Command, Direction, ModeKind, MotionMode, Response, Speed,
};
use star_adventurer_gti::coordinates::{pulse_guide_step_period, sidereal_step_period};
use star_adventurer_gti::units::{Cpr, RaTicks};
use star_adventurer_gti::{SerialTransportFactory, UsbConfig};
use tokio::time::Instant;

/// `:e1` reply of the Star Adventurer `GTi` this probe was written for
/// (motor-controller firmware 3.48, mount code `0x0C`).
const EXPECTED_VERSION_REPLY: &str = "=03300C";
const EXPECTED_CPR_RA: u32 = 3_628_800;
const EXPECTED_CPR_DEC: u32 = 2_903_040;
const EXPECTED_TMR_FREQ: u32 = 16_000_000;

/// Time between `:j1` encoder samples.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(4);
/// Time between `:f1` status samples inside a sampling window.
const STATUS_INTERVAL_S: f64 = 0.05;
/// Time the axis must track at a steady rate before a trial's first edge.
const SETTLE_S: f64 = 1.5;
/// Per-request read / write timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest a stop may take before the probe escalates to `:L1`.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Guard band around the counterweight exclusion zone the whole run's
/// RA travel must stay outside of, in hours of mechanical HA.
const ZONE_MARGIN_H: f64 = 0.5;
/// Fastest rate any experiment runs at, as a multiple of sidereal.
const MAX_RATE_FACTOR: f64 = 1.5;
/// The two guide rates E1 alternates between.
const EDGE_FACTORS: [f64; 2] = [0.5, 1.5];
/// Waits after `:K1` before E4b's late `:I1`, cycled across trials.
const LATE_I_DELAYS: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(30),
    Duration::from_millis(40),
    Duration::from_millis(60),
];
/// Rate of the return leg, as a multiple of sidereal (slow mode).
const RETURN_RATE_FACTOR: f64 = 8.0;
/// Ticks short of the start at which the return leg sends `:K1`, so the
/// deceleration lands on the start rather than past it.
const RETURN_LEAD_TICKS: i32 = 8;
/// One RA encoder tick expressed in seconds of RA (`86_400 s / CPR`).
const RA_SECONDS_PER_TICK: f64 = 86_400.0 / 3_628_800.0;

#[derive(Parser, Debug)]
#[command(about = "Measure whether a live :I1 on a tracking RA axis is applied (operator-run)")]
struct Args {
    /// Serial port of the mount (use the /dev/serial/by-id path).
    #[arg(long, default_value = "")]
    port: String,
    /// Baud rate.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    /// Output directory for exchanges.jsonl and summary.json.
    #[arg(long)]
    out: PathBuf,
    /// E1 trials per guide rate (each trial = one shift edge + one restore edge).
    #[arg(long, default_value_t = 60)]
    edge_trials: u32,
    /// E3 and E3b trials.
    #[arg(long, default_value_t = 20)]
    relatch_trials: u32,
    /// E4a trials.
    #[arg(long, default_value_t = 8)]
    stopped_trials: u32,
    /// E4b trials.
    #[arg(long, default_value_t = 12)]
    decel_trials: u32,
    /// E5 trials per guide rate: the same pulse built from a stop/start
    /// with no fixed wait (`:K1`, `:f1` until stopped, `:G110 :I1 :J1`).
    #[arg(long, default_value_t = 0)]
    stop_start_trials: u32,
    /// Length of each sampling segment around an edge, in seconds.
    #[arg(long, default_value_t = 1.0)]
    segment: f64,
    /// Counterweight exclusion zone, low edge (hours of mechanical HA).
    #[arg(long, default_value_t = 0.95)]
    zone_min: f64,
    /// Counterweight exclusion zone, high edge (hours of mechanical HA).
    #[arg(long, default_value_t = 11.05)]
    zone_max: f64,
    /// Run against the in-repo mock instead of a serial port (needs `--features mock`).
    #[arg(long)]
    mock: bool,
}

#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("mount replied with an error to {cmd}: {reply}")]
    Mount { cmd: String, reply: String },
    #[error("precondition failed: {0}")]
    Precondition(String),
    #[error("unexpected reply to {cmd}: {reply}")]
    Unexpected { cmd: String, reply: String },
    #[error("aborted by signal")]
    Aborted,
    #[error("output: {0}")]
    Output(String),
}

/// The only commands the probe can put on the wire. Motion is confined
/// to the RA axis in tracking / slow / CW, at a step period inside the
/// range [`Link::allow_periods`] opens after the identity checks.
#[derive(Debug, Clone, Copy)]
enum Op {
    Version,
    Cpr(Axis),
    TmrFreq,
    Position(Axis),
    Status(Axis),
    /// `:i1`, inquire the RA step period. Not every board implements it.
    PeriodReadback,
    /// `:G110`, RA tracking / slow / CW.
    Track,
    /// `:G111`, RA tracking / slow / CCW — only for the return leg.
    TrackReverse,
    /// `:I1<period>` on RA.
    Period(u32),
    /// `:J1`.
    Start,
    /// `:K<axis>`.
    Stop(Axis),
    /// `:L<axis>`.
    Halt(Axis),
}

impl Op {
    const fn command(self) -> Option<Command> {
        Some(match self {
            Self::Version => Command::InquireMotorBoardVersion(Axis::Ra),
            Self::Cpr(axis) => Command::InquireCpr(axis),
            Self::TmrFreq => Command::InquireTmrFreq,
            Self::Position(axis) => Command::InquirePosition(axis),
            Self::Status(axis) => Command::InquireStatus(axis),
            Self::PeriodReadback => return None,
            Self::Track => Command::SetMotionMode {
                axis: Axis::Ra,
                mode: MotionMode::TRACKING,
            },
            Self::TrackReverse => Command::SetMotionMode {
                axis: Axis::Ra,
                mode: MotionMode {
                    ccw: true,
                    ..MotionMode::TRACKING
                },
            },
            Self::Period(period) => Command::SetStepPeriod {
                axis: Axis::Ra,
                period,
            },
            Self::Start => Command::StartMotion(Axis::Ra),
            Self::Stop(axis) => Command::StopMotion(axis),
            Self::Halt(axis) => Command::InstantStop(axis),
        })
    }

    /// Whether the op may run after an abort: stop-class and read-only.
    const fn stop_class(self) -> bool {
        matches!(
            self,
            Self::Stop(_) | Self::Halt(_) | Self::Status(_) | Self::Position(_)
        )
    }
}

/// One request/reply round trip, timed against the probe's epoch.
#[derive(Debug, Clone)]
struct Exchange {
    t_send: f64,
    t_recv: f64,
    reply: Result<Response, String>,
    raw: String,
}

impl Exchange {
    const fn mid(&self) -> f64 {
        f64::midpoint(self.t_send, self.t_recv)
    }

    const fn acked(&self) -> bool {
        matches!(self.reply, Ok(Response::Ack))
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    t: f64,
    ticks: i32,
}

fn tracking_cw(status: AxisStatus) -> bool {
    status.motion.running
        && !status.motion.blocked
        && status.mode == ModeKind::Tracking
        && status.speed == Speed::Slow
        && status.direction == Direction::Cw
}

struct Link {
    transport: Box<dyn FrameTransport>,
    epoch: Instant,
    log: BufWriter<File>,
    tag: String,
    period_range: (u32, u32),
    rtts: Vec<f64>,
    abort: Arc<AtomicBool>,
}

impl Link {
    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    fn set_tag(&mut self, tag: impl Into<String>) {
        self.tag = tag.into();
    }

    const fn allow_periods(&mut self, lo: u32, hi: u32) {
        self.period_range = (lo, hi);
    }

    async fn ask(&mut self, op: Op) -> Result<Exchange, ProbeError> {
        if self.abort.load(Ordering::SeqCst) && !op.stop_class() {
            return Err(ProbeError::Aborted);
        }
        if let Op::Period(period) = op {
            let (lo, hi) = self.period_range;
            if period < lo || period > hi {
                return Err(ProbeError::Precondition(format!(
                    "step period {period} outside the allowed range {lo}..={hi}"
                )));
            }
        }
        let command = op.command();
        let frame = match &command {
            Some(cmd) => cmd
                .encode()
                .map_err(|e| ProbeError::Transport(e.to_string()))?,
            None => b":i1\r".to_vec(),
        };
        let cmd_text = String::from_utf8_lossy(&frame).trim_end().to_string();
        let t_send = self.now();
        let io = async {
            self.transport.send_frame(&frame).await?;
            let mut buf = Vec::new();
            self.transport.recv_frame(&mut buf).await?;
            Ok::<_, rusty_photon_shared_transport::TransportError>(buf)
        }
        .await;
        let t_recv = self.now();
        let buf = io.map_err(|e| {
            self.log_line(
                &cmd_text,
                t_send,
                t_recv,
                &format!("<transport error: {e}>"),
            );
            ProbeError::Transport(format!("{cmd_text}: {e}"))
        })?;
        let raw = String::from_utf8_lossy(&buf).trim_end().to_string();
        let reply = command.as_ref().map_or_else(
            || {
                parse_u24_reply(&raw)
                    .map(Response::U24)
                    .ok_or_else(|| raw.clone())
            },
            |cmd| Response::decode(&buf, cmd).map_err(|e| e.to_string()),
        );
        self.log_line(&cmd_text, t_send, t_recv, &raw);
        self.rtts.push(t_recv - t_send);
        Ok(Exchange {
            t_send,
            t_recv,
            reply,
            raw,
        })
    }

    /// [`Self::ask`], treating an error reply as a failure.
    async fn ask_ok(&mut self, op: Op) -> Result<Exchange, ProbeError> {
        let ex = self.ask(op).await?;
        if let Err(reply) = &ex.reply {
            return Err(ProbeError::Mount {
                cmd: format!("{op:?}"),
                reply: format!("{} ({reply})", ex.raw),
            });
        }
        Ok(ex)
    }

    fn log_line(&mut self, cmd: &str, t_send: f64, t_recv: f64, reply: &str) {
        let line = serde_json::json!({
            "t": t_send,
            "rtt": t_recv - t_send,
            "tag": self.tag,
            "cmd": cmd,
            "reply": reply,
        });
        // A lost log line must not abort a run mid-motion; the summary
        // records the analysis either way.
        let _ = writeln!(self.log, "{line}");
    }

    async fn u24(&mut self, op: Op) -> Result<u32, ProbeError> {
        let ex = self.ask_ok(op).await?;
        match ex.reply {
            Ok(Response::U24(v)) => Ok(v),
            _ => Err(unexpected(op, &ex)),
        }
    }

    async fn position(&mut self, axis: Axis) -> Result<(f64, i32), ProbeError> {
        let op = Op::Position(axis);
        let ex = self.ask_ok(op).await?;
        match ex.reply {
            Ok(Response::Position(ticks)) => Ok((ex.mid(), ticks)),
            _ => Err(unexpected(op, &ex)),
        }
    }

    async fn status(&mut self, axis: Axis) -> Result<(f64, AxisStatus), ProbeError> {
        let op = Op::Status(axis);
        let ex = self.ask_ok(op).await?;
        match ex.reply {
            Ok(Response::Status(status)) => Ok((ex.mid(), status)),
            _ => Err(unexpected(op, &ex)),
        }
    }

    /// Sample `:j1` every [`SAMPLE_INTERVAL`] (and `:f1` every
    /// [`STATUS_INTERVAL_S`]) for `seconds`.
    async fn sample(
        &mut self,
        seconds: f64,
        samples: &mut Vec<Sample>,
        statuses: &mut Vec<AxisStatus>,
    ) -> Result<(), ProbeError> {
        let end =
            deadline(Duration::try_from_secs_f64(seconds).map_err(|e| {
                ProbeError::Precondition(format!("sampling window {seconds} s: {e}"))
            })?)?;
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_status = f64::NEG_INFINITY;
        while Instant::now() < end {
            ticker.tick().await;
            let (t, ticks) = self.position(Axis::Ra).await?;
            samples.push(Sample { t, ticks });
            if t - last_status >= STATUS_INTERVAL_S {
                let (ts, status) = self.status(Axis::Ra).await?;
                statuses.push(status);
                last_status = ts;
            }
        }
        Ok(())
    }

    /// Poll `:f1` tightly until the RA axis reports stopped; returns the
    /// time the first stopped status was read, or `None` on timeout.
    async fn wait_stopped(&mut self, timeout: Duration) -> Result<Option<f64>, ProbeError> {
        let end = deadline(timeout)?;
        while Instant::now() < end {
            let (t, status) = self.status(Axis::Ra).await?;
            if !status.motion.running {
                return Ok(Some(t));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(None)
    }
}

fn deadline(after: Duration) -> Result<Instant, ProbeError> {
    Instant::now()
        .checked_add(after)
        .ok_or_else(|| ProbeError::Precondition(format!("deadline {after:?} overflows")))
}

fn unexpected(op: Op, ex: &Exchange) -> ProbeError {
    ProbeError::Unexpected {
        cmd: format!("{op:?}"),
        reply: ex.raw.clone(),
    }
}

/// Encoder rate in ticks per second for a step period.
fn rate_of(tmr_freq: u32, period: u32) -> f64 {
    f64::from(tmr_freq) / f64::from(period)
}

/// Mean of `ticks − rate·t` over the samples in `[from, to]`: the
/// intercept of a line of known slope through them.
fn offset_at(samples: &[Sample], from: f64, to: f64, rate: f64) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0.0;
    for s in samples.iter().filter(|s| s.t >= from && s.t <= to) {
        sum += rate.mul_add(-s.t, f64::from(s.ticks));
        n += 1.0;
    }
    (n >= 20.0).then(|| sum / n)
}

/// Least-squares slope (ticks per second) over the samples in `[from, to]`.
fn slope_in(samples: &[Sample], from: f64, to: f64) -> Option<f64> {
    let window: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.t >= from && s.t <= to)
        .collect();
    let n = f64::from(u32::try_from(window.len()).ok()?);
    if n < 20.0 {
        return None;
    }
    let mean_t = window.iter().map(|s| s.t).sum::<f64>() / n;
    let mean_y = window.iter().map(|s| f64::from(s.ticks)).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for s in &window {
        let dt = s.t - mean_t;
        sxy = dt.mul_add(f64::from(s.ticks) - mean_y, sxy);
        sxx = dt.mul_add(dt, sxx);
    }
    (sxx > 0.0).then(|| sxy / sxx)
}

fn within(value: Option<f64>, target: f64, tolerance: f64) -> bool {
    value.is_some_and(|v| ((v - target) / target).abs() <= tolerance)
}

/// The instant two lines of known slope cross, relative to `t_ref`.
fn crossing(a0: Option<f64>, r0: f64, a1: Option<f64>, r1: f64, t_ref: f64) -> Option<f64> {
    Some((a1? - a0?) / (r0 - r1) - t_ref)
}

/// Pre-registered pass criteria. Posted on the issue before the rig run.
#[derive(Debug, Clone, Copy, Serialize)]
struct Thresholds {
    /// A measured rate must be within this fraction of the commanded one.
    rate_tolerance: f64,
    /// An edge's latency may exceed one outgoing step period by this much
    /// (seconds) for the 99 % criterion.
    latency_slack_s: f64,
    /// No edge's latency may exceed this (seconds), or fall below its negation / 10.
    latency_max_s: f64,
    /// |net pulse error| for the 99 % criterion (ticks).
    error_p99_ticks: f64,
    /// |net pulse error| no trial may exceed (ticks).
    error_max_ticks: f64,
    /// Stationary tolerance for a stopped axis (ticks, peak-to-peak).
    stationary_ticks: f64,
    /// Option C gate: mean and max |position step| of a `:J1` on a running axis (ticks).
    relatch_step_mean_ticks: f64,
    relatch_step_max_ticks: f64,
}

const THRESHOLDS: Thresholds = Thresholds {
    rate_tolerance: 0.03,
    latency_slack_s: 0.010,
    latency_max_s: 0.100,
    error_p99_ticks: 1.0,
    error_max_ticks: 2.0,
    stationary_ticks: 1.0,
    relatch_step_mean_ticks: 0.3,
    relatch_step_max_ticks: 1.0,
};

/// Whether each edge's commanded rate took, judged from the fitted
/// encoder slope after it.
#[derive(Debug, Clone, Copy, Serialize)]
struct Applied {
    shift: bool,
    restore: bool,
}

/// One E1 / E3b trial: sidereal → shifted → sidereal.
#[derive(Debug, Clone, Serialize)]
struct EdgeTrial {
    experiment: &'static str,
    factor: f64,
    t_shift: f64,
    t_restore: f64,
    acked: bool,
    relatch_acked: Option<bool>,
    rate_pre: Option<f64>,
    rate_during: Option<f64>,
    rate_post: Option<f64>,
    applied: Applied,
    /// Seconds from the shift `:I1` (exchange midpoint) to the rate change.
    latency_shift_s: Option<f64>,
    /// Seconds from the restore `:I1` to the rate change.
    latency_restore_s: Option<f64>,
    /// Outgoing step period at each edge, for the latency criterion.
    step_before_shift_s: f64,
    step_before_restore_s: f64,
    /// Delivered minus commanded excess over sidereal, in ticks.
    error_ticks: Option<f64>,
    status_ok: bool,
}

/// One E3 trial: `:J1` on an axis already tracking at sidereal.
#[derive(Debug, Clone, Serialize)]
struct RelatchTrial {
    reply: String,
    rate_post: Option<f64>,
    step_ticks: Option<f64>,
    status_ok: bool,
}

/// One E5 trial: a pulse built from a stop/start with no fixed wait.
#[derive(Debug, Clone, Serialize)]
struct StopStartTrial {
    factor: f64,
    /// `:K1` send to `:J1` exchange midpoint, at the pulse start and at
    /// the restore.
    stopped_start_s: f64,
    stopped_restore_s: f64,
    /// Delivered minus commanded excess over sidereal, in ticks, with the
    /// shifted interval timed from the start `:J1` to the restore `:K1`
    /// (the driver times a pulse from the motion it started).
    error_ticks: Option<f64>,
}

/// One E4 trial: an `:I1` the axis must ignore.
#[derive(Debug, Clone, Serialize)]
struct InertTrial {
    experiment: &'static str,
    delay_s: Option<f64>,
    reply: String,
    /// `:K1` reply to the first stopped `:f1`.
    stop_time_s: Option<f64>,
    /// Encoder travel from the `:K1` to the first stopped `:f1`.
    coast_ticks: Option<i32>,
    /// Peak-to-peak encoder travel after the axis reported stopped.
    stationary_span_ticks: Option<i32>,
    stayed_stopped: bool,
}

#[derive(Debug, Serialize)]
struct Identity {
    version_reply: String,
    cpr_ra: u32,
    cpr_dec: u32,
    tmr_freq: u32,
    ra_ticks: i32,
    dec_ticks: i32,
    mech_ha_h: f64,
    period_readback: Option<u32>,
}

#[derive(Debug, Serialize, Default)]
struct Summary {
    identity: Option<Identity>,
    thresholds: Option<Thresholds>,
    sidereal_period: u32,
    edges: Vec<EdgeTrial>,
    relatch: Vec<RelatchTrial>,
    relatch_edges: Vec<EdgeTrial>,
    inert: Vec<InertTrial>,
    stop_start: Vec<StopStartTrial>,
    rtt_p50_s: Option<f64>,
    rtt_p99_s: Option<f64>,
    rtt_max_s: Option<f64>,
    run_error: Option<String>,
    return_report: String,
    stop_report: String,
    verdict: Vec<String>,
}

/// Check identity, geometry and state before anything moves.
async fn preflight(link: &mut Link, args: &Args) -> Result<Identity, ProbeError> {
    link.set_tag("preflight");
    let version = link.ask_ok(Op::Version).await?;
    if version.raw != EXPECTED_VERSION_REPLY {
        return Err(ProbeError::Precondition(format!(
            ":e1 replied {:?}, expected {EXPECTED_VERSION_REPLY:?} (Star Adventurer GTi, fw 3.48)",
            version.raw
        )));
    }
    let cpr_ra = link.u24(Op::Cpr(Axis::Ra)).await?;
    let cpr_dec = link.u24(Op::Cpr(Axis::Dec)).await?;
    let tmr_freq = link.u24(Op::TmrFreq).await?;
    if (cpr_ra, cpr_dec, tmr_freq) != (EXPECTED_CPR_RA, EXPECTED_CPR_DEC, EXPECTED_TMR_FREQ) {
        return Err(ProbeError::Precondition(format!(
            "geometry {cpr_ra}/{cpr_dec}/{tmr_freq} is not the GTi's \
             {EXPECTED_CPR_RA}/{EXPECTED_CPR_DEC}/{EXPECTED_TMR_FREQ}"
        )));
    }
    for axis in [Axis::Ra, Axis::Dec] {
        let (_, status) = link.status(axis).await?;
        if !status.init.initialized || status.motion.running {
            return Err(ProbeError::Precondition(format!(
                "{axis:?} must be initialised and stopped, :f reads {status:?}"
            )));
        }
    }
    let (_, ra_ticks) = link.position(Axis::Ra).await?;
    let (_, dec_ticks) = link.position(Axis::Dec).await?;
    let mech_ha_h = RaTicks::new(ra_ticks).to_mech_ha(Cpr::new(cpr_ra)).value();
    let travel_h = planned_seconds(args) * MAX_RATE_FACTOR / 3600.0;
    let (lo, hi) = (args.zone_min - ZONE_MARGIN_H, args.zone_max + ZONE_MARGIN_H);
    if (mech_ha_h + travel_h >= lo && mech_ha_h <= hi) || mech_ha_h + travel_h >= 12.0 {
        return Err(ProbeError::Precondition(format!(
            "RA travel mech HA {mech_ha_h:.3} h .. {:.3} h would come within \
             {ZONE_MARGIN_H} h of the CW exclusion zone ({}, {})",
            mech_ha_h + travel_h,
            args.zone_min,
            args.zone_max
        )));
    }
    let period_readback = match link.ask(Op::PeriodReadback).await?.reply {
        Ok(Response::U24(period)) => Some(period),
        _ => None,
    };
    Ok(Identity {
        version_reply: version.raw,
        cpr_ra,
        cpr_dec,
        tmr_freq,
        ra_ticks,
        dec_ticks,
        mech_ha_h,
        period_readback,
    })
}

fn parse_u24_reply(raw: &str) -> Option<u32> {
    let payload: [u8; 6] = raw.strip_prefix('=')?.as_bytes().try_into().ok()?;
    decode_u24(&payload).ok()
}

/// Upper bound on the run's motion time, for the zone check.
fn planned_seconds(args: &Args) -> f64 {
    let seg = args.segment;
    let edge = f64::from(args.edge_trials) * 2.0 * 3.0 * seg;
    let relatch = f64::from(args.relatch_trials) * 5.0 * seg;
    let stop_start = f64::from(args.stop_start_trials) * 2.0 * 3.0 * seg;
    let inert =
        f64::from(args.stopped_trials.saturating_add(args.decel_trials)) * (SETTLE_S + seg + 1.0);
    SETTLE_S + edge + relatch + inert + stop_start + 60.0
}

/// `:K1`, wait for stopped, then `:G110 :I1<sidereal> :J1` and settle.
async fn start_sidereal(link: &mut Link, sidereal: u32) -> Result<(), ProbeError> {
    link.ask_ok(Op::Stop(Axis::Ra)).await?;
    if link.wait_stopped(STOP_TIMEOUT).await?.is_none() {
        return Err(ProbeError::Precondition(
            "RA did not stop before a tracking start".into(),
        ));
    }
    link.ask_ok(Op::Track).await?;
    link.ask_ok(Op::Period(sidereal)).await?;
    link.ask_ok(Op::Start).await?;
    let (mut s, mut st) = (Vec::new(), Vec::new());
    link.sample(SETTLE_S, &mut s, &mut st).await
}

struct EdgeContext {
    tmr_freq: u32,
    sidereal: u32,
    segment: f64,
    relatch: bool,
}

async fn edge_trial(
    link: &mut Link,
    ctx: &EdgeContext,
    factor: f64,
) -> Result<EdgeTrial, ProbeError> {
    let shifted = pulse_guide_step_period(ctx.sidereal, factor);
    let (r0, r1) = (
        rate_of(ctx.tmr_freq, ctx.sidereal),
        rate_of(ctx.tmr_freq, shifted),
    );
    let (mut s, mut st) = (Vec::new(), Vec::new());
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let shift = link.ask(Op::Period(shifted)).await?;
    let mut relatch_acked = if ctx.relatch {
        Some(link.ask(Op::Start).await?.acked())
    } else {
        None
    };
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let restore = link.ask(Op::Period(ctx.sidereal)).await?;
    if ctx.relatch {
        let relatched = link.ask(Op::Start).await?.acked();
        relatch_acked = relatch_acked.map(|a| a && relatched);
    }
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let (ta, tb) = (shift.mid(), restore.mid());
    let rate_pre = slope_in(&s, ta - ctx.segment, ta - 0.02);
    let rate_during = slope_in(&s, ta + 0.15, tb - 0.02);
    let rate_post = slope_in(&s, tb + 0.15, tb + ctx.segment);
    let a_pre = offset_at(&s, ta - ctx.segment, ta - 0.02, r0);
    let a_during = offset_at(&s, ta + 0.15, tb - 0.02, r1);
    let a_post = offset_at(&s, tb + 0.15, tb + ctx.segment, r0);
    let expected_excess = (r1 - r0) * (tb - ta);
    let error_ticks = a_post
        .zip(a_pre)
        .map(|(post, pre)| post - pre - expected_excess);
    let status_ok = st.iter().copied().all(tracking_cw);
    Ok(EdgeTrial {
        experiment: if ctx.relatch { "E3b" } else { "E1" },
        factor,
        t_shift: ta,
        t_restore: tb,
        acked: shift.acked() && restore.acked(),
        relatch_acked,
        rate_pre,
        rate_during,
        rate_post,
        applied: Applied {
            shift: within(rate_during, r1, THRESHOLDS.rate_tolerance),
            restore: within(rate_post, r0, THRESHOLDS.rate_tolerance),
        },
        latency_shift_s: crossing(a_pre, r0, a_during, r1, ta),
        latency_restore_s: crossing(a_during, r1, a_post, r0, tb),
        step_before_shift_s: 1.0 / r0,
        step_before_restore_s: 1.0 / r1,
        error_ticks,
        status_ok,
    })
}

async fn relatch_trial(link: &mut Link, ctx: &EdgeContext) -> Result<RelatchTrial, ProbeError> {
    let r0 = rate_of(ctx.tmr_freq, ctx.sidereal);
    let (mut s, mut st) = (Vec::new(), Vec::new());
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let relatch = link.ask(Op::Start).await?;
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let t = relatch.mid();
    let pre = offset_at(&s, t - ctx.segment, t - 0.02, r0);
    let post = offset_at(&s, t + 0.1, t + ctx.segment, r0);
    Ok(RelatchTrial {
        reply: relatch.raw,
        rate_post: slope_in(&s, t + 0.1, t + ctx.segment),
        step_ticks: post.zip(pre).map(|(b, a)| b - a),
        status_ok: st.iter().copied().all(tracking_cw),
    })
}

/// `:K1`, `:f1` until stopped (no fixed wait), then `:G110 :I1 :J1`.
/// Returns the `:K1` send time and the `:J1` exchange midpoint.
async fn stop_start_to(link: &mut Link, period: u32) -> Result<(f64, f64), ProbeError> {
    let stop = link.ask_ok(Op::Stop(Axis::Ra)).await?;
    if link.wait_stopped(STOP_TIMEOUT).await?.is_none() {
        return Err(ProbeError::Precondition(
            "RA did not stop in a stop/start".into(),
        ));
    }
    link.ask_ok(Op::Track).await?;
    link.ask_ok(Op::Period(period)).await?;
    let start = link.ask_ok(Op::Start).await?;
    Ok((stop.t_send, start.mid()))
}

async fn stop_start_trial(
    link: &mut Link,
    ctx: &EdgeContext,
    factor: f64,
) -> Result<StopStartTrial, ProbeError> {
    let shifted = pulse_guide_step_period(ctx.sidereal, factor);
    let (r0, r1) = (
        rate_of(ctx.tmr_freq, ctx.sidereal),
        rate_of(ctx.tmr_freq, shifted),
    );
    let (mut s, mut st) = (Vec::new(), Vec::new());
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let (k1, j1) = stop_start_to(link, shifted).await?;
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let (k2, j2) = stop_start_to(link, ctx.sidereal).await?;
    link.sample(ctx.segment, &mut s, &mut st).await?;
    let pre = offset_at(&s, k1 - ctx.segment, k1 - 0.02, r0);
    let post = offset_at(&s, j2 + 0.15, j2 + ctx.segment, r0);
    let expected = (r1 - r0) * (k2 - j1);
    Ok(StopStartTrial {
        factor,
        stopped_start_s: j1 - k1,
        stopped_restore_s: j2 - k2,
        error_ticks: post.zip(pre).map(|(b, a)| b - a - expected),
    })
}

/// E4: stop the tracking axis, send an `:I1` it must ignore — after
/// the stop completes (`delay = None`) or `delay` after the `:K1` —
/// and check the axis stays put. Leaves the axis tracking at sidereal.
async fn inert_trial(
    link: &mut Link,
    ctx: &EdgeContext,
    delay: Option<Duration>,
    period: u32,
) -> Result<InertTrial, ProbeError> {
    let (_, before) = link.position(Axis::Ra).await?;
    let stop = link.ask_ok(Op::Stop(Axis::Ra)).await?;
    let reply = match delay {
        Some(delay) => {
            tokio::time::sleep(delay).await;
            link.ask(Op::Period(period)).await?.raw
        }
        None => String::new(),
    };
    let stopped_at = link.wait_stopped(STOP_TIMEOUT).await?;
    let (_, at_stop) = link.position(Axis::Ra).await?;
    let reply = if delay.is_none() {
        link.ask(Op::Period(period)).await?.raw
    } else {
        reply
    };
    let (mut after, mut after_st) = (Vec::new(), Vec::new());
    link.sample(ctx.segment, &mut after, &mut after_st).await?;
    let span = after
        .iter()
        .map(|x| x.ticks)
        .max()
        .zip(after.iter().map(|x| x.ticks).min())
        .and_then(|(hi, lo)| hi.checked_sub(lo));
    let stayed_stopped = stopped_at.is_some()
        && after_st.iter().all(|x| !x.motion.running)
        && span.is_some_and(|d| f64::from(d) <= THRESHOLDS.stationary_ticks);
    let trial = InertTrial {
        experiment: if delay.is_some() { "E4b" } else { "E4a" },
        delay_s: delay.map(|d| d.as_secs_f64()),
        reply,
        stop_time_s: stopped_at.map(|t| t - stop.t_recv),
        coast_ticks: at_stop.checked_sub(before),
        stationary_span_ticks: span,
        stayed_stopped,
    };
    start_sidereal(link, ctx.sidereal).await?;
    Ok(trial)
}

async fn run(link: &mut Link, args: &Args, summary: &mut Summary) -> Result<(), ProbeError> {
    let identity = preflight(link, args).await?;
    let sidereal = sidereal_step_period(identity.tmr_freq, Cpr::new(identity.cpr_ra));
    summary.sidereal_period = sidereal;
    let ctx = EdgeContext {
        tmr_freq: identity.tmr_freq,
        sidereal,
        segment: args.segment,
        relatch: false,
    };
    summary.identity = Some(identity);
    link.allow_periods(
        pulse_guide_step_period(sidereal, 1.6),
        pulse_guide_step_period(sidereal, 0.4),
    );
    link.set_tag("start");
    start_sidereal(link, sidereal).await?;

    for i in 0..args.edge_trials.saturating_mul(2) {
        let factor = if i.is_multiple_of(2) {
            EDGE_FACTORS[0]
        } else {
            EDGE_FACTORS[1]
        };
        link.set_tag(format!("E1/{i}"));
        summary.edges.push(edge_trial(link, &ctx, factor).await?);
    }
    for i in 0..args.stop_start_trials.saturating_mul(2) {
        let factor = if i.is_multiple_of(2) {
            EDGE_FACTORS[0]
        } else {
            EDGE_FACTORS[1]
        };
        link.set_tag(format!("E5/{i}"));
        summary
            .stop_start
            .push(stop_start_trial(link, &ctx, factor).await?);
    }
    for i in 0..args.relatch_trials {
        link.set_tag(format!("E3/{i}"));
        summary.relatch.push(relatch_trial(link, &ctx).await?);
    }
    let relatch_ctx = EdgeContext {
        relatch: true,
        ..ctx
    };
    for i in 0..args.relatch_trials {
        let factor = if i.is_multiple_of(2) {
            EDGE_FACTORS[0]
        } else {
            EDGE_FACTORS[1]
        };
        link.set_tag(format!("E3b/{i}"));
        summary
            .relatch_edges
            .push(edge_trial(link, &relatch_ctx, factor).await?);
    }
    for i in 0..args.stopped_trials {
        link.set_tag(format!("E4a/{i}"));
        summary
            .inert
            .push(inert_trial(link, &ctx, None, sidereal).await?);
    }
    for (i, delay) in (0..args.decel_trials).zip(LATE_I_DELAYS.iter().cycle()) {
        let factor = if i.is_multiple_of(2) {
            EDGE_FACTORS[0]
        } else {
            EDGE_FACTORS[1]
        };
        link.set_tag(format!("E4b/{i}"));
        let period = pulse_guide_step_period(sidereal, factor);
        summary
            .inert
            .push(inert_trial(link, &ctx, Some(*delay), period).await?);
    }
    Ok(())
}

/// Drive RA back to the encoder count it started from — tracking mode,
/// slow, CCW, at [`RETURN_RATE_FACTOR`] × sidereal — so the run leaves
/// the mount where it found it (the park pose on the rig).
async fn return_to_start(link: &mut Link, start: i32, sidereal: u32) -> Result<String, ProbeError> {
    link.set_tag("return");
    link.ask_ok(Op::Stop(Axis::Ra)).await?;
    if link.wait_stopped(STOP_TIMEOUT).await?.is_none() {
        return Err(ProbeError::Precondition(
            "RA did not stop before the return leg".into(),
        ));
    }
    let (_, from) = link.position(Axis::Ra).await?;
    let target = start.saturating_add(RETURN_LEAD_TICKS);
    if from <= target {
        return Ok(format!(
            "already at {from} (start {start}); no return needed"
        ));
    }
    let period = pulse_guide_step_period(sidereal, RETURN_RATE_FACTOR);
    let travel_s =
        f64::from(from.saturating_sub(start)) * f64::from(period) / f64::from(EXPECTED_TMR_FREQ);
    let limit = deadline(
        Duration::try_from_secs_f64(travel_s.mul_add(1.5, 5.0))
            .map_err(|e| ProbeError::Precondition(format!("return leg duration: {e}")))?,
    )?;
    link.allow_periods(period, sidereal);
    link.ask_ok(Op::TrackReverse).await?;
    link.ask_ok(Op::Period(period)).await?;
    link.ask_ok(Op::Start).await?;
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    loop {
        ticker.tick().await;
        // The reads below are stop-class and never abort by themselves, so
        // a signal is honoured here: the safety stop that follows ends the
        // leg wherever it has got to.
        if link.abort.load(Ordering::SeqCst) {
            return Err(ProbeError::Aborted);
        }
        let (_, now) = link.position(Axis::Ra).await?;
        if now <= target {
            break;
        }
        if Instant::now() >= limit {
            return Err(ProbeError::Precondition(format!(
                "return leg overran its time budget at {now} (target {target})"
            )));
        }
    }
    link.ask_ok(Op::Stop(Axis::Ra)).await?;
    let stopped = link.wait_stopped(STOP_TIMEOUT).await?;
    let (_, end) = link.position(Axis::Ra).await?;
    Ok(format!(
        "returned {from} -> {end} (start {start}, off by {:?} ticks, stopped {})",
        end.checked_sub(start),
        stopped.is_some()
    ))
}

/// Stop both axes and confirm RA stopped. Runs on every exit path.
async fn safety_stop(link: &mut Link) -> String {
    link.set_tag("safety-stop");
    let mut report = Vec::new();
    for axis in [Axis::Ra, Axis::Dec] {
        match link.ask(Op::Stop(axis)).await {
            Ok(ex) => report.push(format!(":K {axis:?} -> {}", ex.raw)),
            Err(e) => report.push(format!(":K {axis:?} failed: {e}")),
        }
    }
    match link.wait_stopped(STOP_TIMEOUT).await {
        Ok(Some(_)) => report.push("RA stopped".into()),
        other => {
            report.push(format!(
                "RA not confirmed stopped ({other:?}); escalating to :L1"
            ));
            let halt = link.ask(Op::Halt(Axis::Ra)).await;
            report.push(format!(":L1 -> {halt:?}"));
            let confirmed = link.wait_stopped(STOP_TIMEOUT).await;
            report.push(format!("after :L1: {confirmed:?}"));
        }
    }
    report.join("; ")
}

/// The `pct`-th percentile (nearest rank, rounded down) of sorted data.
fn percentile(sorted: &[f64], pct: usize) -> Option<f64> {
    let last = sorted.len().checked_sub(1)?;
    sorted
        .get(last.checked_mul(pct)?.checked_div(100)?)
        .copied()
}

fn share(passing: usize, total: usize) -> f64 {
    let (p, t) = (
        u32::try_from(passing).unwrap_or(0),
        u32::try_from(total).unwrap_or(1),
    );
    if t == 0 {
        return 0.0;
    }
    f64::from(p) / f64::from(t)
}

fn latency_ok(latency: Option<f64>, outgoing_step_s: f64) -> (bool, bool) {
    let t = THRESHOLDS;
    latency.map_or((false, false), |l| {
        let strict = l >= -t.latency_slack_s && l <= outgoing_step_s + t.latency_slack_s;
        let loose = l >= -t.latency_max_s / 10.0 && l <= t.latency_max_s;
        (strict, loose)
    })
}

/// Apply [`THRESHOLDS`] to the trials and name the design the data selects.
fn verdict(summary: &Summary) -> Vec<String> {
    let t = THRESHOLDS;
    let e = &summary.edges;
    let mut out = Vec::new();
    let v1 = !e.is_empty() && e.iter().all(|x| x.acked);
    let v2 = !e.is_empty() && e.iter().all(|x| x.applied.shift && x.applied.restore);
    let lat: Vec<(bool, bool)> = e
        .iter()
        .flat_map(|x| {
            [
                latency_ok(x.latency_shift_s, x.step_before_shift_s),
                latency_ok(x.latency_restore_s, x.step_before_restore_s),
            ]
        })
        .collect();
    let v3 =
        share(lat.iter().filter(|x| x.0).count(), lat.len()) >= 0.99 && lat.iter().all(|x| x.1);
    let errs: Vec<f64> = e
        .iter()
        .map(|x| x.error_ticks.map_or(f64::INFINITY, f64::abs))
        .collect();
    let v4 = share(
        errs.iter().filter(|x| **x <= t.error_p99_ticks).count(),
        errs.len(),
    ) >= 0.99
        && errs.iter().all(|x| *x <= t.error_max_ticks);
    let v5 = e.iter().all(|x| x.status_ok);
    let v6 = !summary.inert.is_empty() && summary.inert.iter().all(|x| x.stayed_stopped);
    for (name, ok) in [
        ("V1 every live :I1 acked", v1),
        ("V2 every edge applied", v2),
        ("V3 edge latency", v3),
        ("V4 net pulse error", v4),
        ("V5 status stays tracking/slow/CW", v5),
        ("V6 stray :I1 inert (E4)", v6),
    ] {
        out.push(format!("{name}: {}", if ok { "PASS" } else { "FAIL" }));
    }
    let steps: Vec<f64> = summary
        .relatch
        .iter()
        .map(|x| x.step_ticks.map_or(f64::INFINITY, f64::abs))
        .collect();
    let step_count = f64::from(u32::try_from(steps.len()).unwrap_or(1).max(1));
    let step_mean = steps.iter().sum::<f64>() / step_count;
    let step_max = steps.iter().copied().fold(0.0, f64::max);
    let relatch_ok = !summary.relatch_edges.is_empty()
        && summary
            .relatch_edges
            .iter()
            .all(|x| x.applied.shift && x.applied.restore)
        && step_mean <= t.relatch_step_mean_ticks
        && step_max <= t.relatch_step_max_ticks;
    out.push(format!(
        "E3 :J1 on running: mean |step| {step_mean:.2} ticks, max {step_max:.2}"
    ));
    let design = if v1 && v2 && v3 && v4 && v5 {
        "B (bare live :I1)"
    } else if relatch_ok {
        "C (live :I1 + :J1 re-latch)"
    } else {
        "D candidate (neither live rate change usable; check decel times)"
    };
    out.push(format!("SELECTED: {design}"));
    if !v6 {
        out.push("NOTE: a stray :I1 moved a stopped axis — the pulse fence is load-bearing".into());
    }
    out
}

fn mean(values: &[f64]) -> Option<f64> {
    let n = f64::from(u32::try_from(values.len()).ok()?);
    (n > 0.0).then(|| values.iter().sum::<f64>() / n)
}

fn write_summary(dir: &Path, summary: &Summary) -> Result<(), ProbeError> {
    let file =
        File::create(dir.join("summary.json")).map_err(|e| ProbeError::Output(e.to_string()))?;
    serde_json::to_writer_pretty(BufWriter::new(file), summary)
        .map_err(|e| ProbeError::Output(e.to_string()))
}

fn print_report(summary: &Summary) {
    let decel: Vec<f64> = summary.inert.iter().filter_map(|x| x.stop_time_s).collect();
    let errs: Vec<f64> = summary.edges.iter().filter_map(|x| x.error_ticks).collect();
    let err_max = errs.iter().copied().map(f64::abs).fold(0.0, f64::max);
    println!("identity: {:?}", summary.identity);
    println!(
        "E1: {} trials, max |net error| {err_max:.2} ticks ({:.4} s of RA)",
        summary.edges.len(),
        err_max * RA_SECONDS_PER_TICK
    );
    println!("stop time from sidereal (s): {decel:?}");
    for factor in EDGE_FACTORS {
        let errs: Vec<f64> = summary
            .edges
            .iter()
            .filter(|x| (x.factor - factor).abs() < 1e-9)
            .filter_map(|x| x.error_ticks)
            .collect();
        let ss: Vec<f64> = summary
            .stop_start
            .iter()
            .filter(|x| (x.factor - factor).abs() < 1e-9)
            .filter_map(|x| x.error_ticks)
            .collect();
        println!(
            "{factor}x net error ticks: live :I1 {:?} (n={}), stop/start {:?} (n={})",
            mean(&errs),
            errs.len(),
            mean(&ss),
            ss.len()
        );
    }
    println!(
        "RTT p50 {:?} p99 {:?} max {:?}",
        summary.rtt_p50_s, summary.rtt_p99_s, summary.rtt_max_s
    );
    if let Some(e) = &summary.run_error {
        println!("RUN ERROR: {e}");
    }
    println!("return: {}", summary.return_report);
    println!("safety stop: {}", summary.stop_report);
    for line in &summary.verdict {
        println!("{line}");
    }
}

async fn open_transport(args: &Args) -> Result<Box<dyn FrameTransport>, ProbeError> {
    if args.mock {
        return open_mock().await;
    }
    if args.port.is_empty() {
        return Err(ProbeError::Precondition(
            "--port is required without --mock".into(),
        ));
    }
    let factory = SerialTransportFactory::new(UsbConfig {
        port: args.port.clone(),
        baud_rate: args.baud,
        command_timeout: COMMAND_TIMEOUT,
        polling_interval: Duration::from_millis(200),
    });
    factory
        .open()
        .await
        .map_err(|e| ProbeError::Transport(e.to_string()))
}

#[cfg(feature = "mock")]
async fn open_mock() -> Result<Box<dyn FrameTransport>, ProbeError> {
    use star_adventurer_gti::transport::mock::{AxisSimState, CapturingMockFactory};
    let factory = CapturingMockFactory::new();
    {
        let mut state = factory.state.lock().await;
        state.ra = AxisSimState {
            position_ticks: -907_200,
            initialized: true,
            ..AxisSimState::default()
        };
        state.dec = AxisSimState {
            position_ticks: 725_760,
            initialized: true,
            ..AxisSimState::default()
        };
    }
    factory
        .open()
        .await
        .map_err(|e| ProbeError::Transport(e.to_string()))
}

#[cfg(not(feature = "mock"))]
fn open_mock() -> std::future::Ready<Result<Box<dyn FrameTransport>, ProbeError>> {
    std::future::ready(Err(ProbeError::Precondition(
        "--mock needs `--features mock`".into(),
    )))
}

/// Turn `SIGINT`, `SIGTERM`, `SIGHUP` (an SSH session dropping) and
/// `SIGQUIT` into the abort flag instead of their default termination, so
/// every one of them ends with the safety stop. Installed before the port
/// opens; later signals repeat the notice and change nothing.
fn spawn_signal_watch(abort: &Arc<AtomicBool>) -> Result<(), ProbeError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut streams = Vec::new();
        for kind in [
            SignalKind::interrupt(),
            SignalKind::terminate(),
            SignalKind::hangup(),
            SignalKind::quit(),
        ] {
            streams.push(
                signal(kind).map_err(|e| ProbeError::Output(format!("signal handler: {e}")))?,
            );
        }
        for mut stream in streams {
            let abort = Arc::clone(abort);
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    abort.store(true, Ordering::SeqCst);
                    report_signal();
                }
            });
        }
    }
    #[cfg(not(unix))]
    let abort = Arc::clone(abort);
    #[cfg(not(unix))]
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            abort.store(true, Ordering::SeqCst);
            report_signal();
        }
    });
    Ok(())
}

/// Say a signal arrived. After an SSH drop the terminal is already hung
/// up, and `eprintln!` would panic on the failed write, so the write's
/// error is ignored; the abort flag is set before this is called.
fn report_signal() {
    let _ = writeln!(
        std::io::stderr(),
        "signal received: stopping the mount after the current exchange"
    );
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match probe(&args).await {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(2),
        Err(e) => {
            eprintln!("probe failed before any motion: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(true)` when the run completed and the mount confirmed stopped.
async fn probe(args: &Args) -> Result<bool, ProbeError> {
    std::fs::create_dir_all(&args.out).map_err(|e| ProbeError::Output(e.to_string()))?;
    let log = File::create(args.out.join("exchanges.jsonl"))
        .map_err(|e| ProbeError::Output(e.to_string()))?;
    let abort = Arc::new(AtomicBool::new(false));
    spawn_signal_watch(&abort)?;
    let transport = open_transport(args).await?;
    let mut link = Link {
        transport,
        epoch: Instant::now(),
        log: BufWriter::new(log),
        tag: String::new(),
        period_range: (0, 0),
        rtts: Vec::new(),
        abort,
    };
    let mut summary = Summary {
        thresholds: Some(THRESHOLDS),
        ..Summary::default()
    };
    let result = run(&mut link, args, &mut summary).await;
    if !link.abort.load(Ordering::SeqCst) {
        if let Some(id) = &summary.identity {
            let (start, sidereal) = (id.ra_ticks, summary.sidereal_period);
            summary.return_report = return_to_start(&mut link, start, sidereal)
                .await
                .unwrap_or_else(|e| format!("return leg failed: {e}"));
        }
    }
    summary.stop_report = safety_stop(&mut link).await;
    let _ = link.log.flush();
    summary.run_error = result.err().map(|e| e.to_string());
    let mut rtts = link.rtts.clone();
    rtts.sort_by(f64::total_cmp);
    summary.rtt_p50_s = percentile(&rtts, 50);
    summary.rtt_p99_s = percentile(&rtts, 99);
    summary.rtt_max_s = rtts.last().copied();
    summary.verdict = verdict(&summary);
    write_summary(&args.out, &summary)?;
    print_report(&summary);
    Ok(summary.run_error.is_none() && summary.stop_report.contains("RA stopped"))
}

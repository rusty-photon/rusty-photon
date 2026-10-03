//! Stop-coast probe: how long, and how far, does a Star Adventurer `GTi`
//! axis keep moving after a stop command from the driver's own goto
//! speed — and does `:f`'s running flag clear only once the step count
//! has stopped changing?
//!
//! The safety stop the service issues on a last-client disconnect, at a
//! cold start and after a reconnect (`:L1`, `:L2`, `:K1`) is judged on its
//! replies alone. Whether it should also read `:f` back, and how long it
//! should wait for an axis to come to rest, turns on two firmware facts
//! no document or mock settles: the time a moving axis takes to stop
//! after `:L` (instant stop) and after `:K` (decelerating stop), and
//! whether the count still moves once `:f` reports the axis stopped.
//! This tool measures both on the real mount, from dense `:f` and `:j`
//! samples taken back to back after the stop, and the same for every
//! goto's natural landing as a baseline.
//!
//! Experiments, in order:
//!
//! * **Dec** — a 30° Dec goto stopped once it has covered
//!   `--stop-after-deg`, cycling away from the pole with `:L2`, away with
//!   `:K2`, toward the pole with `:L2`, toward with `:K2`. A goto toward the
//!   pole first moves Dec 30° away to start from.
//! * **RA** — an RA goto with Dec at the pole, cycling `:L1` +, `:L1` −,
//!   `:K1` +, `:K1` −.
//! * **Sequence** — both axes in a goto at once, stopped with the service's
//!   own safety-stop sequence `:L1 :L2 :K1`, back to back.
//!
//! Every trial ends with a goto back to the pose the run started from.
//!
//! **Settling.** An axis counts as at rest once `:f` reports it stopped and
//! its count has not changed for [`SETTLE_WINDOW_S`], and that has held for
//! [`HOLD_S`]. A stopped `GTi` count is exactly constant, but it can still
//! move by a few ticks for about a second after `:f` first reads stopped,
//! which is why the hold is long and the tolerance is zero.
//!
//! **Envelope.** The run must start at the `ApPark3` pose (RA counterweight
//! down, Dec at the pole). Motion stays inside a fixed envelope: RA within
//! mechanical HA −8.5 h .. −3.5 h, well clear of the counterweight
//! exclusion zone, and Dec within encoder 58° .. 92°, so the optical axis
//! never points more than 32° from the celestial pole. A goto's start and
//! target are checked against the envelope before its `:J`, and every
//! count the probe reads is checked too: one outside the envelope halts
//! every axis at once and ends the run. A stop can only end a goto early,
//! so even a mount that ignored every stop would come to rest at a goto
//! target, inside the envelope. Only the commands in [`Op`] reach the wire.
//!
//! **Blocked.** An axis whose `:f` reports `blocked` (stepping, but not
//! following) is treated as the service's slew watcher treats it: every
//! axis is halted at once, that one first, and the run ends. `:f` is
//! read on every pass while an axis moves, and a goto refuses to start
//! on a blocked axis.
//!
//! Every exit path — a finished run, an error, `SIGINT`, `SIGTERM`,
//! `SIGHUP`, `SIGQUIT` — ends with `:L1 :L2 :K1` and waits for both axes to
//! report stopped.
//!
//! This is an operator-run bench tool, never part of the service: the
//! `rusty-photon-star-adventurer-gti` service must be stopped first so the
//! probe has the port to itself. Run it on the rig with the mount parked at
//! `ApPark3`, with a fresh output directory each time, and with no
//! controlling terminal (`systemd-run`, `setsid -f`, tmux) so a dropped
//! session cannot end it. `nohup` is not enough: the probe turns `SIGHUP`
//! into an abort whatever disposition it inherited.
//!
//! ```text
//! cargo run --release -p star-adventurer-gti --example probe_stop_coast -- \
//!     --port /dev/serial/by-id/usb-STMicroelectronics_STM32_Virtual_ComPort_<id>-if00 \
//!     --out ~/probe-stop/$(date -u +%Y%m%dT%H%M%SZ)
//! ```
//!
//! A logic-only dry run against the in-repo mock (circular by
//! construction — it proves the tool, not the firmware):
//!
//! ```text
//! cargo run -p star-adventurer-gti --features mock --example probe_stop_coast -- \
//!     --mock --out /tmp/probe-stop-mock
//! ```
//!
//! Every exchange is written to `<out>/exchanges.jsonl` and the per-trial
//! analysis, with its raw samples, to `<out>/summary.json`.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rusty_photon_shared_transport::{FrameTransport, TransportFactory};
use serde::Serialize;
use skywatcher_motor_protocol::{Axis, AxisStatus, Command, ModeKind, MotionMode, Response, Speed};
use star_adventurer_gti::{SerialTransportFactory, UsbConfig};
use tokio::time::Instant;

/// `:e1` reply of the Star Adventurer `GTi` this probe was written for
/// (motor-controller firmware 3.48, mount code `0x0C`).
const EXPECTED_VERSION_REPLY: &str = "=03300C";
const EXPECTED_CPR_RA: u32 = 3_628_800;
const EXPECTED_CPR_DEC: u32 = 2_903_040;
const EXPECTED_TMR_FREQ: u32 = 16_000_000;

/// The `ApPark3` pose the run must start from.
const PARK3_RA_TICKS: i32 = -907_200;
const PARK3_DEC_TICKS: i32 = 725_760;
/// How far from `ApPark3` each axis may start, in encoder ticks.
const START_TOLERANCE_TICKS: i32 = 2_000;

/// RA envelope, mechanical HA hours.
const RA_ENVELOPE_H: (f64, f64) = (-8.5, -3.5);
/// Dec envelope, encoder degrees (90° is the pole).
const DEC_ENVELOPE_DEG: (f64, f64) = (58.0, 92.0);
/// The counterweight exclusion zone configured on the rig, mechanical HA
/// hours, and the clearance the RA envelope must keep from it.
const CW_ZONE_H: (f64, f64) = (0.95, 11.05);
const CW_ZONE_CLEARANCE_H: f64 = 2.0;

/// The driver's goto wire constants (`mount_device/slew.rs`): step period
/// `6`, break-point increment `min(|delta| / 10, 3200)`.
const SLEW_STEP_PERIOD: u32 = 6;
const SLEW_BREAK_POINT_DIVISOR: u32 = 10;
const SLEW_BREAK_POINT_MAX: u32 = 3200;

/// Per-request read / write timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
/// Pacing of the `:j` samples while a goto runs up to speed.
const RUNUP_INTERVAL: Duration = Duration::from_millis(4);
/// Longest the run-up may take to cover `--stop-after-deg`, beyond the
/// time it would take at 1°/s.
const RUNUP_SLACK: Duration = Duration::from_secs(3);
/// Longest an axis may take to come to rest after a stop. On a timeout
/// the probe sends `:L` to every axis it was watching.
const STOP_OBSERVE_TIMEOUT: Duration = Duration::from_secs(12);
/// An axis has settled once `:f` reads it stopped and its count has not
/// changed for this long.
const SETTLE_WINDOW_S: f64 = 0.15;
/// How long every watched axis must stay settled before the observation
/// ends. Any change to a count in that time restarts it.
const HOLD_S: f64 = 1.5;
/// Pre-stop window used to estimate the speed at the stop, and the
/// earlier window it is compared against to call the speed steady.
const SPEED_WINDOW_S: f64 = 0.25;
/// Largest relative change between the two speed windows still called
/// cruise.
const CRUISE_TOLERANCE: f64 = 0.03;
/// Return legs: the slowest goto rate the time budget allows for, and
/// the fixed allowance on top.
const RETURN_MIN_RATE_DEG_S: f64 = 2.0;
const RETURN_SLACK_S: f64 = 8.0;
/// A return leg that rests further than this from its target is re-run
/// once; further than [`RETURN_ABORT_TICKS`] after that ends the run.
const RETURN_TOLERANCE_TICKS: i32 = 10;
const RETURN_ABORT_TICKS: i32 = 200;
/// Wait for both axes to report stopped in the final safety stop.
const FINAL_STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(
    about = "Measure how far and how long a GTi axis moves after :L / :K from goto speed (operator-run)"
)]
struct Args {
    /// Serial port of the mount (use the /dev/serial/by-id path).
    #[arg(long, default_value = "")]
    port: String,
    /// Baud rate.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    /// Output directory for exchanges.jsonl and summary.json; must not
    /// already hold a run.
    #[arg(long)]
    out: PathBuf,
    /// Length of every goto, degrees of axis rotation.
    #[arg(long, default_value_t = 30.0)]
    goto_deg: f64,
    /// Distance into the goto at which the stop is sent, degrees.
    #[arg(long, default_value_t = 12.0)]
    stop_after_deg: f64,
    /// Dec trials, cycling away/`:L2`, away/`:K2`, toward/`:L2`, toward/`:K2`.
    #[arg(long, default_value_t = 12)]
    dec_trials: u32,
    /// RA trials, cycling `:L1` +, `:L1` −, `:K1` +, `:K1` −.
    #[arg(long, default_value_t = 12)]
    ra_trials: u32,
    /// Trials of the service's safety-stop sequence on both axes at once.
    #[arg(long, default_value_t = 4)]
    seq_trials: u32,
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

/// The only commands the probe can put on the wire.
#[derive(Debug, Clone, Copy)]
enum Op {
    Version,
    Cpr(Axis),
    TmrFreq,
    Position(Axis),
    Status(Axis),
    /// `:G<axis>` goto / fast, direction by `ccw`.
    GotoMode {
        axis: Axis,
        ccw: bool,
    },
    /// `:I<axis>6`, the driver's goto step period.
    GotoPeriod(Axis),
    /// `:H<axis>`, the goto's target increment.
    Increment {
        axis: Axis,
        increment: u32,
    },
    /// `:M<axis>`, the goto's break-point increment.
    Breaks {
        axis: Axis,
        breaks: u32,
    },
    /// `:J<axis>`. Refused unless [`goto`] armed the axis.
    Start(Axis),
    /// `:K<axis>`.
    Stop(Axis),
    /// `:L<axis>`.
    Halt(Axis),
}

impl Op {
    const fn command(self) -> Command {
        match self {
            Self::Version => Command::InquireMotorBoardVersion(Axis::Ra),
            Self::Cpr(axis) => Command::InquireCpr(axis),
            Self::TmrFreq => Command::InquireTmrFreq,
            Self::Position(axis) => Command::InquirePosition(axis),
            Self::Status(axis) => Command::InquireStatus(axis),
            Self::GotoMode { axis, ccw } => Command::SetMotionMode {
                axis,
                mode: MotionMode {
                    kind: ModeKind::Goto,
                    speed: Speed::Fast,
                    ccw,
                },
            },
            Self::GotoPeriod(axis) => Command::SetStepPeriod {
                axis,
                period: SLEW_STEP_PERIOD,
            },
            Self::Increment { axis, increment } => {
                Command::SetGotoTargetIncrement { axis, increment }
            }
            Self::Breaks { axis, breaks } => Command::SetBreakPointIncrement { axis, breaks },
            Self::Start(axis) => Command::StartMotion(axis),
            Self::Stop(axis) => Command::StopMotion(axis),
            Self::Halt(axis) => Command::InstantStop(axis),
        }
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

/// A `:j` reading. The firmware latches the count as the command
/// arrives, so `t_send` is the end of the round trip that dates it.
#[derive(Debug, Clone, Copy, Serialize)]
struct Sample {
    t_send: f64,
    t_recv: f64,
    ticks: i32,
}

/// A `:f` reading.
#[derive(Debug, Clone, Copy, Serialize)]
struct StatusSample {
    t_send: f64,
    t_recv: f64,
    running: bool,
    blocked: bool,
    goto: bool,
}

/// Per-axis geometry: ticks per degree and the envelope in ticks.
#[derive(Debug, Clone, Copy, Serialize)]
struct AxisGeometry {
    ticks_per_deg: f64,
    lo: i32,
    hi: i32,
    start: i32,
}

impl AxisGeometry {
    fn contains(&self, ticks: i32) -> bool {
        (self.lo..=self.hi).contains(&ticks)
    }

    fn check(&self, axis: Axis, ticks: i32, what: &str) -> Result<(), ProbeError> {
        if self.contains(ticks) {
            Ok(())
        } else {
            Err(ProbeError::Precondition(format!(
                "{axis:?} {what} {ticks} is outside the envelope {}..={}",
                self.lo, self.hi
            )))
        }
    }

    fn ticks(&self, degrees: f64) -> Result<i32, ProbeError> {
        to_ticks(degrees * self.ticks_per_deg)
    }

    fn degrees(&self, ticks: i32) -> f64 {
        f64::from(ticks) / self.ticks_per_deg
    }
}

/// Round a tick count held as `f64` to `i32`, refusing anything outside
/// the 24-bit counter's range.
fn to_ticks(value: f64) -> Result<i32, ProbeError> {
    let rounded = value.round();
    if !rounded.is_finite() || rounded.abs() >= 8_388_608.0 {
        return Err(ProbeError::Precondition(format!(
            "tick count {value} is outside the 24-bit range"
        )));
    }
    // In range of the 24-bit counter, so the string round trip is exact.
    format!("{rounded:.0}")
        .parse::<i32>()
        .map_err(|e| ProbeError::Precondition(format!("tick count {value}: {e}")))
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Geometry {
    ra: AxisGeometry,
    dec: AxisGeometry,
}

impl Geometry {
    const fn axis(&self, axis: Axis) -> &AxisGeometry {
        match axis {
            Axis::Dec => &self.dec,
            _ => &self.ra,
        }
    }
}

struct Link {
    transport: Box<dyn FrameTransport>,
    epoch: Instant,
    log: BufWriter<File>,
    tag: String,
    armed_ra: bool,
    armed_dec: bool,
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

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::SeqCst)
    }

    const fn arm(&mut self, axis: Axis) {
        match axis {
            Axis::Dec => self.armed_dec = true,
            _ => self.armed_ra = true,
        }
    }

    /// Consume the arming [`goto`] left for `axis`.
    const fn take_armed(&mut self, axis: Axis) -> bool {
        let slot = match axis {
            Axis::Dec => &mut self.armed_dec,
            _ => &mut self.armed_ra,
        };
        let armed = *slot;
        *slot = false;
        armed
    }

    async fn ask(&mut self, op: Op) -> Result<Exchange, ProbeError> {
        if self.aborted() && !op.stop_class() {
            return Err(ProbeError::Aborted);
        }
        if let Op::Start(axis) = op {
            if !self.take_armed(axis) {
                return Err(ProbeError::Precondition(format!(
                    ":J{axis:?} refused: no goto armed it"
                )));
            }
        }
        let command = op.command();
        let frame = command
            .encode()
            .map_err(|e| ProbeError::Transport(e.to_string()))?;
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
        match io {
            Ok(buf) => {
                let raw = String::from_utf8_lossy(&buf).trim_end().to_string();
                let reply = Response::decode(&buf, &command).map_err(|e| e.to_string());
                self.log_line(&cmd_text, t_send, t_recv, &raw);
                self.rtts.push(t_recv - t_send);
                Ok(Exchange {
                    t_send,
                    t_recv,
                    reply,
                    raw,
                })
            }
            Err(e) => {
                self.log_line(
                    &cmd_text,
                    t_send,
                    t_recv,
                    &format!("<transport error: {e}>"),
                );
                // A stop must not wait on a drain: the next axis' halt goes
                // out first. A late ack shifted onto it is harmless.
                if !matches!(op, Op::Halt(_) | Op::Stop(_)) {
                    self.drain(&cmd_text).await;
                }
                Err(ProbeError::Transport(format!("{cmd_text}: {e}")))
            }
        }
    }

    /// After a failed exchange, swallow a reply that may still arrive for
    /// it, so it cannot be read as the answer to the next command.
    async fn drain(&mut self, after: &str) {
        let t = self.now();
        let mut buf = Vec::new();
        let drained = match self.transport.recv_frame(&mut buf).await {
            Ok(()) => format!(
                "<drained late reply: {}>",
                String::from_utf8_lossy(&buf).trim_end()
            ),
            Err(e) => format!("<nothing to drain: {e}>"),
        };
        let now = self.now();
        self.log_line(&format!("drain after {after}"), t, now, &drained);
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

    async fn position(&mut self, axis: Axis) -> Result<Sample, ProbeError> {
        let op = Op::Position(axis);
        let ex = self.ask_ok(op).await?;
        match ex.reply {
            Ok(Response::Position(ticks)) => Ok(Sample {
                t_send: ex.t_send,
                t_recv: ex.t_recv,
                ticks,
            }),
            _ => Err(unexpected(op, &ex)),
        }
    }

    async fn status(&mut self, axis: Axis) -> Result<(StatusSample, AxisStatus), ProbeError> {
        let op = Op::Status(axis);
        let ex = self.ask_ok(op).await?;
        match ex.reply {
            Ok(Response::Status(status)) => Ok((
                StatusSample {
                    t_send: ex.t_send,
                    t_recv: ex.t_recv,
                    running: status.motion.running,
                    blocked: status.motion.blocked,
                    goto: status.mode == ModeKind::Goto,
                },
                status,
            )),
            _ => Err(unexpected(op, &ex)),
        }
    }

    /// `:L` to every axis, `first` (if given) first, one round trip after
    /// another with errors ignored, and no wait for any axis to stop.
    async fn halt_all(&mut self, axes: &[Axis], first: Option<Axis>) {
        let rest = axes.iter().filter(|a| Some(**a) != first);
        for axis in first.iter().chain(rest) {
            let _ = self.ask(Op::Halt(*axis)).await;
        }
    }

    /// Poll `:f` on `axis` until it reports stopped; `false` on timeout.
    async fn wait_stopped(&mut self, axis: Axis, timeout: Duration) -> Result<bool, ProbeError> {
        let end = deadline(timeout)?;
        loop {
            let (status, _) = self.status(axis).await?;
            if !status.running {
                return Ok(true);
            }
            if Instant::now() >= end {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn deadline(after: Duration) -> Result<Instant, ProbeError> {
    Instant::now()
        .checked_add(after)
        .ok_or_else(|| ProbeError::Precondition(format!("deadline {after:?} overflows")))
}

fn seconds(value: f64) -> Result<Duration, ProbeError> {
    Duration::try_from_secs_f64(value)
        .map_err(|e| ProbeError::Precondition(format!("duration {value} s: {e}")))
}

fn blocked(axis: Axis) -> ProbeError {
    ProbeError::Precondition(format!(
        "{axis:?} reports blocked (stepping, not following); halted every axis"
    ))
}

fn unexpected(op: Op, ex: &Exchange) -> ProbeError {
    ProbeError::Unexpected {
        cmd: format!("{op:?}"),
        reply: ex.raw.clone(),
    }
}

#[derive(Debug, Serialize)]
struct Identity {
    version_reply: String,
    cpr_ra: u32,
    cpr_dec: u32,
    tmr_freq: u32,
    ra_ticks: i32,
    dec_ticks: i32,
}

/// Check identity, geometry, state and pose before anything moves.
async fn preflight(link: &mut Link) -> Result<(Identity, Geometry), ProbeError> {
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
        if !status.init.initialized || status.motion.running || status.motion.blocked {
            return Err(ProbeError::Precondition(format!(
                "{axis:?} must be initialised, stopped and not blocked; :f reads {status:?}"
            )));
        }
    }
    let ra_ticks = link.position(Axis::Ra).await?.ticks;
    let dec_ticks = link.position(Axis::Dec).await?.ticks;
    for (axis, at, park) in [
        (Axis::Ra, ra_ticks, PARK3_RA_TICKS),
        (Axis::Dec, dec_ticks, PARK3_DEC_TICKS),
    ] {
        if at.abs_diff(park) > START_TOLERANCE_TICKS.unsigned_abs() {
            return Err(ProbeError::Precondition(format!(
                "{axis:?} is at {at}, not within {START_TOLERANCE_TICKS} ticks of ApPark3 ({park}); \
                 park the mount at ApPark3 first"
            )));
        }
    }
    let geometry = geometry(cpr_ra, cpr_dec, ra_ticks, dec_ticks)?;
    Ok((
        Identity {
            version_reply: version.raw,
            cpr_ra,
            cpr_dec,
            tmr_freq,
            ra_ticks,
            dec_ticks,
        },
        geometry,
    ))
}

/// The envelope in ticks, checked against the counterweight zone.
fn geometry(cpr_ra: u32, cpr_dec: u32, ra: i32, dec: i32) -> Result<Geometry, ProbeError> {
    let (ra_lo_h, ra_hi_h) = RA_ENVELOPE_H;
    let (zone_lo, zone_hi) = CW_ZONE_H;
    // Mechanical HA folds to [-12, 12); the envelope must clear the zone
    // on both sides of the fold.
    let clear_below = ra_hi_h + CW_ZONE_CLEARANCE_H <= zone_lo;
    let clear_wrapped = ra_lo_h - CW_ZONE_CLEARANCE_H + 24.0 >= zone_hi;
    if !(clear_below && clear_wrapped && ra_lo_h > -12.0) {
        return Err(ProbeError::Precondition(format!(
            "RA envelope {RA_ENVELOPE_H:?} h does not keep {CW_ZONE_CLEARANCE_H} h from the \
             counterweight zone {CW_ZONE_H:?}"
        )));
    }
    let ra_ticks_per_h = f64::from(cpr_ra) / 24.0;
    let dec_ticks_per_deg = f64::from(cpr_dec) / 360.0;
    let (dec_lo_deg, dec_hi_deg) = DEC_ENVELOPE_DEG;
    Ok(Geometry {
        ra: AxisGeometry {
            ticks_per_deg: f64::from(cpr_ra) / 360.0,
            lo: to_ticks(ra_lo_h * ra_ticks_per_h)?,
            hi: to_ticks(ra_hi_h * ra_ticks_per_h)?,
            start: ra,
        },
        dec: AxisGeometry {
            ticks_per_deg: dec_ticks_per_deg,
            lo: to_ticks(dec_lo_deg * dec_ticks_per_deg)?,
            hi: to_ticks(dec_hi_deg * dec_ticks_per_deg)?,
            start: dec,
        },
    })
}

/// An axis' name for the summary; the protocol crate does not derive
/// `Serialize` for [`Axis`].
const fn label(axis: Axis) -> &'static str {
    match axis {
        Axis::Dec => "Dec",
        _ => "Ra",
    }
}

/// The stop a trial sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum StopKind {
    /// `:L<axis>`.
    Instant,
    /// `:K<axis>`.
    Decelerate,
    /// `:L1 :L2 :K1`, the service's safety stop.
    Sequence,
}

/// One axis' share of a trial: where its goto starts and which way it
/// runs.
#[derive(Debug, Clone, Copy, Serialize)]
struct LegPlan {
    #[serde(skip)]
    axis: Axis,
    axis_label: &'static str,
    direction: i32,
    origin: i32,
    target: i32,
}

#[derive(Debug, Clone, Serialize)]
struct TrialPlan {
    tag: String,
    kind: StopKind,
    legs: Vec<LegPlan>,
}

fn leg(
    geometry: &Geometry,
    axis: Axis,
    direction: i32,
    origin: i32,
    goto_deg: f64,
) -> Result<LegPlan, ProbeError> {
    let g = geometry.axis(axis);
    let target = g
        .ticks(goto_deg)?
        .checked_mul(direction)
        .and_then(|t| origin.checked_add(t))
        .ok_or_else(|| ProbeError::Precondition(format!("goto of {goto_deg}° overflows")))?;
    g.check(axis, origin, "planned goto start")?;
    g.check(axis, target, "planned goto target")?;
    Ok(LegPlan {
        axis,
        axis_label: label(axis),
        direction,
        origin,
        target,
    })
}

/// Every trial, with every goto's start and target checked against the
/// envelope before anything moves.
fn plan(geometry: &Geometry, args: &Args) -> Result<Vec<TrialPlan>, ProbeError> {
    if !(args.stop_after_deg > 1.0 && args.stop_after_deg + 8.0 <= args.goto_deg) {
        return Err(ProbeError::Precondition(format!(
            "--stop-after-deg {} must be above 1 and at least 8 below --goto-deg {}",
            args.stop_after_deg, args.goto_deg
        )));
    }
    let (ra0, dec0) = (geometry.ra.start, geometry.dec.start);
    let dec_low = leg(geometry, Axis::Dec, -1, dec0, args.goto_deg)?.target;
    let mut trials = Vec::new();
    for i in 0..args.dec_trials {
        let (direction, origin, kind) = match i % 4 {
            0 => (-1, dec0, StopKind::Instant),
            1 => (-1, dec0, StopKind::Decelerate),
            2 => (1, dec_low, StopKind::Instant),
            _ => (1, dec_low, StopKind::Decelerate),
        };
        trials.push(TrialPlan {
            tag: format!("dec/{i}"),
            kind,
            legs: vec![leg(geometry, Axis::Dec, direction, origin, args.goto_deg)?],
        });
    }
    for i in 0..args.ra_trials {
        let (direction, kind) = match i % 4 {
            0 => (1, StopKind::Instant),
            1 => (-1, StopKind::Instant),
            2 => (1, StopKind::Decelerate),
            _ => (-1, StopKind::Decelerate),
        };
        trials.push(TrialPlan {
            tag: format!("ra/{i}"),
            kind,
            legs: vec![leg(geometry, Axis::Ra, direction, ra0, args.goto_deg)?],
        });
    }
    for i in 0..args.seq_trials {
        let direction = if i.is_multiple_of(2) { 1 } else { -1 };
        trials.push(TrialPlan {
            tag: format!("seq/{i}"),
            kind: StopKind::Sequence,
            legs: vec![
                leg(geometry, Axis::Ra, direction, ra0, args.goto_deg)?,
                leg(geometry, Axis::Dec, -1, dec0, args.goto_deg)?,
            ],
        });
    }
    Ok(trials)
}

/// The driver's goto sequence: `:G :I6 :H :M :J`, after checking the
/// axis is stopped and both ends of the move lie inside the envelope.
/// Returns the count it started from and when its `:J` went out.
async fn goto(
    link: &mut Link,
    geometry: &Geometry,
    axis: Axis,
    target: i32,
) -> Result<(i32, f64), ProbeError> {
    let g = *geometry.axis(axis);
    let (status, _) = link.status(axis).await?;
    if status.running || status.blocked {
        return Err(ProbeError::Precondition(format!(
            "{axis:?} is running or blocked ({status:?}); a goto needs it stopped and free"
        )));
    }
    let from = link.position(axis).await?.ticks;
    g.check(axis, from, "goto start")?;
    g.check(axis, target, "goto target")?;
    let delta = target
        .checked_sub(from)
        .ok_or_else(|| ProbeError::Precondition("goto delta overflows".into()))?;
    if delta == 0 {
        return Ok((from, link.now()));
    }
    let increment = delta.unsigned_abs();
    let breaks = (increment / SLEW_BREAK_POINT_DIVISOR).min(SLEW_BREAK_POINT_MAX);
    link.ask_ok(Op::GotoMode {
        axis,
        ccw: delta < 0,
    })
    .await?;
    link.ask_ok(Op::GotoPeriod(axis)).await?;
    link.ask_ok(Op::Increment { axis, increment }).await?;
    link.ask_ok(Op::Breaks { axis, breaks }).await?;
    link.arm(axis);
    let start = link.ask_ok(Op::Start(axis)).await?;
    Ok((from, start.t_send))
}

/// `:f` and `:j` samples of one axis.
#[derive(Debug, Default, Serialize)]
struct Trace {
    samples: Vec<Sample>,
    statuses: Vec<StatusSample>,
}

impl Trace {
    /// Whether `:f` last read the axis stopped and its count has not
    /// changed for [`SETTLE_WINDOW_S`].
    fn settled(&self) -> bool {
        let stopped = self.statuses.last().is_some_and(|s| !s.running);
        stopped
            && settled_at(&self.samples)
                .and_then(|i| self.samples.get(i))
                .zip(self.samples.last())
                .is_some_and(|(a, b)| b.t_send - a.t_send >= SETTLE_WINDOW_S)
    }
}

/// Index of the first sample of the final run of identical counts.
fn settled_at(samples: &[Sample]) -> Option<usize> {
    let last = samples.last()?.ticks;
    let tail = samples.iter().rev().take_while(|s| s.ticks == last).count();
    samples.len().checked_sub(tail)
}

/// Sample `:f` and `:j` on every axis, one pass every
/// [`RUNUP_INTERVAL`], until all have settled and stayed settled for
/// [`HOLD_S`]. Every count is checked against the envelope; one outside
/// it halts every axis, that one first, and fails. On `timeout` every
/// axis gets `:L` and the traces come back marked timed out.
async fn observe(
    link: &mut Link,
    geometry: &Geometry,
    axes: &[Axis],
    timeout: Duration,
) -> Result<(Vec<Trace>, bool), ProbeError> {
    let limit = deadline(timeout)?;
    let mut traces: Vec<Trace> = axes.iter().map(|_| Trace::default()).collect();
    let mut held_since: Option<f64> = None;
    let mut ticker = tokio::time::interval(RUNUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if link.aborted() {
            return Err(ProbeError::Aborted);
        }
        let mut all_settled = true;
        for (axis, trace) in axes.iter().zip(traces.iter_mut()) {
            let (status, _) = link.status(*axis).await?;
            trace.statuses.push(status);
            if status.blocked {
                link.halt_all(axes, Some(*axis)).await;
                return Err(blocked(*axis));
            }
            let sample = link.position(*axis).await?;
            trace.samples.push(sample);
            if let Err(e) = geometry.axis(*axis).check(*axis, sample.ticks, "count") {
                link.halt_all(axes, Some(*axis)).await;
                return Err(e);
            }
            all_settled &= trace.settled();
        }
        let now = link.now();
        if all_settled {
            let since = *held_since.get_or_insert(now);
            if now - since >= HOLD_S {
                return Ok((traces, false));
            }
        } else {
            held_since = None;
        }
        if Instant::now() >= limit {
            link.halt_all(axes, None).await;
            return Ok((traces, true));
        }
    }
}

/// How one axis' motion ended, read from its trace.
#[derive(Debug, Default, Serialize)]
struct MotionEnd {
    /// Whether any `:f` in the trace read the axis running.
    running_seen: bool,
    /// When `:f` first read the axis stopped, seconds after the reference
    /// instant: after the last `:f` that read it running was sent (or the
    /// reference, if none did), and before the clearing `:f` came back.
    running_clear_s: Option<(f64, f64)>,
    /// When the count reached its resting value for the last time,
    /// seconds after the reference: the first sample of the final run of
    /// identical counts.
    settle_s: Option<f64>,
    rest_ticks: Option<i32>,
    /// The count from the last `:j` sent before the clearing `:f`.
    at_clear_ticks: Option<i32>,
    /// After the clear, the furthest the count went along the motion and
    /// against it, relative to `at_clear_ticks`, and where it rested.
    /// Positive forward travel means `:f` read stopped while the axis
    /// still moved on; reverse travel is a correction back.
    after_clear_forward_ticks: Option<i32>,
    after_clear_reverse_ticks: Option<i32>,
    after_clear_net_ticks: Option<i32>,
    blocked_seen: bool,
}

/// Read how a motion ended. `t_ref` is the stop (or the goto's `:J`);
/// `direction` the sign of the motion in ticks. A `:f` reading stopped
/// only counts as the clear once one has read running, unless none ever
/// did, so a running bit that lags a fresh `:J` cannot pass for a stop.
fn motion_end(trace: &Trace, t_ref: f64, direction: i32) -> MotionEnd {
    let after: Vec<&StatusSample> = trace
        .statuses
        .iter()
        .filter(|s| s.t_send >= t_ref)
        .collect();
    let running_seen = after.iter().any(|s| s.running);
    let first_running = after.iter().position(|s| s.running);
    let clear_idx = after
        .iter()
        .enumerate()
        .position(|(i, s)| !s.running && first_running.is_none_or(|r| i > r));
    let clear = clear_idx.and_then(|i| after.get(i)).copied();
    let previous_running = clear_idx
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| after.get(i))
        .filter(|s| s.running);
    let running_clear_s = clear.map(|c| {
        let lower = previous_running.map_or(0.0, |p| p.t_send - t_ref);
        (lower, c.t_recv - t_ref)
    });
    let rest = trace.samples.last().map(|s| s.ticks);
    let settle_s = settled_at(&trace.samples)
        .and_then(|i| trace.samples.get(i))
        .map(|s| s.t_send - t_ref);
    // The last count read before the clearing `:f`; when the very first
    // `:f` already reads stopped there is none, so the first count after.
    let at_clear = clear.and_then(|c| {
        trace
            .samples
            .iter()
            .rev()
            .find(|s| s.t_send < c.t_send)
            .or_else(|| trace.samples.first())
            .map(|s| s.ticks)
    });
    let later: Vec<i32> = clear.map_or_else(Vec::new, |c| {
        trace
            .samples
            .iter()
            .filter(|s| s.t_send > c.t_send)
            .map(|s| s.ticks)
            .collect()
    });
    let along = |ticks: i32| at_clear.and_then(|a| ticks.checked_sub(a)?.checked_mul(direction));
    let excursions: Vec<i32> = later.iter().filter_map(|t| along(*t)).collect();
    MotionEnd {
        running_seen,
        running_clear_s,
        settle_s,
        rest_ticks: rest,
        at_clear_ticks: at_clear,
        after_clear_forward_ticks: excursions.iter().copied().max().map(|m| m.max(0)),
        after_clear_reverse_ticks: excursions
            .iter()
            .copied()
            .min()
            .map(|m| m.min(0).saturating_neg()),
        after_clear_net_ticks: rest.and_then(along),
        blocked_seen: trace.statuses.iter().any(|s| s.blocked),
    }
}

/// One axis' motion from the stop to rest.
#[derive(Debug, Serialize)]
struct AxisStop {
    leg: LegPlan,
    kind: StopKind,
    /// Count the goto started from.
    from: i32,
    /// Epoch time the axis' own stop command was sent (`:L` for the
    /// sequence).
    t_stop: f64,
    /// Distance covered from the goto's start to the stop, degrees.
    runup_deg: f64,
    /// Speed over the last [`SPEED_WINDOW_S`] before the stop, °/s.
    speed_deg_s: Option<f64>,
    /// Whether the speed was steady across the two pre-stop windows.
    cruise: bool,
    /// Signed travel from the stop to rest, along the goto's direction.
    coast_ticks: Option<i32>,
    coast_deg: Option<f64>,
    /// `2 · coast / speed`, the stop time a constant deceleration from the
    /// measured speed would take, seconds.
    constant_decel_s: Option<f64>,
    end: MotionEnd,
    timed_out: bool,
    pre: Vec<Sample>,
    trace: Trace,
}

/// A goto's natural landing, or a return leg's.
#[derive(Debug, Serialize)]
struct Landing {
    #[serde(skip)]
    axis: Axis,
    axis_label: &'static str,
    from: i32,
    target: i32,
    /// Epoch time the goto's `:J` went out.
    t_start: f64,
    end: MotionEnd,
    off_by_ticks: Option<i32>,
    timed_out: bool,
    trace: Trace,
}

#[derive(Debug, Serialize)]
struct Trial {
    plan: TrialPlan,
    prepositions: Vec<Landing>,
    stops: Vec<AxisStop>,
    landings: Vec<Landing>,
}

#[derive(Debug, Serialize, Default)]
struct Summary {
    identity: Option<Identity>,
    geometry: Option<Geometry>,
    args: String,
    trials: Vec<Trial>,
    rtt_p50_s: Option<f64>,
    rtt_p99_s: Option<f64>,
    rtt_max_s: Option<f64>,
    run_error: Option<String>,
    final_return: Vec<Landing>,
    final_return_error: Option<String>,
    stop_report: String,
}

/// Sample `:j` on every leg's axis until each has covered `stop_after`,
/// returning the samples per leg. A count outside the envelope, or a
/// run-up that takes too long, halts every leg's axis before failing.
async fn run_up(
    link: &mut Link,
    geometry: &Geometry,
    legs: &[(LegPlan, i32)],
    stop_after_deg: f64,
) -> Result<Vec<Vec<Sample>>, ProbeError> {
    let axes: Vec<Axis> = legs.iter().map(|(l, _)| l.axis).collect();
    let limit = deadline(seconds(stop_after_deg)?.saturating_add(RUNUP_SLACK))?;
    let mut samples: Vec<Vec<Sample>> = legs.iter().map(|_| Vec::new()).collect();
    let mut ticker = tokio::time::interval(RUNUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if link.aborted() {
            return Err(ProbeError::Aborted);
        }
        let mut done = true;
        for ((leg, from), out) in legs.iter().zip(samples.iter_mut()) {
            let g = geometry.axis(leg.axis);
            if link.status(leg.axis).await?.0.blocked {
                link.halt_all(&axes, Some(leg.axis)).await;
                return Err(blocked(leg.axis));
            }
            let s = link.position(leg.axis).await?;
            out.push(s);
            if let Err(e) = g.check(leg.axis, s.ticks, "count during the run-up") {
                link.halt_all(&axes, Some(leg.axis)).await;
                return Err(e);
            }
            let covered = g.degrees(s.ticks.saturating_sub(*from).saturating_mul(leg.direction));
            done &= covered >= stop_after_deg;
        }
        if done {
            return Ok(samples);
        }
        if Instant::now() >= limit {
            link.halt_all(&axes, None).await;
            return Err(ProbeError::Precondition(format!(
                "the goto did not cover {stop_after_deg}° in time; sent :L"
            )));
        }
    }
}

/// Least-squares slope of the samples dated within `[from, to]`, ticks/s.
fn slope_in(samples: &[Sample], from: f64, to: f64) -> Option<f64> {
    let window: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.t_send >= from && s.t_send <= to)
        .collect();
    if window.len() < 3 {
        return None;
    }
    let n = f64::from(u32::try_from(window.len()).ok()?);
    let mean_t = window.iter().map(|s| s.t_send).sum::<f64>() / n;
    let mean_x = window.iter().map(|s| f64::from(s.ticks)).sum::<f64>() / n;
    let (num, den) = window.iter().fold((0.0_f64, 0.0_f64), |(num, den), s| {
        let dt = s.t_send - mean_t;
        (
            dt.mul_add(f64::from(s.ticks) - mean_x, num),
            dt.mul_add(dt, den),
        )
    });
    (den > 0.0).then(|| num / den)
}

/// Turn one axis' samples into its stop record.
fn analyse_stop(
    (leg, from): (LegPlan, i32),
    kind: StopKind,
    g: &AxisGeometry,
    t_stop: f64,
    pre: Vec<Sample>,
    trace: Trace,
    timed_out: bool,
) -> AxisStop {
    let speed = slope_in(&pre, t_stop - SPEED_WINDOW_S, t_stop);
    let earlier = slope_in(
        &pre,
        SPEED_WINDOW_S.mul_add(-2.0, t_stop),
        t_stop - SPEED_WINDOW_S,
    );
    let cruise = speed
        .zip(earlier)
        .is_some_and(|(v, e)| v.abs() > 0.0 && ((v - e) / v).abs() <= CRUISE_TOLERANCE);
    let direction = f64::from(leg.direction);
    let last_pre = pre.iter().rev().find(|s| s.t_send <= t_stop);
    // The count at the moment the stop went out, carried forward from the
    // last pre-stop sample at the measured speed.
    let at_stop = last_pre.map(|s| {
        speed.map_or_else(
            || f64::from(s.ticks),
            |v| v.mul_add(t_stop - s.t_send, f64::from(s.ticks)),
        )
    });
    let end = motion_end(&trace, t_stop, leg.direction);
    let coast = at_stop
        .zip(end.rest_ticks)
        .map(|(a, r)| (f64::from(r) - a) * direction);
    let speed_deg_s = speed.map(|v| v * direction / g.ticks_per_deg);
    let coast_deg = coast.map(|c| c / g.ticks_per_deg);
    AxisStop {
        leg,
        kind,
        from,
        t_stop,
        runup_deg: last_pre.map_or(0.0, |s| g.degrees(s.ticks.saturating_sub(from)) * direction),
        speed_deg_s,
        cruise,
        coast_ticks: coast.and_then(|c| to_ticks(c).ok()),
        coast_deg,
        constant_decel_s: coast_deg
            .zip(speed_deg_s)
            .and_then(|(c, v)| (v > 0.0).then(|| 2.0 * c / v)),
        end,
        timed_out,
        pre: pre
            .into_iter()
            .filter(|s| s.t_send >= SPEED_WINDOW_S.mul_add(-2.0, t_stop) - 0.1)
            .collect(),
        trace,
    }
}

/// Goto each axis to its target and watch it land, then re-run a leg that
/// rests more than [`RETURN_TOLERANCE_TICKS`] out once. Every landing is
/// recorded in `out`, whatever happens after it.
async fn move_to(
    link: &mut Link,
    geometry: &Geometry,
    targets: &[(Axis, i32)],
    out: &mut Vec<Landing>,
) -> Result<(), ProbeError> {
    for _attempt in 0..2_u8 {
        let mut pending = Vec::new();
        for &(axis, target) in targets {
            let at = link.position(axis).await?.ticks;
            if at.abs_diff(target) <= RETURN_TOLERANCE_TICKS.unsigned_abs() {
                continue;
            }
            let (from, t_start) = goto(link, geometry, axis, target).await?;
            pending.push((axis, target, from, t_start));
        }
        if pending.is_empty() {
            break;
        }
        let travel_deg = pending
            .iter()
            .map(|&(axis, target, from, _)| {
                geometry
                    .axis(axis)
                    .degrees(target.saturating_sub(from))
                    .abs()
            })
            .fold(0.0_f64, f64::max);
        let budget = travel_deg / RETURN_MIN_RATE_DEG_S + RETURN_SLACK_S + HOLD_S;
        let axes: Vec<Axis> = pending.iter().map(|p| p.0).collect();
        let (traces, timed_out) = observe(link, geometry, &axes, seconds(budget)?).await?;
        for ((axis, target, from, t_start), trace) in pending.into_iter().zip(traces) {
            let direction = if target >= from { 1 } else { -1 };
            let end = motion_end(&trace, t_start, direction);
            out.push(Landing {
                axis,
                axis_label: label(axis),
                from,
                target,
                t_start,
                off_by_ticks: end.rest_ticks.and_then(|r| r.checked_sub(target)),
                end,
                timed_out,
                trace,
            });
        }
        if timed_out {
            return Err(ProbeError::Precondition(format!(
                "{axes:?} did not come to rest within {budget:.1} s of their goto; sent :L"
            )));
        }
    }
    for &(axis, target) in targets {
        let at = link.position(axis).await?.ticks;
        if at.abs_diff(target) > RETURN_ABORT_TICKS.unsigned_abs() {
            return Err(ProbeError::Precondition(format!(
                "{axis:?} came to rest at {at}, {} ticks from {target}",
                at.abs_diff(target)
            )));
        }
    }
    Ok(())
}

/// Run one trial into `out`, keeping whatever it measured even when a
/// later step fails.
async fn trial(
    link: &mut Link,
    geometry: &Geometry,
    args: &Args,
    plan: &TrialPlan,
    out: &mut Trial,
) -> Result<(), ProbeError> {
    link.set_tag(format!("{}/position", plan.tag));
    let origins: Vec<(Axis, i32)> = plan.legs.iter().map(|l| (l.axis, l.origin)).collect();
    move_to(link, geometry, &origins, &mut out.prepositions).await?;
    link.set_tag(plan.tag.clone());
    let mut started = Vec::new();
    for l in &plan.legs {
        let (from, _) = goto(link, geometry, l.axis, l.target).await?;
        started.push((*l, from));
    }
    let pre = run_up(link, geometry, &started, args.stop_after_deg).await?;
    let t_stops = send_stops(link, plan).await?;
    let axes: Vec<Axis> = plan.legs.iter().map(|l| l.axis).collect();
    let (traces, timed_out) = observe(link, geometry, &axes, STOP_OBSERVE_TIMEOUT).await?;
    for (((leg, pre), t_stop), trace) in started.into_iter().zip(pre).zip(t_stops).zip(traces) {
        out.stops.push(analyse_stop(
            leg,
            plan.kind,
            geometry.axis(leg.0.axis),
            t_stop,
            pre,
            trace,
            timed_out,
        ));
    }
    if timed_out {
        return Err(ProbeError::Precondition(format!(
            "{axes:?} did not come to rest within {STOP_OBSERVE_TIMEOUT:?} of the stop; sent :L"
        )));
    }
    link.set_tag(format!("{}/return", plan.tag));
    move_to(
        link,
        geometry,
        &[
            (Axis::Ra, geometry.ra.start),
            (Axis::Dec, geometry.dec.start),
        ],
        &mut out.landings,
    )
    .await
}

/// Send the trial's stop and return, per leg, when that leg's own stop
/// command went out. The sequence sends all three commands whatever the
/// replies say, as the service's own does, and fails afterwards on a
/// refusal.
async fn send_stops(link: &mut Link, plan: &TrialPlan) -> Result<Vec<f64>, ProbeError> {
    let ops: Vec<Op> = match plan.kind {
        StopKind::Sequence => vec![Op::Halt(Axis::Ra), Op::Halt(Axis::Dec), Op::Stop(Axis::Ra)],
        StopKind::Instant => plan.legs.iter().map(|l| Op::Halt(l.axis)).collect(),
        StopKind::Decelerate => plan.legs.iter().map(|l| Op::Stop(l.axis)).collect(),
    };
    let mut sent = Vec::new();
    for op in ops {
        sent.push((op, link.ask(op).await));
    }
    let mut t_stops = Vec::new();
    for l in &plan.legs {
        let first = sent.iter().find(|(op, _)| match op {
            Op::Halt(a) | Op::Stop(a) => *a == l.axis,
            _ => false,
        });
        match first {
            Some((_, Ok(ex))) => t_stops.push(ex.t_send),
            _ => {
                return Err(ProbeError::Precondition(format!(
                    "no stop reached {:?}",
                    l.axis
                )))
            }
        }
    }
    for (op, result) in sent {
        let ex = result?;
        if let Err(reply) = &ex.reply {
            return Err(ProbeError::Mount {
                cmd: format!("{op:?}"),
                reply: format!("{} ({reply})", ex.raw),
            });
        }
    }
    Ok(t_stops)
}

async fn run(link: &mut Link, args: &Args, summary: &mut Summary) -> Result<(), ProbeError> {
    let (identity, geometry) = preflight(link).await?;
    summary.identity = Some(identity);
    summary.geometry = Some(geometry);
    let trials = plan(&geometry, args)?;
    say(&format!("plan: {} trials", trials.len()));
    for p in trials {
        let mut t = Trial {
            plan: p.clone(),
            prepositions: Vec::new(),
            stops: Vec::new(),
            landings: Vec::new(),
        };
        let result = trial(link, &geometry, args, &p, &mut t).await;
        print_trial(&t);
        summary.trials.push(t);
        result?;
    }
    Ok(())
}

/// `:L1 :L2 :K1`, then wait for both axes to report stopped, repeating
/// the halt once if either does not. Runs on every exit path.
async fn final_stop(link: &mut Link) -> String {
    link.set_tag("final-stop");
    let mut report = Vec::new();
    for round in 0..2_u8 {
        for op in [Op::Halt(Axis::Ra), Op::Halt(Axis::Dec), Op::Stop(Axis::Ra)] {
            match link.ask(op).await {
                Ok(ex) => report.push(format!("{op:?} -> {}", ex.raw)),
                Err(e) => report.push(format!("{op:?} failed: {e}")),
            }
        }
        let mut stopped = true;
        for axis in [Axis::Ra, Axis::Dec] {
            let ok = matches!(link.wait_stopped(axis, FINAL_STOP_TIMEOUT).await, Ok(true));
            report.push(format!("{axis:?} stopped: {ok} (round {round})"));
            stopped &= ok;
        }
        if stopped {
            report.push("both axes stopped".into());
            break;
        }
    }
    report.join("; ")
}

/// Print a line, ignoring a stdout that has gone away: a write error must
/// never end a run mid-motion.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn fmt_opt(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "-".into(), |v| format!("{v:.digits$}"))
}

fn fmt_ticks(value: Option<i32>) -> String {
    value.map_or_else(|| "-".into(), |v| v.to_string())
}

fn fmt_end(end: &MotionEnd) -> String {
    format!(
        ":f clear {} s | rest {} s | after clear fwd {} rev {} net {} | blocked {}",
        end.running_clear_s
            .map_or_else(|| "-".into(), |(a, b)| format!("{a:.3}..{b:.3}")),
        fmt_opt(end.settle_s, 3),
        fmt_ticks(end.after_clear_forward_ticks),
        fmt_ticks(end.after_clear_reverse_ticks),
        fmt_ticks(end.after_clear_net_ticks),
        end.blocked_seen,
    )
}

fn print_trial(t: &Trial) {
    for s in &t.stops {
        say(&format!(
            "{:<7} {:<3} {:<10} dir {:+} | v0 {} °/s cruise {} | coast {} ° ({} t) | 2d/v {} s | {}{}",
            t.plan.tag,
            format!("{:?}", s.leg.axis),
            format!("{:?}", s.kind),
            s.leg.direction,
            fmt_opt(s.speed_deg_s, 3),
            s.cruise,
            fmt_opt(s.coast_deg, 3),
            fmt_ticks(s.coast_ticks),
            fmt_opt(s.constant_decel_s, 3),
            fmt_end(&s.end),
            if s.timed_out { " | TIMED OUT" } else { "" },
        ));
    }
    for l in t.prepositions.iter().chain(&t.landings) {
        say(&format!(
            "{:<7} {:<3} landing {} -> {} | off {} | {}{}",
            t.plan.tag,
            format!("{:?}", l.axis),
            l.from,
            l.target,
            fmt_ticks(l.off_by_ticks),
            fmt_end(&l.end),
            if l.timed_out { " | TIMED OUT" } else { "" },
        ));
    }
}

fn percentile(sorted: &[f64], pct: usize) -> Option<f64> {
    let len = sorted.len();
    let idx = len.checked_sub(1)?.checked_mul(pct)?.checked_div(100)?;
    sorted.get(idx).copied()
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
            position_ticks: PARK3_RA_TICKS,
            initialized: true,
            ..AxisSimState::default()
        };
        state.dec = AxisSimState {
            position_ticks: PARK3_DEC_TICKS,
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
/// every one of them ends with the final stop. Installed before the port
/// opens; later signals repeat the notice and change nothing.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "only the cfg(unix) signal registration can fail; one signature serves both cfgs"
    )
)]
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
            let _ = writeln!(std::io::stderr(), "probe failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(true)` when every trial completed, the mount came back to
/// its start pose and both axes confirmed stopped.
async fn probe(args: &Args) -> Result<bool, ProbeError> {
    std::fs::create_dir_all(&args.out).map_err(|e| ProbeError::Output(e.to_string()))?;
    let log = File::create_new(args.out.join("exchanges.jsonl")).map_err(|e| {
        ProbeError::Output(format!(
            "{}: {e} (use a fresh --out for every run)",
            args.out.display()
        ))
    })?;
    let abort = Arc::new(AtomicBool::new(false));
    spawn_signal_watch(&abort)?;
    let transport = open_transport(args).await?;
    let mut link = Link {
        transport,
        epoch: Instant::now(),
        log: BufWriter::new(log),
        tag: String::new(),
        armed_ra: false,
        armed_dec: false,
        rtts: Vec::new(),
        abort,
    };
    let mut summary = Summary {
        args: format!("{args:?}"),
        ..Summary::default()
    };
    let result = run(&mut link, args, &mut summary).await;
    // A run that failed part-way leaves the axes wherever its last trial
    // put them. Unless a signal asked for a plain stop, halt both, wait
    // for the counts to come to rest (a relative goto from a count still
    // moving would land off), and bring them home.
    if result.is_err() && !link.aborted() {
        if let Some(geometry) = summary.geometry {
            link.set_tag("final-return");
            link.halt_all(&[Axis::Ra, Axis::Dec], None).await;
            let home = match observe(
                &mut link,
                &geometry,
                &[Axis::Ra, Axis::Dec],
                STOP_OBSERVE_TIMEOUT,
            )
            .await
            {
                Ok((_, false)) => {
                    move_to(
                        &mut link,
                        &geometry,
                        &[
                            (Axis::Ra, geometry.ra.start),
                            (Axis::Dec, geometry.dec.start),
                        ],
                        &mut summary.final_return,
                    )
                    .await
                }
                Ok((_, true)) => Err(ProbeError::Precondition(
                    "the axes did not come to rest; not returning".into(),
                )),
                Err(e) => Err(e),
            };
            summary.final_return_error = home.err().map(|e| e.to_string());
        }
    }
    summary.stop_report = final_stop(&mut link).await;
    let _ = link.log.flush();
    summary.run_error = result.err().map(|e| e.to_string());
    let mut rtts = link.rtts.clone();
    rtts.sort_by(f64::total_cmp);
    summary.rtt_p50_s = percentile(&rtts, 50);
    summary.rtt_p99_s = percentile(&rtts, 99);
    summary.rtt_max_s = rtts.last().copied();
    if let Some(e) = &summary.run_error {
        say(&format!("RUN ERROR: {e}"));
    }
    for l in &summary.final_return {
        say(&format!(
            "final return {:?} {} -> {} off {}",
            l.axis,
            l.from,
            l.target,
            fmt_ticks(l.off_by_ticks)
        ));
    }
    if let Some(e) = &summary.final_return_error {
        say(&format!("final return failed: {e}"));
    }
    say(&format!("final stop: {}", summary.stop_report));
    say(&format!(
        "rtt p50 {} s, p99 {} s, max {} s",
        fmt_opt(summary.rtt_p50_s, 4),
        fmt_opt(summary.rtt_p99_s, 4),
        fmt_opt(summary.rtt_max_s, 4)
    ));
    let ok = summary.run_error.is_none() && summary.stop_report.contains("both axes stopped");
    let json = serde_json::to_string(&summary).map_err(|e| ProbeError::Output(e.to_string()))?;
    std::fs::write(args.out.join("summary.json"), json)
        .map_err(|e| ProbeError::Output(e.to_string()))?;
    Ok(ok)
}

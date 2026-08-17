//! Feature-gated in-memory mock transport for the Sky-Watcher protocol.
//!
//! Simulates the motor controller as a small state machine: accepts
//! `:cmd<axis><payload>\r` frames, maintains per-axis state (position,
//! motion mode, running flag, initialised flag, tracking), and emits
//! well-formed `=...\r` / `!XX\r` replies. Plugs into the shared
//! transport via [`MockTransportFactory`] (and [`CapturingMockFactory`]
//! for tests that need a long-lived state handle), implementing the
//! shared crate's [`TransportFactory`] / [`FrameTransport`] traits in
//! place of the legacy `Transport`-based mock.
//!
//! The mock is deliberately not exposed unless the `mock` feature is on
//! so a production build cannot accidentally pick it up.

// `#[cfg(feature = "mock")]`-gated test-helper infrastructure that never
// ships in production builds. Excluded from coverage so the workspace
// coverage number reflects only production-shipped code — counting these
// never-shipped mock lines would produce false coverage figures.
#![allow(clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use skywatcher_motor_protocol::codec::{
    decode_position, decode_u24, decode_u8, encode_position, encode_u24,
};
use skywatcher_motor_protocol::{Direction, ModeKind, Speed};
use tokio::sync::Mutex;

/// Per-axis simulator state.
#[derive(Debug, Clone, Copy)]
pub struct AxisSimState {
    pub position_ticks: i32,
    pub initialized: bool,
    pub running: bool,
    pub mode: ModeKind,
    /// Step direction. Matches the `:G` DB2 bit-0 and the `:f`
    /// nibble-0 bit-1 in the Sky-Watcher spec. The driver translates
    /// `sign(target - current)` into a direction at slew-issue time;
    /// the mock advances the encoder accordingly.
    pub direction: Direction,
    pub speed: Speed,
    /// Sky-Watcher spec §5 (Response E nibble-1 bit-1): the firmware
    /// reports `Blocked` when the motor is stepping but the encoder
    /// isn't advancing. Tests seed this directly to exercise the
    /// slew/park watchers' blocked-abort path; the mock does not
    /// derive it from physical state.
    pub blocked: bool,
    pub goto_target_ticks: i32,
    pub step_period: u32,
}

impl Default for AxisSimState {
    fn default() -> Self {
        Self {
            position_ticks: 0,
            initialized: false,
            running: false,
            mode: ModeKind::Tracking,
            direction: Direction::Cw,
            speed: Speed::Slow,
            blocked: false,
            goto_target_ticks: 0,
            step_period: 0,
        }
    }
}

impl AxisSimState {
    /// Pack the running / mode / direction / init flags into the three
    /// hex-digit payload that `:f<axis>` returns. Bit layout matches
    /// the Sky-Watcher spec §5 (Response E) — see
    /// [`skywatcher_motor_protocol::AxisStatus`].
    fn encode_status(self) -> [u8; 3] {
        let mut n0 = 0u8;
        if self.mode == ModeKind::Tracking {
            n0 |= 0x1; // bit 0: 1 = Tracking, 0 = Goto
        }
        if self.direction == Direction::Ccw {
            n0 |= 0x2; // bit 1: 1 = CCW, 0 = CW
        }
        if self.speed == Speed::Fast {
            n0 |= 0x4; // bit 2: 1 = Fast, 0 = Slow
        }
        let mut n1 = 0u8;
        if self.running {
            n1 |= 0x1; // bit 0: 1 = Running
        }
        if self.blocked {
            n1 |= 0x2; // bit 1: 1 = Blocked
        }
        let n2 = u8::from(self.initialized);
        [nibble_to_hex(n0), nibble_to_hex(n1), nibble_to_hex(n2)]
    }

    /// Advance position by one polling step.
    ///
    /// In **goto mode** the axis walks toward
    /// `goto_target_ticks` at a high-speed chunk and stops `running`
    /// once it arrives. In **tracking mode** the axis
    /// steps forever in the configured direction at a small per-poll
    /// chunk — this is the sidereal-tracking analogue: on real
    /// hardware the encoder advances continuously while tracking, so
    /// the resulting `RightAscension` reading stays constant after a
    /// slew completes. Without this, post-slew `RA` reads drift at
    /// sidereal rate (one of the issues `ConformU`'s slew tests flag).
    ///
    /// The tracking-mode chunk is chosen to approximate one sidereal
    /// "step" per `:j` poll given the default polling cadence
    /// (200 ms) and CPR (3.6 M): sidereal rate ≈ 42 ticks/s, so
    /// ~8 ticks per 200 ms poll. Picking 8 directly keeps things
    /// simple and predictable across tests that override the polling
    /// interval.
    fn advance_one_step(&mut self) {
        if !self.running {
            return;
        }
        // Direction comes from the wire-level direction bit decoded
        // out of the last `:G`, NOT from `sign(target - position)`.
        // Real hardware steps in whatever direction the mode byte
        // told it to, regardless of where the target sits relative
        // to the current encoder — if the driver tells the motor
        // to go CW while the target is CCW of the current
        // position, the hardware happily steps CW (and either
        // overshoots and never stops, or hits a mechanical limit).
        // Faithful-mock matters here: if the driver issues a
        // direction-vs-delta mismatch we want the BDD suite to
        // catch it rather than silently auto-correct.
        let dir: i32 = if self.direction == Direction::Ccw {
            -1
        } else {
            1
        };
        if self.mode == ModeKind::Goto {
            let chunk: i32 = if self.speed == Speed::Fast {
                100_000
            } else {
                100
            };
            let delta = self.goto_target_ticks.saturating_sub(self.position_ticks);
            if delta == 0 {
                self.running = false;
                return;
            }
            // Step in the *commanded* direction, capped by the
            // remaining distance only when the commanded direction
            // moves us toward the target.
            let toward_target = delta.signum() == dir;
            let step = if toward_target {
                chunk.min(delta.saturating_abs()).saturating_mul(dir)
            } else {
                chunk.saturating_mul(dir)
            };
            self.position_ticks = clamp_to_wire_range(self.position_ticks.saturating_add(step));
            if toward_target && self.position_ticks == self.goto_target_ticks {
                self.running = false;
            }
        } else {
            // Tracking mode: free-run in the configured direction at
            // a sidereal-ish chunk per poll. Never auto-stop — only
            // `:K` / `:L` should clear `running`. Real GTi firmware
            // saturates at the 24-bit encoder limit too, so a
            // long-running tracking mock matches hardware behaviour.
            const SIDEREAL_CHUNK_PER_POLL: i32 = 8;
            self.position_ticks = clamp_to_wire_range(
                self.position_ticks
                    .saturating_add(SIDEREAL_CHUNK_PER_POLL.saturating_mul(dir)),
            );
        }
    }
}

/// Saturating-clamp an encoder-tick value to the wire-representable
/// signed-24-bit range. Used by [`AxisSimState::advance_one_step`] so
/// a long-running tracking mock can't drift past `POSITION_MAX` and
/// panic the next `:j` handler when `encode_position` rejects the
/// out-of-range value. Real `GTi` firmware saturates here too.
fn clamp_to_wire_range(ticks: i32) -> i32 {
    use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};
    ticks.clamp(POSITION_MIN, POSITION_MAX)
}

fn nibble_to_hex(n: u8) -> u8 {
    let n = n & 0x0F;
    match n {
        // The mask bounds `n` at 15, so neither sum leaves ASCII.
        0..=9 => b'0'.saturating_add(n),
        _ => b'A'.saturating_add(n.saturating_sub(10)),
    }
}

/// In-memory mock state machine. Lives behind an `Arc<Mutex<…>>` and
/// is shared between the [`MockTransportFactory`] (which clones the
/// `Arc` into each opened `FrameTransport`) and the test handle that
/// pre-seeds or introspects state.
#[derive(Debug)]
pub struct MockMountState {
    pub ra: AxisSimState,
    pub dec: AxisSimState,
    /// Counts per revolution on the RA axis. Defaults to the `GTi` value
    /// `0x375F00` (3,628,800); tests can override.
    pub cpr_ra: u32,
    /// Counts per revolution on the Dec axis. Defaults to the `GTi` value
    /// `0x2C4C00` (2,903,040 — hardware-measured `:a2` reply `=004C2C`;
    /// deliberately **different** from the RA CPR so any code path that
    /// uses the wrong axis' CPR in a conversion fails tests loudly
    /// instead of passing on identical values); tests can override.
    pub cpr_dec: u32,
    /// Timer-interrupt frequency. Defaults to the `GTi` value `0xF42400`
    /// (≈ 16 MHz).
    pub tmr_freq: u32,
    pub high_speed_ratio_ra: u32,
    pub high_speed_ratio_dec: u32,
    /// Motor-board version. Defaults to `0x000C_3003` — the decode of the
    /// `GTi` probe table's wire reply `=03300C\r` (mount-type byte `0x03` in
    /// the low byte, fw `0x30`/`0x0C` above it).
    pub motor_board_version: u32,
    /// Every command frame received, in arrival order. Tests assert against
    /// this to verify the driver issued the expected wire commands.
    pub command_log: Vec<Vec<u8>>,
    /// Test-only fault injection. When `Some(letter)`, any command whose
    /// letter matches replies with a mount error (`!XX`) instead of its
    /// normal response, so the driver's send path returns `Err`. Used to
    /// exercise wire-failure branches (e.g. the tracking guard's
    /// `:K1`-failed path). The request is still recorded in `command_log`.
    pub fail_command: Option<u8>,
    /// Pending replies the next `recv_frame` call should drain. Every
    /// processed command appends one frame; the [`FrameTransport`] impl
    /// pulls from the front to deliver replies in order.
    pending_replies: std::collections::VecDeque<Vec<u8>>,
}

impl Default for MockMountState {
    fn default() -> Self {
        // Matches the GTi probe table in
        // `docs/references/skywatcher-motor-controller-command-set.md`.
        Self {
            ra: AxisSimState::default(),
            dec: AxisSimState::default(),
            cpr_ra: 0x0037_5F00,
            cpr_dec: 0x002C_4C00,
            tmr_freq: 0x00F4_2400,
            // High-speed ratio is mount-specific and the design doc lists
            // example values (16/32/64) without naming a default. Pick a
            // common one; tests that care will override.
            high_speed_ratio_ra: 32,
            high_speed_ratio_dec: 32,
            motor_board_version: 0x000C_3003,
            command_log: Vec::new(),
            fail_command: None,
            pending_replies: std::collections::VecDeque::new(),
        }
    }
}

impl MockMountState {
    const fn axis_mut(&mut self, axis: u8) -> Option<&mut AxisSimState> {
        match axis {
            b'1' => Some(&mut self.ra),
            b'2' => Some(&mut self.dec),
            _ => None,
        }
    }

    const fn cpr(&self, axis: u8) -> Option<u32> {
        match axis {
            b'1' => Some(self.cpr_ra),
            b'2' => Some(self.cpr_dec),
            _ => None,
        }
    }

    const fn high_speed_ratio(&self, axis: u8) -> Option<u32> {
        match axis {
            b'1' => Some(self.high_speed_ratio_ra),
            b'2' => Some(self.high_speed_ratio_dec),
            _ => None,
        }
    }

    /// Apply a `:cmd<axis><payload?>\r` request frame to the simulator,
    /// updating state and pushing the reply onto [`pending_replies`].
    fn process_command(&mut self, request: &[u8]) {
        self.command_log.push(request.to_vec());
        debug_assert!(request.len() >= 3, "send_frame admits only :...\\r frames");
        let (Some(&cmd), Some(&axis)) = (request.get(1), request.get(2)) else {
            self.pending_replies.push_back(err_reply(0));
            return;
        };
        // A 3-byte frame has no payload: `3..2` is `None` here, where the
        // old slicing panicked on the backwards range.
        let payload = request
            .get(3..request.len().saturating_sub(1))
            .unwrap_or_default();

        // Test-only fault injection: reply with a mount error for the
        // selected command letter so the driver's send path returns
        // `Err` (see `MockMountState::fail_command`).
        if self.fail_command == Some(cmd) {
            self.pending_replies.push_back(err_reply(0));
            return;
        }

        // Inquiries (lowercase letters)
        let reply = match cmd {
            b'a' => {
                // CPR per axis (24-bit unsigned)
                self.cpr(axis)
                    .map_or_else(|| err_reply(0), |cpr| ack_with(&encode_u24(cpr)))
            }
            b'b' => {
                // TMR_Freq, axis 1 only.
                if axis == b'1' {
                    ack_with(&encode_u24(self.tmr_freq))
                } else {
                    err_reply(0)
                }
            }
            b'g' => self
                .high_speed_ratio(axis)
                .map_or_else(|| err_reply(0), |hsr| ack_with(&encode_u24(hsr))),
            b'e' => {
                // Motor board version, returned for either axis.
                if axis == b'1' || axis == b'2' {
                    ack_with(&encode_u24(self.motor_board_version))
                } else {
                    err_reply(0)
                }
            }
            b'j' => {
                if let Some(ax) = self.axis_mut(axis) {
                    // Polling-driven motion: every `:j` advances one step.
                    ax.advance_one_step();
                    let pos = ax.position_ticks;
                    ack_with(&encode_position(pos).expect("position in range"))
                } else {
                    err_reply(0)
                }
            }
            b'f' => {
                // `:f` is a status-read; it must NOT advance motion, or
                // tests that pre-seed `running=true` see the simulator
                // immediately clear it on the first poll.
                self.axis_mut(axis)
                    .map_or_else(|| err_reply(0), |ax| ack_with(&ax.encode_status()))
            }
            // Setters (uppercase letters)
            b'F' => {
                if let Some(ax) = self.axis_mut(axis) {
                    ax.initialized = true;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'G' => {
                // Set motion mode: payload is two hex digits. Per the
                // Sky-Watcher spec §5 each digit is an independent
                // nibble — DB1 (high nibble of the byte, mode info)
                // and DB2 (low nibble, direction / variant). See
                // `skywatcher_motor_protocol::MotionMode`.
                let bytes: [u8; 2] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                let Ok(mode_byte) = decode_u8(bytes) else {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                };
                let db1 = (mode_byte >> 4) & 0x0F;
                let db2 = mode_byte & 0x0F;
                if let Some(ax) = self.axis_mut(axis) {
                    // DB1 bit 0: 1=Tracking, 0=Goto.
                    ax.mode = if (db1 & 0x1) == 0 {
                        ModeKind::Goto
                    } else {
                        ModeKind::Tracking
                    };
                    // DB1 bit 1: speed selector — meaning inverts
                    // between Goto and Tracking modes per spec.
                    let bit1 = (db1 & 0x2) != 0;
                    let fast = if ax.mode == ModeKind::Goto {
                        // Goto: 0 = Fast, 1 = Slow
                        !bit1
                    } else {
                        // Tracking: 0 = Slow, 1 = Fast
                        bit1
                    };
                    ax.speed = if fast { Speed::Fast } else { Speed::Slow };
                    // DB2 bit 0: 0 = CW, 1 = CCW.
                    ax.direction = if (db2 & 0x1) == 0 {
                        Direction::Cw
                    } else {
                        Direction::Ccw
                    };
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'S' => {
                // Set goto target absolute: 6-byte signed/biased payload.
                let bytes: &[u8; 6] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                let Ok(ticks) = decode_position(bytes) else {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                };
                if let Some(ax) = self.axis_mut(axis) {
                    ax.goto_target_ticks = ticks;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'H' => {
                // Set goto target by *increment*: 6-byte u24 magnitude.
                // Direction comes from what the previous `:G` left on
                // the axis. The mount computes the absolute target by
                // adding the signed delta to the current encoder
                // position.
                let bytes: &[u8; 6] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                let Ok(increment) = decode_u24(bytes) else {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                };
                if let Some(ax) = self.axis_mut(axis) {
                    let sign: i32 = if ax.direction == Direction::Ccw {
                        -1
                    } else {
                        1
                    };
                    ax.goto_target_ticks = ax
                        .position_ticks
                        .saturating_add(sign.saturating_mul(increment.cast_signed()));
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'M' => {
                // Set breakpoint increment: 6-byte u24 magnitude. The
                // firmware uses it to schedule deceleration; the mock
                // accepts and ignores the value — running through
                // `:j` polling already lands on `goto_target_ticks`
                // without overshoot.
                let bytes: &[u8; 6] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                if decode_u24(bytes).is_err() {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                }
                if axis == b'1' || axis == b'2' {
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'I' => {
                // Set step period: 6-byte u24 payload.
                let bytes: &[u8; 6] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                let Ok(period) = decode_u24(bytes) else {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                };
                if let Some(ax) = self.axis_mut(axis) {
                    ax.step_period = period;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'E' => {
                // Sync: write encoder position. 6-byte signed/biased payload.
                let bytes: &[u8; 6] = if let Ok(b) = payload.try_into() {
                    b
                } else {
                    self.pending_replies.push_back(err_reply(1));
                    return;
                };
                let Ok(ticks) = decode_position(bytes) else {
                    self.pending_replies.push_back(err_reply(3));
                    return;
                };
                if let Some(ax) = self.axis_mut(axis) {
                    ax.position_ticks = ticks;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            b'J' => {
                if let Some(ax) = self.axis_mut(axis) {
                    if !ax.initialized {
                        self.pending_replies.push_back(err_reply(4));
                        return;
                    }
                    ax.running = true;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            // `:K` (stop) and `:L` (instant stop) differ only in
            // deceleration on real hardware; the mock stops instantly
            // either way.
            b'K' | b'L' => {
                if let Some(ax) = self.axis_mut(axis) {
                    ax.running = false;
                    ack_with(&[])
                } else {
                    err_reply(0)
                }
            }
            _ => err_reply(0), // UnknownCommand
        };

        self.pending_replies.push_back(reply);
    }
}

/// Build an `=<payload>\r` success reply. Empty payload → `=\r`.
fn ack_with(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len().saturating_add(2));
    out.push(b'=');
    out.extend_from_slice(payload);
    out.push(b'\r');
    out
}

/// Build an `!XX\r` mount-error reply.
fn err_reply(code: u8) -> Vec<u8> {
    use skywatcher_motor_protocol::codec::encode_u8;
    let bytes = encode_u8(code);
    vec![b'!', bytes[0], bytes[1], b'\r']
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings survive a reconnect cycle.
struct MockFrameTransport {
    state: Arc<Mutex<MockMountState>>,
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if bytes.len() < 3 || bytes.first() != Some(&b':') || bytes.last() != Some(&b'\r') {
            return Err(TransportError::Framing(format!(
                "mock received malformed request frame: {bytes:?}"
            )));
        }
        self.state.lock().await.process_command(bytes);
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        let frame = self
            .state
            .lock()
            .await
            .pending_replies
            .pop_front()
            .ok_or(TransportError::Eof)?;
        buf.clear();
        buf.extend_from_slice(&frame);
        Ok(())
    }
}

/// [`TransportFactory`] that emits a fresh [`FrameTransport`] backed by
/// its own [`MockMountState`] on every open. Each new connection gets a
/// brand-new state machine — matches the BDD harness's expectation that
/// a server restart equals a power cycle.
#[derive(Debug, Default)]
pub struct MockTransportFactory;

#[async_trait]
impl TransportFactory for MockTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        Ok(Box::new(MockFrameTransport {
            state: Arc::new(Mutex::new(MockMountState::default())),
        }))
    }
}

/// [`TransportFactory`] that returns a fresh [`FrameTransport`] backed
/// by a shared [`MockMountState`] on every open call. The test holds
/// the original `Arc<Mutex<MockMountState>>` and can introspect /
/// pre-seed the same state the driver mutates through the transport.
///
/// Used by the unit tests that need to assert on the exact wire frames
/// the driver emitted (e.g. "tracking issues `:G1` then `:I1` then
/// `:J1` in that order").
#[derive(Debug, Clone, Default)]
pub struct CapturingMockFactory {
    pub state: Arc<Mutex<MockMountState>>,
}

impl CapturingMockFactory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TransportFactory for CapturingMockFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;
    use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};

    async fn open(factory: &MockTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    #[test]
    fn advance_one_step_clamps_at_wire_range_in_tracking_mode() {
        // Regression test: a long-running tracking mock used to panic
        // on the next `:j` poll once `position_ticks` drifted past
        // `POSITION_MAX`, because `encode_position` rejects
        // out-of-range values. The fix saturates the position at the
        // 24-bit signed encoder boundary inside `advance_one_step`
        // itself, matching how real GTi firmware behaves.
        let mut s = AxisSimState {
            running: true,
            mode: ModeKind::Tracking,
            direction: Direction::Cw,
            position_ticks: POSITION_MAX - 4,
            ..Default::default()
        };
        s.advance_one_step();
        assert_eq!(s.position_ticks, POSITION_MAX);
        s.advance_one_step();
        s.advance_one_step();
        assert_eq!(s.position_ticks, POSITION_MAX);

        let mut s = AxisSimState {
            running: true,
            mode: ModeKind::Tracking,
            direction: Direction::Ccw,
            position_ticks: POSITION_MIN + 4,
            ..Default::default()
        };
        s.advance_one_step();
        assert_eq!(s.position_ticks, POSITION_MIN);
        s.advance_one_step();
        assert_eq!(s.position_ticks, POSITION_MIN);
    }

    #[test]
    fn axis_sim_state_default_is_at_home_uninitialised_and_stopped() {
        let s = AxisSimState::default();
        assert_eq!(s.position_ticks, 0);
        assert!(!s.initialized);
        assert!(!s.running);
        assert_eq!(s.mode, ModeKind::Tracking);
        assert_eq!(s.direction, Direction::Cw);
        assert_eq!(s.speed, Speed::Slow);
        assert_eq!(s.goto_target_ticks, 0);
        assert_eq!(s.step_period, 0);
    }

    #[test]
    fn mock_mount_state_default_seeds_documented_gti_values() {
        let s = MockMountState::default();
        assert_eq!(s.cpr_ra, 0x0037_5F00);
        // The dec axis has its own, hardware-measured CPR — deliberately
        // different from RA so cross-axis CPR mix-ups fail tests.
        assert_eq!(s.cpr_dec, 0x002C_4C00);
        assert_eq!(s.tmr_freq, 0x00F4_2400);
        assert_eq!(s.motor_board_version, 0x000C_3003);
        assert_eq!(s.high_speed_ratio_ra, 32);
        assert_eq!(s.high_speed_ratio_dec, 32);
    }

    async fn round_trip(t: &mut Box<dyn FrameTransport>, req: &[u8]) -> Vec<u8> {
        t.send_frame(req).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn round_trip_initialize_acks_and_marks_axis_initialized() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let mut t = factory.open().await.unwrap();
        let reply = round_trip(&mut t, b":F1\r").await;
        assert_eq!(reply, b"=\r");
        assert!(state.lock().await.ra.initialized);
    }

    #[tokio::test]
    async fn round_trip_inquire_cpr_returns_seeded_value() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":a1\r").await;
        // GTi RA CPR 0x375F00 → encode_u24 → "005F37"
        assert_eq!(reply, b"=005F37\r");
        // Dec CPR 0x2C4C00 → "004C2C" — matches the hardware-captured
        // `:a2` reply in the probe table (the axes differ on the GTi).
        let reply = round_trip(&mut t, b":a2\r").await;
        assert_eq!(reply, b"=004C2C\r");
    }

    #[tokio::test]
    async fn round_trip_inquire_position_returns_biased_value() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        // Initial position 0 → bias 0x800000 → "000080"
        let reply = round_trip(&mut t, b":j1\r").await;
        assert_eq!(reply, b"=000080\r");
    }

    #[tokio::test]
    async fn round_trip_set_motion_mode_then_status_reflects_it() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let mut t = factory.open().await.unwrap();
        round_trip(&mut t, b":F1\r").await;
        let reply = round_trip(&mut t, b":G100\r").await;
        assert_eq!(reply, b"=\r");
        let s = state.lock().await;
        assert_eq!(s.ra.mode, ModeKind::Goto);
        assert_eq!(s.ra.speed, Speed::Fast);
        assert_eq!(s.ra.direction, Direction::Cw);
        assert!(s.ra.initialized);
    }

    #[tokio::test]
    async fn start_motion_before_initialize_returns_not_initialized() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":J1\r").await;
        assert_eq!(reply, b"!04\r");
    }

    #[tokio::test]
    async fn slew_lifecycle_advances_position_to_target_then_stops() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let mut t = factory.open().await.unwrap();
        round_trip(&mut t, b":F1\r").await;
        round_trip(&mut t, b":G120\r").await;
        round_trip(&mut t, b":S1C80080\r").await;
        round_trip(&mut t, b":J1\r").await;
        round_trip(&mut t, b":j1\r").await;
        round_trip(&mut t, b":j1\r").await;
        let s = state.lock().await;
        assert_eq!(s.ra.position_ticks, 200);
        assert!(!s.ra.running);
    }

    #[tokio::test]
    async fn capturing_factory_logs_every_request() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let mut t = factory.open().await.unwrap();
        round_trip(&mut t, b":F1\r").await;
        round_trip(&mut t, b":F2\r").await;
        let log = &state.lock().await.command_log;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], b":F1\r");
        assert_eq!(log[1], b":F2\r");
    }

    #[tokio::test]
    async fn send_frame_rejects_malformed_request() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let err = t.send_frame(b"F1\r").await.unwrap_err();
        assert!(matches!(err, TransportError::Framing(_)));
    }

    /// Test matrix for the per-command `CommandLengthError` (`!01\r`) +
    /// `InvalidCharacter` (`!03\r`) branches in `process_command`. Real
    /// drivers never emit malformed payloads, so these branches are
    /// only reachable from direct `send_frame` calls with bogus
    /// frames. Exercising them here keeps the mock's defensive
    /// per-command parser logic from rotting silently.
    #[tokio::test]
    async fn set_motion_mode_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        // `:G1\r` is missing the 2-hex mode byte → CommandLengthError.
        let reply = round_trip(&mut t, b":G1\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_motion_mode_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        // Two-byte payload but neither is a hex digit → InvalidCharacter.
        let reply = round_trip(&mut t, b":G1ZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn set_goto_target_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        // `:S1ABC\r` is 3 hex chars; needs 6 → CommandLengthError.
        let reply = round_trip(&mut t, b":S1ABC\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_goto_target_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":S1ZZZZZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn set_goto_increment_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":H1ABC\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_goto_increment_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":H1ZZZZZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn set_breakpoint_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":M1ABC\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_breakpoint_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":M1ZZZZZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn set_step_period_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":I1ABC\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_step_period_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":I1ZZZZZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn set_position_rejects_short_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":E1ABC\r").await;
        assert_eq!(reply, b"!01\r");
    }

    #[tokio::test]
    async fn set_position_rejects_non_hex_payload() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":E1ZZZZZZ\r").await;
        assert_eq!(reply, b"!03\r");
    }

    #[tokio::test]
    async fn unknown_command_letter_returns_unknown_command_error() {
        let factory = MockTransportFactory;
        let mut t = open(&factory).await;
        let reply = round_trip(&mut t, b":Z1\r").await;
        assert_eq!(reply, b"!00\r");
    }

    #[tokio::test]
    async fn capturing_factory_shares_state_across_opens() {
        // Two opens on the same CapturingMockFactory must both see the
        // same MockMountState — mutations from the first transport
        // become visible to the second open's reads.
        let factory = CapturingMockFactory::new();
        {
            let mut t = factory.open().await.unwrap();
            round_trip(&mut t, b":F1\r").await;
        }
        let state = Arc::clone(&factory.state);
        assert!(state.lock().await.ra.initialized);
        let mut t2 = factory.open().await.unwrap();
        // `:F1` was already issued; ask `:f1` and we should see the
        // initialized bit set in nibble 2 of the status payload.
        // Default-after-`:F1` layout: n0=1 (Tracking, goto=false →
        // bit-0 set), n1=0 (not running), n2=1 (initialized) →
        // `=101\r`.
        let reply = round_trip(&mut t2, b":f1\r").await;
        assert_eq!(reply, b"=101\r");
    }
}

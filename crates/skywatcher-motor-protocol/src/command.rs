//! Outbound commands.
//!
//! The wire form is `: <cmd> <axis> <payload?> \r` where `<cmd>` is a single
//! ASCII letter (uppercase = setter / motion; lowercase = inquiry), `<axis>`
//! is `'1'`, `'2'`, or `'3'`, and `<payload>` is 0..=6 ASCII hex bytes.

use crate::codec::{encode_position, encode_u24, nibble_to_hex};
use crate::error::Result;

/// Which physical axis a command targets.
///
/// The wire byte is `'1'` (RA / Az motor), `'2'` (Dec / Alt motor), or `'3'`
/// (both axes). `Both` is only valid for a subset of commands; the codec does
/// not police this — the caller is responsible for not asking for impossible
/// combinations.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Axis {
    Ra,
    Dec,
    Both,
}

impl Axis {
    const fn wire_byte(self) -> u8 {
        match self {
            Self::Ra => b'1',
            Self::Dec => b'2',
            Self::Both => b'3',
        }
    }
}

/// Motion-mode flags for the `:G` command.
///
/// Per the Sky-Watcher motor-controller spec §5, the `:G` payload is **two
/// independent hex nibbles** (DB1 then DB2 on the wire — high nibble first
/// when read as a single byte). Each nibble has its own bit definitions.
/// This was the source of [the original codec's worst hardware bug][bug]:
/// it treated the payload as a flat 8-bit bitfield with `goto = 0x10`,
/// `fast = 0x20`, `reverse = 0x01`, which by coincidence produced wire
/// bytes (`0x30` = "30") that the firmware decoded as
/// **Tracking-Fast-CW** — i.e. continuous stepping with no auto-stop at
/// the `:S` target.
///
/// [bug]: ../../docs/services/star-adventurer-gti.md "Star Adventurer GTi design doc"
///
/// Prior art: INDI eqmod's `SetMotion` (`indi-eqmod/skywatcher.cpp:1643-1664`)
/// emits the same byte pairs we do (`"10"`, `"30"`, `"20"`, `"00"` with
/// direction in the low nibble).
///
/// **DB1** — high nibble (mode):
///
/// | Bit | Value | Meaning |
/// |-----|-------|---------|
/// | 0   | `0x1` | `0=Goto`, `1=Tracking` |
/// | 1   | `0x2` | In Goto: `0=Fast`, `1=Slow`; in Tracking: `0=Slow`, `1=Fast` |
/// | 2   | `0x4` | `0=S/F` (slow or fast), `1=Medium` |
/// | 3   | `0x8` | `1x Slow Goto` |
///
/// **DB2** — low nibble (direction / variant):
///
/// | Bit | Value | Meaning |
/// |-----|-------|---------|
/// | 0   | `0x1` | `0=CW`, `1=CCW` |
/// | 1   | `0x2` | `0=North`, `1=South` |
/// | 2   | `0x4` | `0=Normal Goto`, `1=Coarse Goto` |
///
/// The MVP uses just three combinations:
/// * Sidereal tracking → DB1=`0x1` (Tracking, Slow), DB2=`0x0` → wire `"10"`
/// * Goto-Fast CW → DB1=`0x0` (Goto, Fast), DB2=`0x0` → wire `"00"`
/// * Goto-Fast CCW → DB1=`0x0`, DB2=`0x1` → wire `"01"`
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct MotionMode {
    pub kind: ModeKind,
    pub speed: Speed,
    /// `true` = CCW (counter-clockwise); `false` = CW. Encoder convention
    /// is mount- and axis-specific — empirically on the Star Adventurer
    /// `GTi` CW corresponds to increasing encoder counts on both axes.
    pub ccw: bool,
}

/// `:G` DB1 bit 0 — Goto-vs-Tracking selector.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ModeKind {
    /// Move to the target set by `:S` and then auto-stop. The firmware
    /// handles slew speed internally; `:I` need not (and per the spec
    /// must not) be issued before `:J` for high-speed goto.
    Goto,
    /// Step continuously at the rate determined by `:I`. The driver
    /// must issue `:K`/`:L` to stop tracking when desired.
    Tracking,
}

/// `:G` DB1 bit 1 — Slow-vs-Fast selector.
///
/// Note the spec's wording: in **Goto** mode the bit reads `0=Fast,
/// 1=Slow`; in **Tracking** mode it reads `0=Slow, 1=Fast`. The
/// encoder collapses this into the consumer-friendly two-variant
/// enum and flips the bit per [`ModeKind`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Speed {
    Slow,
    Fast,
}

/// Step direction, as commanded via `:G` DB2 bit 0 and read back from
/// `:f` nibble 0 bit 1.
///
/// Encoder convention is mount- and axis-specific — empirically on the
/// Star Adventurer `GTi`, CW corresponds to increasing encoder counts
/// on both axes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Direction {
    Cw,
    Ccw,
}

impl MotionMode {
    /// Sidereal tracking preset: tracking, slow, CW (encoder-increasing).
    pub const TRACKING: Self = Self {
        kind: ModeKind::Tracking,
        speed: Speed::Slow,
        ccw: false,
    };
    /// Standard goto preset: goto, fast, CW. The caller flips `ccw`
    /// from the sign of the tick delta (target < current → `ccw=true`).
    pub const GOTO_FAST_FORWARD: Self = Self {
        kind: ModeKind::Goto,
        speed: Speed::Fast,
        ccw: false,
    };

    /// Pack this mode into the two ASCII hex bytes the `:G` command
    /// expects on the wire (DB1 first, then DB2).
    #[must_use]
    pub fn to_wire_bytes(self) -> [u8; 2] {
        // DB1 (high nibble): bit 0 = Tracking flag; bit 1 = Slow/Fast
        // selector whose meaning inverts between Goto and Tracking.
        let mut db1: u8 = 0;
        if matches!(self.kind, ModeKind::Tracking) {
            db1 |= 0x1;
        }
        let bit1_set = match (self.kind, self.speed) {
            // Goto: 1=Slow, 0=Fast; Tracking: 1=Fast, 0=Slow.
            (ModeKind::Goto, Speed::Slow) | (ModeKind::Tracking, Speed::Fast) => true,
            (ModeKind::Goto, Speed::Fast) | (ModeKind::Tracking, Speed::Slow) => false,
        };
        if bit1_set {
            db1 |= 0x2;
        }
        // DB2 (low nibble): bit 0 = CCW. North/South and Coarse-Goto
        // bits are left zero — neither is used in the MVP.
        let mut db2: u8 = 0;
        if self.ccw {
            db2 |= 0x1;
        }
        [nibble_to_hex(db1), nibble_to_hex(db2)]
    }
}

/// Every command the MVP issues. See
/// [`docs/services/star-adventurer-gti.md`](../../../docs/services/star-adventurer-gti.md)
/// §"Commands used by the MVP" for the table that drives this enum.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Command {
    /// `:F<axis>` — initialise axis.
    Initialize(Axis),
    /// `:a<axis>` — inquire counts-per-revolution.
    InquireCpr(Axis),
    /// `:b1` — inquire timer-interrupt frequency. Always axis 1.
    InquireTmrFreq,
    /// `:g<axis>` — inquire high-speed ratio.
    InquireHighSpeedRatio(Axis),
    /// `:e<axis>` — inquire motor-board version.
    InquireMotorBoardVersion(Axis),
    /// `:j<axis>` — inquire current encoder position.
    InquirePosition(Axis),
    /// `:f<axis>` — inquire axis status (running / mode / direction bits).
    InquireStatus(Axis),
    /// `:G<axis><mode>` — set motion mode.
    SetMotionMode { axis: Axis, mode: MotionMode },
    /// `:S<axis><pos>` — set absolute goto target (signed encoder ticks,
    /// bias-encoded by the codec).
    SetGotoTarget { axis: Axis, ticks: i32 },
    /// `:H<axis><inc>` — set goto-target by **unsigned magnitude** of the
    /// delta encoder ticks. The direction of motion is communicated
    /// separately via the `:G` mode byte's CCW bit. INDI eqmod's
    /// `SlewTo` uses `:H` rather than `:S` so the firmware can apply a
    /// fixed deceleration ramp from the configured break point — see
    /// the issue tracker #205 and the INDI source
    /// (`indi-eqmod/skywatcher.cpp::SlewTo`).
    SetGotoTargetIncrement { axis: Axis, increment: u32 },
    /// `:M<axis><breaks>` — set the goto break-point increment. INDI
    /// emits this on every slew with `breaks = min(|delta|/10, 3200)`;
    /// the firmware uses it to start decelerating before the target so
    /// the goto settles without overshoot.
    SetBreakPointIncrement { axis: Axis, breaks: u32 },
    /// `:I<axis><period>` — set step period (T1 preset). 24-bit unsigned.
    SetStepPeriod { axis: Axis, period: u32 },
    /// `:E<axis><pos>` — set current axis position (sync). Signed encoder
    /// ticks; bias-encoded by the codec.
    SetPosition { axis: Axis, ticks: i32 },
    /// `:J<axis>` — start motion.
    StartMotion(Axis),
    /// `:K<axis>` — stop motion (decelerate).
    StopMotion(Axis),
    /// `:L<axis>` — instant stop (used for `AbortSlew`).
    InstantStop(Axis),
}

impl Command {
    /// Encode this command into `out`, including the leading `:` and the
    /// trailing `\r`. Appends; does not clear `out` first.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(b':');
        match *self {
            Self::Initialize(axis) => {
                out.push(b'F');
                out.push(axis.wire_byte());
            }
            Self::InquireCpr(axis) => {
                out.push(b'a');
                out.push(axis.wire_byte());
            }
            Self::InquireTmrFreq => {
                out.extend_from_slice(b"b1");
            }
            Self::InquireHighSpeedRatio(axis) => {
                out.push(b'g');
                out.push(axis.wire_byte());
            }
            Self::InquireMotorBoardVersion(axis) => {
                out.push(b'e');
                out.push(axis.wire_byte());
            }
            Self::InquirePosition(axis) => {
                out.push(b'j');
                out.push(axis.wire_byte());
            }
            Self::InquireStatus(axis) => {
                out.push(b'f');
                out.push(axis.wire_byte());
            }
            Self::SetMotionMode { axis, mode } => {
                out.push(b'G');
                out.push(axis.wire_byte());
                out.extend_from_slice(&mode.to_wire_bytes());
            }
            Self::SetGotoTarget { axis, ticks } => {
                out.push(b'S');
                out.push(axis.wire_byte());
                out.extend_from_slice(&encode_position(ticks)?);
            }
            Self::SetGotoTargetIncrement { axis, increment } => {
                out.push(b'H');
                out.push(axis.wire_byte());
                out.extend_from_slice(&encode_u24(increment));
            }
            Self::SetBreakPointIncrement { axis, breaks } => {
                out.push(b'M');
                out.push(axis.wire_byte());
                out.extend_from_slice(&encode_u24(breaks));
            }
            Self::SetStepPeriod { axis, period } => {
                out.push(b'I');
                out.push(axis.wire_byte());
                out.extend_from_slice(&encode_u24(period));
            }
            Self::SetPosition { axis, ticks } => {
                out.push(b'E');
                out.push(axis.wire_byte());
                out.extend_from_slice(&encode_position(ticks)?);
            }
            Self::StartMotion(axis) => {
                out.push(b'J');
                out.push(axis.wire_byte());
            }
            Self::StopMotion(axis) => {
                out.push(b'K');
                out.push(axis.wire_byte());
            }
            Self::InstantStop(axis) => {
                out.push(b'L');
                out.push(axis.wire_byte());
            }
        }
        out.push(b'\r');
        Ok(())
    }

    /// Convenience: allocate a fresh `Vec<u8>` and encode into it.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(10);
        self.encode_into(&mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn motion_mode_wire_bytes_match_skywatcher_spec() {
        // Per Sky-Watcher motor-controller spec §5: DB1 / DB2 are
        // two independent hex nibbles. The expected wire-byte pairs
        // for the four MVP combinations:
        //   * Tracking-Slow-CW  (sidereal)  → DB1=1, DB2=0 → "10"
        //   * Tracking-Slow-CCW              → DB1=1, DB2=1 → "11"
        //   * Goto-Fast-CW                   → DB1=0, DB2=0 → "00"
        //   * Goto-Fast-CCW                  → DB1=0, DB2=1 → "01"
        assert_eq!(MotionMode::TRACKING.to_wire_bytes(), *b"10");
        assert_eq!(
            MotionMode {
                kind: ModeKind::Tracking,
                speed: Speed::Slow,
                ccw: true,
            }
            .to_wire_bytes(),
            *b"11"
        );
        assert_eq!(MotionMode::GOTO_FAST_FORWARD.to_wire_bytes(), *b"00");
        assert_eq!(
            MotionMode {
                kind: ModeKind::Goto,
                speed: Speed::Fast,
                ccw: true,
            }
            .to_wire_bytes(),
            *b"01"
        );
        // Speed bit semantics flip between Goto and Tracking.
        //   Goto-Slow-CW       → DB1 bit-1 set = "2"; DB2=0 → "20"
        //   Tracking-Fast-CW   → DB1 bit-1 set = "3" (also bit-0) → "30"
        // (so the prior codec's "30" was Tracking-Fast, not Goto-Fast —
        // see the doc comment on MotionMode for why this matters.)
        assert_eq!(
            MotionMode {
                kind: ModeKind::Goto,
                speed: Speed::Slow,
                ccw: false,
            }
            .to_wire_bytes(),
            *b"20"
        );
        assert_eq!(
            MotionMode {
                kind: ModeKind::Tracking,
                speed: Speed::Fast,
                ccw: false,
            }
            .to_wire_bytes(),
            *b"30"
        );
    }

    #[test]
    fn axis_wire_bytes() {
        assert_eq!(Axis::Ra.wire_byte(), b'1');
        assert_eq!(Axis::Dec.wire_byte(), b'2');
        assert_eq!(Axis::Both.wire_byte(), b'3');
    }

    #[test]
    fn handshake_inquiries_encode_to_design_doc_form() {
        // From docs/services/star-adventurer-gti.md §"Initialisation sequence".
        assert_eq!(Command::Initialize(Axis::Ra).encode().unwrap(), b":F1\r");
        assert_eq!(Command::Initialize(Axis::Dec).encode().unwrap(), b":F2\r");
        assert_eq!(Command::InquireCpr(Axis::Ra).encode().unwrap(), b":a1\r");
        assert_eq!(Command::InquireCpr(Axis::Dec).encode().unwrap(), b":a2\r");
        assert_eq!(Command::InquireTmrFreq.encode().unwrap(), b":b1\r");
        assert_eq!(
            Command::InquireHighSpeedRatio(Axis::Ra).encode().unwrap(),
            b":g1\r"
        );
        assert_eq!(
            Command::InquireMotorBoardVersion(Axis::Ra)
                .encode()
                .unwrap(),
            b":e1\r"
        );
        assert_eq!(
            Command::InquirePosition(Axis::Ra).encode().unwrap(),
            b":j1\r"
        );
        assert_eq!(Command::InquireStatus(Axis::Ra).encode().unwrap(), b":f1\r");
    }

    #[test]
    fn motion_setters_encode_with_payloads() {
        // Per Sky-Watcher spec §5: `GOTO_FAST_FORWARD` is DB1=0
        // (Goto+Fast) + DB2=0 (CW) → wire "00".
        assert_eq!(
            Command::SetMotionMode {
                axis: Axis::Ra,
                mode: MotionMode::GOTO_FAST_FORWARD,
            }
            .encode()
            .unwrap(),
            b":G100\r"
        );
        // :S1 with target encoder 0 → bias `0x800000` → "000080"
        assert_eq!(
            Command::SetGotoTarget {
                axis: Axis::Ra,
                ticks: 0,
            }
            .encode()
            .unwrap(),
            b":S1000080\r"
        );
        // :I1 with period 0x123456 → low-byte first "563412"
        assert_eq!(
            Command::SetStepPeriod {
                axis: Axis::Ra,
                period: 0x12_3456,
            }
            .encode()
            .unwrap(),
            b":I1563412\r"
        );
        // :E2 sync to ticks -1 → 0x7FFFFF → "FFFF7F"
        assert_eq!(
            Command::SetPosition {
                axis: Axis::Dec,
                ticks: -1,
            }
            .encode()
            .unwrap(),
            b":E2FFFF7F\r"
        );
    }

    #[test]
    fn slew_increment_setters_encode_with_u24_payloads() {
        // `:H` and `:M` payloads are plain 24-bit unsigned counts —
        // direction comes from the preceding `:G` mode byte's CCW
        // bit, not from the magnitude. INDI eqmod's `SlewTo` issues
        // both with `breaks = min(|delta|/10, 3200)` and increment
        // = `|delta|`, so the codec must accept any 24-bit value
        // without applying the `0x800000` position bias used by
        // `:S` and `:E`.
        // increment 0x000123 → low-byte-first u24 → "230100"
        assert_eq!(
            Command::SetGotoTargetIncrement {
                axis: Axis::Ra,
                increment: 0x0000_0123,
            }
            .encode()
            .unwrap(),
            b":H1230100\r"
        );
        // breaks 3200 = 0x000C80 → "800C00"
        assert_eq!(
            Command::SetBreakPointIncrement {
                axis: Axis::Dec,
                breaks: 3200,
            }
            .encode()
            .unwrap(),
            b":M2800C00\r"
        );
    }

    #[test]
    fn motion_starters_and_stoppers_encode_without_payload() {
        assert_eq!(Command::StartMotion(Axis::Ra).encode().unwrap(), b":J1\r");
        assert_eq!(Command::StopMotion(Axis::Ra).encode().unwrap(), b":K1\r");
        assert_eq!(Command::InstantStop(Axis::Ra).encode().unwrap(), b":L1\r");
        assert_eq!(Command::InstantStop(Axis::Dec).encode().unwrap(), b":L2\r");
    }

    #[test]
    fn encode_into_appends_does_not_clear() {
        let mut out = vec![b'X', b'Y'];
        Command::Initialize(Axis::Ra).encode_into(&mut out).unwrap();
        assert_eq!(out, b"XY:F1\r");
    }

    #[test]
    fn out_of_range_position_propagates_error() {
        // Beyond signed-24-bit range — should bubble up as ProtocolError.
        let result = Command::SetGotoTarget {
            axis: Axis::Ra,
            ticks: i32::MAX,
        }
        .encode();
        assert!(result.is_err());
    }
}

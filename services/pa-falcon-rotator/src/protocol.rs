//! Falcon Rotator wire protocol
//!
//! ASCII commands over 9600-8N1 serial, all LF-terminated in both directions.
//! See `docs/services/falcon-rotator.md#protocol-reference` for the source
//! command table.

use crate::error::{FalconRotatorError, Result};
use crate::units::{MechanicalDegrees, Steps};

/// Commands the driver issues to the Falcon Rotator.
///
/// Two Falcon commands are deliberately absent from this enum even though
/// they exist on the wire:
///
/// - `FF` (firmware reload) — design-doc maintenance operation the driver
///   must never issue.
/// - `SD:<deg>` (device-side sync) — would rewrite the Falcon's stored
///   counter and change `MechanicalPosition`, violating ASCOM `Sync`'s
///   "leave `MechanicalPosition` unchanged" contract. ASCOM `Sync` is
///   implemented as a driver-side offset instead; see
///   `docs/services/falcon-rotator.md#sync-semantics--why-driver-side-not-sd`.
///
/// Keeping these variants out of the public surface removes the temptation
/// to reach for them from later phases.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `F#` — ping / status check. Expect `FR_OK`.
    Ping,
    /// `FA` — full status. Expect `FR_OK:steps:deg:moving:limit:derot:reverse`.
    FullStatus,
    /// `FV` — firmware version. Expect `FV:n.n`.
    FirmwareVersion,
    /// `FD` — current position in degrees. Expect `FD:nn.nn`.
    PositionDeg,
    /// `FP` — current position in steps. Expect `FP:n..`.
    PositionSteps,
    /// `VS` — input voltage (raw / unscaled). Expect `VS:n..`.
    Voltage,
    /// `DR:0` — disable de-rotation.
    DerotationOff,
    /// `DR:<ms>` — enable de-rotation at `ms` ms/step.
    DerotationRate(u32),
    /// `MD:<deg>` — move to absolute (mechanical) degrees.
    MoveDeg(MechanicalDegrees),
    /// `MS:<steps>` — move to absolute steps.
    MoveSteps(Steps),
    /// `FH` — halt. Expect `FH:1`.
    Halt,
    /// `FR` — is running. Expect `FR:1` or `FR:0`.
    IsRunning,
    /// `FN:<b>` — set motor reverse (persisted in EEPROM).
    SetReverse(bool),
}

impl Command {
    /// Render the command into the exact ASCII payload the Falcon expects
    /// (without the LF terminator; the writer appends that).
    ///
    /// Degree values use the wire format the Falcon documents (`MD:nn.nn`):
    /// two-decimal-place fixed-point, which limits commandable precision to
    /// 0.01° regardless of the `f64` the caller hands in.
    #[must_use]
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Ping => "F#".to_string(),
            Self::FullStatus => "FA".to_string(),
            Self::FirmwareVersion => "FV".to_string(),
            Self::PositionDeg => "FD".to_string(),
            Self::PositionSteps => "FP".to_string(),
            Self::Voltage => "VS".to_string(),
            Self::DerotationOff => "DR:0".to_string(),
            Self::DerotationRate(ms) => format!("DR:{ms}"),
            Self::MoveDeg(deg) => format!("MD:{:.2}", deg.value()),
            Self::MoveSteps(steps) => format!("MS:{}", steps.value()),
            Self::Halt => "FH".to_string(),
            Self::IsRunning => "FR".to_string(),
            Self::SetReverse(on) => format!("FN:{}", i32::from(*on)),
        }
    }

    /// Reject command instances that would serialise to an invalid wire
    /// payload — currently just a `MoveDeg` whose `MechanicalDegrees` holds a
    /// non-finite value, which would emit `MD:NaN` / `MD:inf` / `MD:-inf`.
    ///
    /// The ASCOM boundary rejects non-finite move targets before a
    /// `MechanicalDegrees` is ever constructed, so this is defence-in-depth
    /// for callers that hand-build a `Command` and route it through the
    /// public `send_command` entry point. `to_command_string` stays
    /// infallible.
    pub fn validate(&self) -> Result<()> {
        if let Self::MoveDeg(deg) = self {
            if !deg.value().is_finite() {
                return Err(FalconRotatorError::InvalidValue(format!(
                    "MoveDeg target must be finite, got {}",
                    deg.value()
                )));
            }
        }
        Ok(())
    }
}

/// Parsed Falcon `FA` full-status response.
///
/// Wire format: `FR_OK:position_in_steps:position_in_deg:is_moving:limit_detect:do_derotation:motor_reverse`
///
/// `position_steps` is **signed**: the Falcon's step counter is referenced to
/// the 0° home and reads negative for positions reached CCW of home — which
/// happens whenever a target beyond the 220° CW soft limit is reached the long
/// way round. `position_deg` is always normalised to `[0, 360)`. Captured on
/// real hardware (firmware 1.5); see the `FromStr` tests.
#[derive(Debug, Clone, PartialEq)]
pub struct FalconStatus {
    pub position_steps: Steps,
    pub position_deg: MechanicalDegrees,
    pub is_moving: bool,
    pub limit_detect: bool,
    pub do_derotation: bool,
    pub motor_reverse: bool,
}

/// A wire payload: the text of a device response after its known
/// prefix, or one colon-separated field of it. The typed accessors
/// name the wire field in every parse error, so a corrupted or
/// misaligned frame reports *which* slot of the response was bad.
#[derive(Debug, Clone, Copy)]
struct Payload<'a>(&'a str);

impl<'a> Payload<'a> {
    /// Strip `prefix` off a trimmed response line.
    fn strip(response: &'a str, prefix: &str) -> Result<Self> {
        let trimmed = response.trim();
        trimmed.strip_prefix(prefix).map(Self).ok_or_else(|| {
            FalconRotatorError::InvalidResponse(format!(
                "expected prefix {prefix:?}, got {response:?}"
            ))
        })
    }

    fn text(self) -> &'a str {
        self.0
    }

    /// A `0`/`1` device flag, strictly: anything else is a corrupted or
    /// misaligned frame, not a truthy value.
    fn bool_field(self, field: &str) -> Result<bool> {
        match self.0 {
            "0" => Ok(false),
            "1" => Ok(true),
            other => Err(FalconRotatorError::ParseError(format!(
                "{field}: expected '0' or '1', got {other:?}"
            ))),
        }
    }

    /// Any `FromStr` field, with the wire field named in the error.
    fn parse_field<T>(self, field: &str) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        self.0
            .parse()
            .map_err(|e| FalconRotatorError::ParseError(format!("{field}: {e}")))
    }
}

/// Parse the `FR_OK:...` response from `FA`.
impl std::str::FromStr for FalconStatus {
    type Err = FalconRotatorError;

    fn from_str(response: &str) -> Result<Self> {
        let trimmed = response.trim();
        let mut parts = trimmed.split(':');
        let prefix = parts.next().unwrap_or("");
        if prefix != "FR_OK" {
            return Err(FalconRotatorError::InvalidResponse(format!(
                "FA: expected 'FR_OK' prefix, got {response:?}"
            )));
        }
        let fields: Vec<Payload<'_>> = parts.map(Payload).collect();
        // Exactly 6 fields, as before — the slice pattern has no rest arm.
        let [steps_field, deg_field, is_moving_field, limit_field, derotation_field, reverse_field] =
            fields.as_slice()
        else {
            return Err(FalconRotatorError::InvalidResponse(format!(
                "FA: expected 6 fields after 'FR_OK', got {} in {response:?}",
                fields.len()
            )));
        };
        // Signed: negative for positions CCW of the 0° home (e.g. a target
        // past the 220° CW limit reached the long way round). Parsing as u32
        // here is the bug that broke every status read whenever steps went
        // negative.
        let steps_raw: i32 = steps_field.parse_field("FA position_steps")?;
        let deg_raw: f64 = deg_field.parse_field("FA position_deg")?;
        if !deg_raw.is_finite() {
            return Err(FalconRotatorError::ParseError(format!(
                "FA position_deg: non-finite value {deg_raw} in {response:?}"
            )));
        }
        let position_steps = Steps(steps_raw);
        let position_deg = MechanicalDegrees::new(deg_raw);
        let is_moving = is_moving_field.bool_field("FA is_moving")?;
        let limit_detect = limit_field.bool_field("FA limit_detect")?;
        let do_derotation = derotation_field.bool_field("FA do_derotation")?;
        let motor_reverse = reverse_field.bool_field("FA motor_reverse")?;
        Ok(Self {
            position_steps,
            position_deg,
            is_moving,
            limit_detect,
            do_derotation,
            motor_reverse,
        })
    }
}

/// Parse the `FV:n.n` firmware version response.
pub fn parse_firmware_version(response: &str) -> Result<String> {
    let payload = Payload::strip(response, "FV:")?;
    if payload.text().is_empty() {
        return Err(FalconRotatorError::InvalidResponse(format!(
            "FV: empty version in {response:?}"
        )));
    }
    Ok(payload.text().to_string())
}

/// Parse the `FD:nn.nn` degrees response.
pub fn parse_position_deg(response: &str) -> Result<MechanicalDegrees> {
    let value: f64 = Payload::strip(response, "FD:")?.parse_field("FD")?;
    if !value.is_finite() {
        return Err(FalconRotatorError::ParseError(format!(
            "FD: non-finite value {value} in {response:?}"
        )));
    }
    Ok(MechanicalDegrees::new(value))
}

/// Parse the `FP:n..` steps response.
///
/// Signed: the step counter is referenced to the 0° home and reads negative
/// for positions CCW of home (real hardware, firmware 1.5).
pub fn parse_position_steps(response: &str) -> Result<Steps> {
    Payload::strip(response, "FP:")?
        .parse_field("FP")
        .map(Steps)
}

/// Parse the `VS:n..` raw voltage response.
pub fn parse_voltage_raw(response: &str) -> Result<u32> {
    Payload::strip(response, "VS:")?.parse_field("VS")
}

/// Parse the `FR:0` / `FR:1` is-running response.
pub fn parse_is_running(response: &str) -> Result<bool> {
    Payload::strip(response, "FR:")?.bool_field("FR is_running")
}

/// Parse the `FN:0` / `FN:1` motor-reverse echo response.
pub fn parse_reverse(response: &str) -> Result<bool> {
    Payload::strip(response, "FN:")?.bool_field("FN motor_reverse")
}

/// Validate a `FR_OK` ping response (with optional trailing whitespace).
pub fn validate_ping_response(response: &str) -> Result<()> {
    if response.trim() == "FR_OK" {
        Ok(())
    } else {
        Err(FalconRotatorError::InvalidResponse(format!(
            "ping: expected 'FR_OK', got {response:?}"
        )))
    }
}

/// Validate that `response` echoes the issued `command`.
///
/// Supports the echo-bearing wire commands: `MD:`, `MS:`, `DR:0`, `DR:<ms>`,
/// `FN:<b>`, and `FH` (whose echo shape is the special `FH:1` per the
/// design doc). Commands that have non-echo responses (`Ping`, `FullStatus`,
/// `FirmwareVersion`, `PositionDeg`, `PositionSteps`, `Voltage`, `IsRunning`)
/// belong to their dedicated parser and are rejected here.
pub fn validate_echo(command: &Command, response: &str) -> Result<()> {
    let expected = match command {
        Command::Halt => "FH:1".to_string(),
        Command::MoveDeg(_)
        | Command::MoveSteps(_)
        | Command::DerotationOff
        | Command::DerotationRate(_)
        | Command::SetReverse(_) => command.to_command_string(),
        Command::Ping
        | Command::FullStatus
        | Command::FirmwareVersion
        | Command::PositionDeg
        | Command::PositionSteps
        | Command::Voltage
        | Command::IsRunning => {
            return Err(FalconRotatorError::InvalidResponse(format!(
                "validate_echo not applicable for {command:?}; use the dedicated parser"
            )));
        }
    };
    if response.trim() == expected {
        Ok(())
    } else {
        Err(FalconRotatorError::InvalidResponse(format!(
            "echo: expected {expected:?} for {command:?}, got {response:?}"
        )))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // ---- Command::to_command_string ---------------------------------------

    #[test]
    fn command_ping_serialises_to_hash() {
        assert_eq!(Command::Ping.to_command_string(), "F#");
    }

    #[test]
    fn command_full_status_serialises_to_fa() {
        assert_eq!(Command::FullStatus.to_command_string(), "FA");
    }

    #[test]
    fn command_firmware_version_serialises_to_fv() {
        assert_eq!(Command::FirmwareVersion.to_command_string(), "FV");
    }

    #[test]
    fn command_position_deg_serialises_to_fd() {
        assert_eq!(Command::PositionDeg.to_command_string(), "FD");
    }

    #[test]
    fn command_position_steps_serialises_to_fp() {
        assert_eq!(Command::PositionSteps.to_command_string(), "FP");
    }

    #[test]
    fn command_voltage_serialises_to_vs() {
        assert_eq!(Command::Voltage.to_command_string(), "VS");
    }

    #[test]
    fn command_derotation_off_serialises_to_dr_zero() {
        assert_eq!(Command::DerotationOff.to_command_string(), "DR:0");
    }

    #[test]
    fn command_derotation_rate_includes_ms() {
        assert_eq!(Command::DerotationRate(50).to_command_string(), "DR:50");
    }

    #[test]
    fn command_move_deg_uses_two_decimal_places() {
        assert_eq!(
            Command::MoveDeg(MechanicalDegrees::new(284.8)).to_command_string(),
            "MD:284.80"
        );
    }

    #[test]
    fn command_move_deg_pads_to_two_decimal_places() {
        assert_eq!(
            Command::MoveDeg(MechanicalDegrees::new(0.0)).to_command_string(),
            "MD:0.00"
        );
        assert_eq!(
            Command::MoveDeg(MechanicalDegrees::new(45.0)).to_command_string(),
            "MD:45.00"
        );
    }

    #[test]
    fn command_move_steps_includes_count() {
        assert_eq!(
            Command::MoveSteps(Steps(31_192)).to_command_string(),
            "MS:31192"
        );
    }

    #[test]
    fn command_halt_serialises_to_fh() {
        assert_eq!(Command::Halt.to_command_string(), "FH");
    }

    #[test]
    fn command_is_running_serialises_to_fr() {
        assert_eq!(Command::IsRunning.to_command_string(), "FR");
    }

    // ---- Command::validate -----------------------------------------------

    #[test]
    fn validate_accepts_finite_move_deg() {
        Command::MoveDeg(MechanicalDegrees::new(180.0))
            .validate()
            .unwrap();
        Command::MoveDeg(MechanicalDegrees::new(0.0))
            .validate()
            .unwrap();
        Command::MoveDeg(MechanicalDegrees::new(-90.0))
            .validate()
            .unwrap();
        Command::MoveDeg(MechanicalDegrees::new(359.99))
            .validate()
            .unwrap();
    }

    #[test]
    fn validate_rejects_non_finite_move_deg() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = Command::MoveDeg(MechanicalDegrees::new(bad))
                .validate()
                .unwrap_err();
            assert!(
                matches!(err, FalconRotatorError::InvalidValue(_)),
                "expected InvalidValue for {bad}, got {err:?}"
            );
        }
    }

    #[test]
    fn validate_passthrough_for_non_move_deg_variants() {
        // Every other variant is structurally infallible — validate() must
        // accept them without inspecting payload shape.
        Command::Ping.validate().unwrap();
        Command::FullStatus.validate().unwrap();
        Command::FirmwareVersion.validate().unwrap();
        Command::PositionDeg.validate().unwrap();
        Command::PositionSteps.validate().unwrap();
        Command::Voltage.validate().unwrap();
        Command::DerotationOff.validate().unwrap();
        Command::DerotationRate(50).validate().unwrap();
        Command::MoveSteps(Steps(31_192)).validate().unwrap();
        Command::Halt.validate().unwrap();
        Command::IsRunning.validate().unwrap();
        Command::SetReverse(true).validate().unwrap();
        Command::SetReverse(false).validate().unwrap();
    }

    #[test]
    fn command_set_reverse_true_serialises_to_fn_one() {
        assert_eq!(Command::SetReverse(true).to_command_string(), "FN:1");
    }

    #[test]
    fn command_set_reverse_false_serialises_to_fn_zero() {
        assert_eq!(Command::SetReverse(false).to_command_string(), "FN:0");
    }

    // ---- parse_full_status ------------------------------------------------

    #[test]
    fn parse_full_status_happy_path() {
        let status = "FR_OK:4332:50.00:0:0:0:0".parse::<FalconStatus>().unwrap();
        assert_eq!(status.position_steps, Steps(4332));
        assert!((status.position_deg.value() - 50.0).abs() < 1e-9);
        assert!(!status.is_moving);
        assert!(!status.limit_detect);
        assert!(!status.do_derotation);
        assert!(!status.motor_reverse);
    }

    #[test]
    fn parse_full_status_with_trailing_newline() {
        let status = "FR_OK:4332:50.00:0:0:0:0\n"
            .parse::<FalconStatus>()
            .unwrap();
        assert_eq!(status.position_steps, Steps(4332));
    }

    #[test]
    fn parse_full_status_with_trailing_crlf() {
        let status = "FR_OK:4332:50.00:0:0:0:0\r\n"
            .parse::<FalconStatus>()
            .unwrap();
        assert_eq!(status.position_steps, Steps(4332));
    }

    #[test]
    fn parse_full_status_with_limit_detect_high() {
        let status = "FR_OK:0:0.00:0:1:0:0".parse::<FalconStatus>().unwrap();
        assert!(status.limit_detect);
    }

    #[test]
    fn parse_full_status_with_all_flags_high() {
        let status = "FR_OK:100:1.00:1:1:1:1".parse::<FalconStatus>().unwrap();
        assert!(status.is_moving);
        assert!(status.limit_detect);
        assert!(status.do_derotation);
        assert!(status.motor_reverse);
    }

    #[test]
    fn parse_full_status_rejects_wrong_prefix() {
        let err = "FR_ERR:4332:50.00:0:0:0:0"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn parse_full_status_rejects_too_few_fields() {
        let err = "FR_OK:4332:50.00:0:0:0"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn parse_full_status_rejects_too_many_fields() {
        let err = "FR_OK:4332:50.00:0:0:0:0:99"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn parse_full_status_rejects_bad_steps() {
        let err = "FR_OK:abc:50.00:0:0:0:0"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_rejects_bad_float() {
        let err = "FR_OK:4332:nope:0:0:0:0"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_rejects_bad_bool() {
        let err = "FR_OK:4332:50.00:2:0:0:0"
            .parse::<FalconStatus>()
            .unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_rejects_empty() {
        let err = "".parse::<FalconStatus>().unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn parse_full_status_rejects_nan_position() {
        let err = "FR_OK:0:NaN:0:0:0:0".parse::<FalconStatus>().unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_rejects_positive_infinity_position() {
        let err = "FR_OK:0:inf:0:0:0:0".parse::<FalconStatus>().unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_rejects_negative_infinity_position() {
        let err = "FR_OK:0:-inf:0:0:0:0".parse::<FalconStatus>().unwrap_err();
        assert!(matches!(err, FalconRotatorError::ParseError(_)));
    }

    #[test]
    fn parse_full_status_accepts_negative_steps_below_home() {
        // Real-hardware capture (firmware 1.5): driving past the 220° CW limit
        // sends the rotator the long way round — CCW past the 0° home — where
        // the signed step counter goes negative while position_deg wraps into
        // [0, 360). Parsing field 0 as i32 (not u32) is what keeps status reads
        // alive across that region; the u32 parse here used to abort the read
        // with "FA position_steps: invalid digit found in string".
        let status = "FR_OK:-2838:327.24:1:0:0:0"
            .parse::<FalconStatus>()
            .unwrap();
        assert_eq!(status.position_steps, Steps(-2838));
        assert!((status.position_deg.value() - 327.24).abs() < 1e-9);
        assert!(status.is_moving);
        assert!(!status.limit_detect);
    }

    // ---- parse_firmware_version -------------------------------------------

    #[test]
    fn parse_firmware_version_basic() {
        assert_eq!(parse_firmware_version("FV:1.3").unwrap(), "1.3");
    }

    #[test]
    fn parse_firmware_version_strips_trailing_newline() {
        assert_eq!(parse_firmware_version("FV:1.3\n").unwrap(), "1.3");
    }

    #[test]
    fn parse_firmware_version_rejects_bad_prefix() {
        let err = parse_firmware_version("FX:1.3").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn parse_firmware_version_rejects_empty_version() {
        let err = parse_firmware_version("FV:").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    // ---- parse_position_deg -----------------------------------------------

    #[test]
    fn parse_position_deg_basic() {
        let v = parse_position_deg("FD:142.30").unwrap();
        assert!((v.value() - 142.30).abs() < 1e-9);
    }

    #[test]
    fn parse_position_deg_rejects_bad_prefix() {
        assert!(matches!(
            parse_position_deg("FQ:142.30").unwrap_err(),
            FalconRotatorError::InvalidResponse(_)
        ));
    }

    #[test]
    fn parse_position_deg_rejects_bad_value() {
        assert!(matches!(
            parse_position_deg("FD:nope").unwrap_err(),
            FalconRotatorError::ParseError(_)
        ));
    }

    #[test]
    fn parse_position_deg_rejects_nan() {
        assert!(matches!(
            parse_position_deg("FD:NaN").unwrap_err(),
            FalconRotatorError::ParseError(_)
        ));
    }

    #[test]
    fn parse_position_deg_rejects_positive_infinity() {
        assert!(matches!(
            parse_position_deg("FD:inf").unwrap_err(),
            FalconRotatorError::ParseError(_)
        ));
    }

    #[test]
    fn parse_position_deg_rejects_negative_infinity() {
        assert!(matches!(
            parse_position_deg("FD:-inf").unwrap_err(),
            FalconRotatorError::ParseError(_)
        ));
    }

    // ---- parse_position_steps ---------------------------------------------

    #[test]
    fn parse_position_steps_basic() {
        assert_eq!(parse_position_steps("FP:12345").unwrap(), Steps(12345));
    }

    #[test]
    fn parse_position_steps_accepts_negative_below_home() {
        // The Falcon step counter is signed relative to the 0° home: positions
        // CCW of home report negative steps. Captured on real hardware
        // (firmware 1.5), e.g. `FP:-1784` observed at 339.96° while traversing
        // past the 220° CW limit the long way round.
        assert_eq!(parse_position_steps("FP:-1784").unwrap(), Steps(-1784));
    }

    // ---- parse_voltage_raw ------------------------------------------------

    #[test]
    fn parse_voltage_raw_basic() {
        assert_eq!(parse_voltage_raw("VS:812").unwrap(), 812);
    }

    #[test]
    fn parse_voltage_raw_rejects_bad_prefix() {
        assert!(matches!(
            parse_voltage_raw("VX:812").unwrap_err(),
            FalconRotatorError::InvalidResponse(_)
        ));
    }

    // ---- parse_is_running -------------------------------------------------

    #[test]
    fn parse_is_running_true() {
        assert!(parse_is_running("FR:1").unwrap());
    }

    #[test]
    fn parse_is_running_false() {
        assert!(!parse_is_running("FR:0").unwrap());
    }

    #[test]
    fn parse_is_running_rejects_bad_value() {
        assert!(matches!(
            parse_is_running("FR:2").unwrap_err(),
            FalconRotatorError::ParseError(_)
        ));
    }

    // ---- parse_reverse ----------------------------------------------------

    #[test]
    fn parse_reverse_true() {
        assert!(parse_reverse("FN:1").unwrap());
    }

    #[test]
    fn parse_reverse_false() {
        assert!(!parse_reverse("FN:0").unwrap());
    }

    #[test]
    fn parse_reverse_rejects_bad_prefix() {
        assert!(matches!(
            parse_reverse("FM:1").unwrap_err(),
            FalconRotatorError::InvalidResponse(_)
        ));
    }

    // ---- validate_ping_response -------------------------------------------

    #[test]
    fn validate_ping_response_accepts_fr_ok() {
        validate_ping_response("FR_OK").unwrap();
    }

    #[test]
    fn validate_ping_response_accepts_trailing_newline() {
        validate_ping_response("FR_OK\n").unwrap();
    }

    #[test]
    fn validate_ping_response_accepts_surrounding_whitespace() {
        validate_ping_response("  FR_OK \r\n").unwrap();
    }

    #[test]
    fn validate_ping_response_rejects_other_payload() {
        let err = validate_ping_response("INVALID").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn validate_ping_response_rejects_empty() {
        let err = validate_ping_response("").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    // ---- validate_echo ----------------------------------------------------

    #[test]
    fn validate_echo_move_deg_matches() {
        validate_echo(
            &Command::MoveDeg(MechanicalDegrees::new(180.0)),
            "MD:180.00",
        )
        .unwrap();
    }

    #[test]
    fn validate_echo_move_deg_matches_with_trailing_newline() {
        validate_echo(
            &Command::MoveDeg(MechanicalDegrees::new(180.0)),
            "MD:180.00\n",
        )
        .unwrap();
    }

    #[test]
    fn validate_echo_move_steps_matches() {
        validate_echo(&Command::MoveSteps(Steps(15_000)), "MS:15000").unwrap();
    }

    #[test]
    fn validate_echo_derotation_off_matches() {
        validate_echo(&Command::DerotationOff, "DR:0").unwrap();
    }

    #[test]
    fn validate_echo_derotation_rate_matches() {
        validate_echo(&Command::DerotationRate(25), "DR:25").unwrap();
    }

    #[test]
    fn validate_echo_halt_accepts_fh_colon_one() {
        validate_echo(&Command::Halt, "FH:1").unwrap();
    }

    #[test]
    fn validate_echo_halt_rejects_plain_fh() {
        let err = validate_echo(&Command::Halt, "FH").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn validate_echo_set_reverse_true_matches() {
        validate_echo(&Command::SetReverse(true), "FN:1").unwrap();
    }

    #[test]
    fn validate_echo_set_reverse_false_matches() {
        validate_echo(&Command::SetReverse(false), "FN:0").unwrap();
    }

    #[test]
    fn validate_echo_rejects_mismatch() {
        let err = validate_echo(
            &Command::MoveDeg(MechanicalDegrees::new(180.0)),
            "MD:181.00",
        )
        .unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn validate_echo_rejects_ping() {
        let err = validate_echo(&Command::Ping, "FR_OK").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn validate_echo_rejects_full_status() {
        let err = validate_echo(&Command::FullStatus, "FR_OK:0:0.00:0:0:0:0").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }

    #[test]
    fn validate_echo_rejects_is_running() {
        let err = validate_echo(&Command::IsRunning, "FR:0").unwrap_err();
        assert!(matches!(err, FalconRotatorError::InvalidResponse(_)));
    }
}

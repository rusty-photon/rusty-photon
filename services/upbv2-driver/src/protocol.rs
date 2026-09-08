//! UPBv2 protocol implementation.
//!
//! This module handles the serial protocol for the Pegasus Astro Ultimate
//! Powerbox v2.
//!
//! Serial Settings: 9600 baud, 8N1, newline-terminated commands.
//!
//! Set commands echo their own wire string; status commands return a
//! colon-separated payload. See `docs/services/upbv2-driver.md` for the
//! command table and the commands this driver deliberately never sends.

use std::time::Duration;

use crate::error::{Result, Upbv2Error};

/// Number of 12 V output ports.
pub const OUTPUT_COUNT: usize = 4;
/// Number of dew heater channels.
pub const DEW_COUNT: usize = 3;
/// Number of USB ports (1-4 are USB3, 5-6 are USB2).
pub const USB_COUNT: usize = 6;
/// Width of the `PA` overcurrent field: the four outputs then the three dew
/// channels.
pub const OVERCURRENT_COUNT: usize = OUTPUT_COUNT + DEW_COUNT;

/// Token count of a well-formed `PA` reply: the device-name prefix plus 20
/// data fields.
const PA_FIELD_COUNT: usize = 21;
/// Token count of a well-formed `PC` reply.
const PC_FIELD_COUNT: usize = 4;
/// Token count of a well-formed `PS` reply: the prefix, the boot port states,
/// and the variable-output setpoint.
const PS_FIELD_COUNT: usize = 3;

/// Sense-resistor divisor for the four outputs and dew channels A and B.
const SENSE_DIVISOR: f64 = 480.0;
/// Dew channel C runs through a different MOSFET and needs its own divisor.
const SENSE_DIVISOR_DEW_C: f64 = 700.0;

/// Lowest voltage the variable output accepts.
pub const VARIABLE_VOLTS_MIN: u8 = 3;
/// Highest voltage the variable output accepts.
pub const VARIABLE_VOLTS_MAX: u8 = 12;

/// One of the four switched 12 V outputs, numbered as the device labels them.
///
/// An enum rather than a bounded integer so that every lookup keyed on an
/// output — a name, a wire number, an array slot — is a total `match` with no
/// unreachable arm to justify and no cast to bounds-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputId {
    One,
    Two,
    Three,
    Four,
}

impl OutputId {
    /// Every output, in device order.
    pub const ALL: [Self; OUTPUT_COUNT] = [Self::One, Self::Two, Self::Three, Self::Four];

    /// The device's own 1-based port number, as it appears in `P1:`-`P4:`.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
            Self::Four => 4,
        }
    }

    /// Build from the device's own 1-based numbering.
    ///
    /// # Errors
    ///
    /// Returns [`Upbv2Error::InvalidValue`] when `n` is outside 1..=4.
    pub fn new(n: u8) -> Result<Self> {
        match n {
            1 => Ok(Self::One),
            2 => Ok(Self::Two),
            3 => Ok(Self::Three),
            4 => Ok(Self::Four),
            other => Err(Upbv2Error::InvalidValue(format!(
                "12V output must be 1-4, got {other}"
            ))),
        }
    }

    /// Zero-based index into the `PA` port-status and current arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::One => 0,
            Self::Two => 1,
            Self::Three => 2,
            Self::Four => 3,
        }
    }
}

/// One of the three dew heater channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DewChannel {
    A,
    B,
    C,
}

impl DewChannel {
    /// Every dew channel, in device order.
    pub const ALL: [Self; DEW_COUNT] = [Self::A, Self::B, Self::C];

    /// Zero-based index into the `PA` duty and current arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
        }
    }

    /// The wire command number: dew A/B/C are `P5`/`P6`/`P7`.
    const fn command_number(self) -> u8 {
        match self {
            Self::A => 5,
            Self::B => 6,
            Self::C => 7,
        }
    }

    /// Operator-facing channel label, used in error messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "dew heater A",
            Self::B => "dew heater B",
            Self::C => "dew heater C",
        }
    }
}

/// One of the six switched USB ports, numbered as the device labels them.
///
/// Ports 1-4 are USB 3.0, ports 5-6 are USB 2.0. An enum for the same reason
/// [`OutputId`] is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbPortId {
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
}

impl UsbPortId {
    /// Every USB port, in device order.
    pub const ALL: [Self; USB_COUNT] = [
        Self::One,
        Self::Two,
        Self::Three,
        Self::Four,
        Self::Five,
        Self::Six,
    ];

    /// The device's own 1-based port number, as it appears in `U1:`-`U6:`.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
            Self::Four => 4,
            Self::Five => 5,
            Self::Six => 6,
        }
    }

    /// Build from the device's own 1-based numbering.
    ///
    /// # Errors
    ///
    /// Returns [`Upbv2Error::InvalidValue`] when `n` is outside 1..=6.
    pub fn new(n: u8) -> Result<Self> {
        match n {
            1 => Ok(Self::One),
            2 => Ok(Self::Two),
            3 => Ok(Self::Three),
            4 => Ok(Self::Four),
            5 => Ok(Self::Five),
            6 => Ok(Self::Six),
            other => Err(Upbv2Error::InvalidValue(format!(
                "USB port must be 1-6, got {other}"
            ))),
        }
    }

    /// Zero-based index into the `PA` usb-status array.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::One => 0,
            Self::Two => 1,
            Self::Three => 2,
            Self::Four => 3,
            Self::Five => 4,
            Self::Six => 5,
        }
    }
}

/// Dew-heater PWM duty, clamped to the device's 0-255 range. Owns the one
/// analog-value (ASCOM switch `f64`) → wire-byte conversion so call sites stay
/// cast-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmDuty(pub u8);

impl From<f64> for PwmDuty {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to u8's exact range on the line above; NaN clamps \
                  through the saturating cast to 0"
    )]
    fn from(value: f64) -> Self {
        Self(value.round().clamp(0.0, 255.0) as u8)
    }
}

/// Variable-output setpoint in whole volts, 3-12.
///
/// Rejected rather than clamped: writing this value stores it in the device's
/// EEPROM, so a client that asked for 24 V has a bug the driver should report,
/// not silently round away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariableVolts(u8);

impl VariableVolts {
    /// # Errors
    ///
    /// Returns [`Upbv2Error::InvalidValue`] when `volts` is outside 3..=12.
    pub fn new(volts: u8) -> Result<Self> {
        if (VARIABLE_VOLTS_MIN..=VARIABLE_VOLTS_MAX).contains(&volts) {
            Ok(Self(volts))
        } else {
            Err(Upbv2Error::InvalidValue(format!(
                "variable output voltage must be \
                 {VARIABLE_VOLTS_MIN}-{VARIABLE_VOLTS_MAX} V, got {volts}"
            )))
        }
    }

    /// Build from an ASCOM switch value.
    ///
    /// # Errors
    ///
    /// Returns [`Upbv2Error::InvalidValue`] when the rounded value is outside
    /// 3..=12, or when it is not finite.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "range-checked against u8 on the line above before the cast"
    )]
    pub fn from_switch_value(value: f64) -> Result<Self> {
        let rounded = value.round();
        if !rounded.is_finite()
            || rounded < f64::from(VARIABLE_VOLTS_MIN)
            || rounded > f64::from(VARIABLE_VOLTS_MAX)
        {
            return Err(Upbv2Error::InvalidValue(format!(
                "variable output voltage must be \
                 {VARIABLE_VOLTS_MIN}-{VARIABLE_VOLTS_MAX} V, got {value}"
            )));
        }
        Self::new(rounded as u8)
    }

    /// The setpoint in volts.
    #[must_use]
    pub const fn volts(self) -> u8 {
        self.0
    }
}

/// The device's auto-dew channel mask, as reported in `PA` field 20.
///
/// The driver never writes this (`PD:` is out of scope — see the design doc);
/// it reads the mask so a channel the device is driving can report
/// `CanWrite = false` instead of silently losing the operator's write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoDewMask(u8);

impl AutoDewMask {
    /// Wrap the raw wire value. Any `u8` is accepted: the device reports
    /// aggressiveness settings above 7 in the same field, and
    /// [`controls`](Self::controls) treats those as controlling nothing.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        Self(raw)
    }

    /// The raw 0-7 mask as the device reports it.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// Whether the device is driving `channel` itself.
    ///
    /// The vendor encoding is a lookup, not a bitfield: 0 is none, 1 is all
    /// three, and 2-7 enumerate the remaining combinations.
    #[must_use]
    pub const fn controls(self, channel: DewChannel) -> bool {
        let (a, b, c) = match self.0 {
            1 => (true, true, true),
            2 => (true, false, false),
            3 => (false, true, false),
            4 => (false, false, true),
            5 => (true, true, false),
            6 => (true, false, true),
            7 => (false, true, true),
            // 0 disables auto-dew; anything above 7 is an aggressiveness
            // value this driver never writes and treats as "not controlling".
            _ => (false, false, false),
        };
        match channel {
            DewChannel::A => a,
            DewChannel::B => b,
            DewChannel::C => c,
        }
    }
}

/// Commands this driver sends to the UPBv2.
///
/// The set is deliberately narrow. `PD:` (auto-dew), `PE:`/`US:` (boot state),
/// `PL:` (LED — it doubles as the stepper sleep line on revision C and later
/// boards), `PZ:`
/// (master off) and the stepper commands are **not** represented here, so they
/// cannot be sent by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upbv2Command {
    /// Ping / status check — returns `UPB2_OK`.
    Ping,
    /// Firmware version — returns `n.n`.
    FirmwareVersion,
    /// Full status and sensor readings (`PA`).
    Status,
    /// Power consumption counters (`PC`).
    PowerConsumption,
    /// Boot power state and the variable-voltage setpoint (`PS`).
    BootState,
    /// Switch a 12 V output (`P1:`-`P4:`).
    SetOutput(OutputId, bool),
    /// Set a dew channel's PWM duty (`P5:`-`P7:`).
    SetDew(DewChannel, PwmDuty),
    /// Set the variable output voltage (`P8:`). Stored in EEPROM.
    SetVariableVoltage(VariableVolts),
    /// Switch a USB port (`U1:`-`U6:`).
    SetUsb(UsbPortId, bool),
}

impl Upbv2Command {
    /// Serialize the command to the string sent to the device.
    #[must_use]
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Ping => "P#".to_string(),
            Self::FirmwareVersion => "PV".to_string(),
            Self::Status => "PA".to_string(),
            Self::PowerConsumption => "PC".to_string(),
            Self::BootState => "PS".to_string(),
            Self::SetOutput(id, on) => format!("P{}:{}", id.number(), u8::from(*on)),
            Self::SetDew(channel, duty) => format!("P{}:{}", channel.command_number(), duty.0),
            Self::SetVariableVoltage(volts) => format!("P8:{}", volts.0),
            Self::SetUsb(id, on) => format!("U{}:{}", id.number(), u8::from(*on)),
        }
    }
}

/// Parsed `PA` status response.
///
/// Currents arrive as raw sense counts on the wire and are divided here, so
/// everything above this module works in Amps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Upbv2Status {
    /// Input voltage in Volts.
    pub voltage: f64,
    /// Total current draw in Amps.
    pub current: f64,
    /// Total power draw in Watts.
    pub power: u32,
    /// Temperature in Celsius.
    pub temperature: f64,
    /// Relative humidity percentage.
    pub humidity: f64,
    /// Dewpoint in Celsius.
    pub dewpoint: f64,
    /// On/off state of the four 12 V outputs.
    pub outputs: [bool; OUTPUT_COUNT],
    /// On/off state of the six USB ports.
    pub usb_ports: [bool; USB_COUNT],
    /// PWM duty (0-255) of the three dew channels.
    pub dew_duty: [u8; DEW_COUNT],
    /// Per-output current draw in Amps.
    pub output_current: [f64; OUTPUT_COUNT],
    /// Per-dew-channel current draw in Amps.
    pub dew_current: [f64; DEW_COUNT],
    /// Overcurrent / short-circuit flags: outputs 1-4 then dew A-C. The device
    /// shuts an affected channel down on its own when one trips.
    pub overcurrent: [bool; OVERCURRENT_COUNT],
    /// Which dew channels the device is driving itself.
    pub auto_dew: AutoDewMask,
}

/// Parsed `PC` power-consumption response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Upbv2PowerConsumption {
    /// Average current in Amps.
    pub average_amps: f64,
    /// Cumulative amp-hours.
    pub amp_hours: f64,
    /// Cumulative watt-hours.
    pub watt_hours: f64,
    /// Device uptime. The wire field is integer milliseconds; it is held as a
    /// `Duration` internally and flattened back only at the boundary.
    pub uptime: Duration,
}

impl Upbv2PowerConsumption {
    /// Uptime in hours, the unit the ASCOM switch reports.
    #[must_use]
    pub fn uptime_hours(&self) -> f64 {
        self.uptime.as_secs_f64() / 3600.0
    }
}

/// Parsed `PS` boot-state response.
///
/// Only [`variable_volts`](Self::variable_volts) is exposed as a switch: `PA`
/// does not carry the variable-output setpoint, so `PS` is the one place to
/// read it. The boot-state field is parsed so a malformed frame is still
/// caught, then discarded — configuring boot state is out of scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Upbv2BootState {
    /// Power-on-boot state of the four outputs. Parsed, not exposed.
    ///
    /// Read right-aligned: the firmware omits leading zeros in this field
    /// even though the vendor table documents it as `bbbb`.
    pub boot_outputs: [bool; OUTPUT_COUNT],
    /// The variable output's stored setpoint in Volts.
    pub variable_volts: u8,
}

/// Sequential reader over a response's colon-separated tokens.
///
/// Fields are pulled in wire order and labelled as they are read, so a parse
/// error always names the slot that was bad. Reading positionally rather than
/// binding every token also keeps a twenty-field frame free of the twenty
/// near-identical locals a slice pattern would need.
struct Fields<'a> {
    tokens: std::str::Split<'a, char>,
    frame: &'a str,
}

impl<'a> Fields<'a> {
    fn new(frame: &'a str) -> Self {
        Self {
            tokens: frame.split(':'),
            frame,
        }
    }

    /// The next raw token, or an error naming the field that ran out.
    fn raw(&mut self, field: &str) -> Result<&'a str> {
        self.tokens.next().ok_or_else(|| {
            Upbv2Error::InvalidResponse(format!("response ended before {field}: {}", self.frame))
        })
    }

    /// Consume a token whose value the driver does not use.
    fn skip(&mut self, field: &str) -> Result<()> {
        self.raw(field).map(|_| ())
    }

    /// Any `FromStr` field, with the wire field named in the error.
    fn parse<T: std::str::FromStr>(&mut self, field: &str) -> Result<T> {
        let raw = self.raw(field)?;
        raw.parse()
            .map_err(|_| Upbv2Error::ParseError(format!("Invalid {field} value: {raw}")))
    }

    /// A raw sense-current count converted to Amps.
    fn current(&mut self, field: &str, divisor: f64) -> Result<f64> {
        let raw: u32 = self.parse(field)?;
        Ok(f64::from(raw) / divisor)
    }

    /// A fixed-width run of `0`/`1` characters, one per channel.
    fn flags<const N: usize>(&mut self, field: &str) -> Result<[bool; N]> {
        let raw = self.raw(field)?;
        if raw.len() != N {
            return Err(Upbv2Error::ParseError(format!(
                "Invalid {field} value: expected {N} flags, got {} in {raw}",
                raw.len(),
            )));
        }
        let mut out = [false; N];
        for (slot, ch) in out.iter_mut().zip(raw.chars()) {
            *slot = match ch {
                '0' => false,
                '1' => true,
                other => {
                    return Err(Upbv2Error::ParseError(format!(
                        "Invalid {field} flag {other:?} in {raw}"
                    )))
                }
            };
        }
        Ok(out)
    }

    /// Like [`flags`](Self::flags), for a field the firmware emits as a
    /// *number* rather than a fixed-width string.
    ///
    /// `PS`'s boot-state field is documented as `bbbb` and exampled as
    /// `PS:1111:8`, but the firmware prints it unpadded: rig2's box answers
    /// `PS:110:6`, three characters for four outputs. `PA`'s port-status
    /// field in the same frame is `0010` — zero-padded — so the two go
    /// through different formatting paths in the firmware, and only this one
    /// loses leading zeros.
    ///
    /// Reading it right-aligned recovers the intended value: `110` is
    /// `0110`, `1` is `0001`, `0` is `0000`. Every case is consistent with
    /// digits-as-flags printed as an integer, which is the only reading that
    /// explains a three-character field at all.
    fn flags_right_aligned<const N: usize>(&mut self, field: &str) -> Result<[bool; N]> {
        let raw = self.raw(field)?;
        if raw.is_empty() || raw.len() > N {
            return Err(Upbv2Error::ParseError(format!(
                "Invalid {field} value: expected 1 to {N} flags, got {} in {raw}",
                raw.len(),
            )));
        }
        let mut out = [false; N];
        // Right-align: the last character of `raw` is the last flag.
        let offset = N.saturating_sub(raw.len());
        for (i, ch) in raw.chars().enumerate() {
            let slot = out
                .get_mut(offset.saturating_add(i))
                .ok_or_else(|| Upbv2Error::ParseError(format!("Invalid {field} value: {raw}")))?;
            *slot = match ch {
                '0' => false,
                '1' => true,
                other => {
                    return Err(Upbv2Error::ParseError(format!(
                        "Invalid {field} flag {other:?} in {raw}"
                    )))
                }
            };
        }
        Ok(out)
    }
}

/// The two prefixes a `PA` reply is accepted under.
///
/// The vendor command table contradicts itself: its worked example shows
/// `UPB:` while the field legend for the same frame says `UPB2:`. Both are
/// accepted rather than guessing which firmware writes which — the frame is
/// identified by its field count and shape either way. See open item 1 in
/// `docs/services/upbv2-driver.md`.
pub const STATUS_PREFIXES: [&str; 2] = ["UPB2:", "UPB:"];

/// The ping reply a UPBv2 gives.
pub const PING_OK: &str = "UPB2_OK";
/// The ping reply a Pocket Powerbox Advance gives — recognised only so the
/// driver can tell the operator they pointed it at the wrong device.
const PPBA_PING_OK: &str = "PPBA_OK";

/// Reject a frame that cannot hold `expected` colon-separated tokens.
///
/// Checked up front so a truncated frame reports its shape once, rather than
/// surfacing as whichever field happened to fall off the end.
fn require_token_count(frame: &str, expected: usize, what: &str) -> Result<()> {
    let got = frame.split(':').count();
    if got < expected {
        return Err(Upbv2Error::InvalidResponse(format!(
            "Expected {expected} parts in {what} response, got {got}: {frame}"
        )));
    }
    Ok(())
}

impl std::str::FromStr for Upbv2Status {
    type Err = Upbv2Error;

    fn from_str(response: &str) -> Result<Self> {
        let response = response.trim();

        if !STATUS_PREFIXES.iter().any(|p| response.starts_with(p)) {
            return Err(Upbv2Error::InvalidResponse(format!(
                "Expected one of {STATUS_PREFIXES:?} prefix, got: {response}"
            )));
        }
        require_token_count(response, PA_FIELD_COUNT, "PA")?;

        let mut f = Fields::new(response);
        f.skip("device name")?;

        Ok(Self {
            voltage: f.parse("voltage")?,
            current: f.parse("current")?,
            power: f.parse("power")?,
            temperature: f.parse("temperature")?,
            humidity: f.parse("humidity")?,
            dewpoint: f.parse("dewpoint")?,
            outputs: f.flags("port status")?,
            usb_ports: f.flags("usb status")?,
            dew_duty: [
                f.parse("dew A duty")?,
                f.parse("dew B duty")?,
                f.parse("dew C duty")?,
            ],
            output_current: [
                f.current("output 1 current", SENSE_DIVISOR)?,
                f.current("output 2 current", SENSE_DIVISOR)?,
                f.current("output 3 current", SENSE_DIVISOR)?,
                f.current("output 4 current", SENSE_DIVISOR)?,
            ],
            dew_current: [
                f.current("dew A current", SENSE_DIVISOR)?,
                f.current("dew B current", SENSE_DIVISOR)?,
                // Dew C runs through a different MOSFET.
                f.current("dew C current", SENSE_DIVISOR_DEW_C)?,
            ],
            overcurrent: f.flags("overcurrent")?,
            auto_dew: AutoDewMask(f.parse("auto-dew mask")?),
        })
    }
}

impl std::str::FromStr for Upbv2PowerConsumption {
    type Err = Upbv2Error;

    /// Parse a `PC` reply.
    ///
    /// The vendor table documents the payload as a bare
    /// `avgAmps:ampHours:wattHours:uptime` tuple with no command echo, unlike
    /// the PPBA's `PS:`-prefixed statistics. An optional `PC:` prefix is
    /// tolerated so a firmware that does echo still parses. See open item 2 in
    /// `docs/services/upbv2-driver.md`.
    fn from_str(response: &str) -> Result<Self> {
        let response = response.trim();
        let body = response.strip_prefix("PC:").unwrap_or(response);
        require_token_count(body, PC_FIELD_COUNT, "PC")?;

        let mut f = Fields::new(body);

        Ok(Self {
            average_amps: f.parse("average amps")?,
            amp_hours: f.parse("amp hours")?,
            watt_hours: f.parse("watt hours")?,
            uptime: Duration::from_millis(f.parse("uptime")?),
        })
    }
}

impl std::str::FromStr for Upbv2BootState {
    type Err = Upbv2Error;

    fn from_str(response: &str) -> Result<Self> {
        let response = response.trim();

        if !response.starts_with("PS:") {
            return Err(Upbv2Error::InvalidResponse(format!(
                "Expected PS: prefix, got: {response}"
            )));
        }
        require_token_count(response, PS_FIELD_COUNT, "PS")?;

        let mut f = Fields::new(response);
        f.skip("PS prefix")?;

        Ok(Self {
            boot_outputs: f.flags_right_aligned("boot port status")?,
            variable_volts: f.parse("variable voltage")?,
        })
    }
}

/// Validate a ping response.
///
/// A PPBA answers `PPBA_OK` on the same baud rate and framing, and the two
/// boxes share a USB vendor and product id — so pointing this driver at one is
/// an easy mistake with an unpleasant failure mode, since `P3:`/`P4:` set dew
/// heaters there and 12 V rails here. Naming the right service is cheaper than
/// letting the operator find out downstream.
///
/// # Errors
///
/// Returns [`Upbv2Error::WrongModel`] for a PPBA, and
/// [`Upbv2Error::InvalidResponse`] for anything else that is not
/// [`PING_OK`].
pub fn validate_ping_response(response: &str) -> Result<()> {
    match response.trim() {
        PING_OK => Ok(()),
        PPBA_PING_OK => Err(Upbv2Error::WrongModel {
            got: PPBA_PING_OK.to_string(),
            model: "Pocket Powerbox Advance Gen2",
            service: "ppba-driver",
        }),
        other => Err(Upbv2Error::InvalidResponse(format!(
            "Expected {PING_OK}, got: {other}"
        ))),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The mock's canonical frame, and the one the BDD suite asserts against.
    const SAMPLE_PA: &str =
        "UPB2:12.5:2.4:30:25.0:60:16.5:1101:111101:128:64:0:480:960:0:240:240:96:350:0000000:0";

    // ---- Upbv2Command::to_command_string ---------------------------------

    #[test]
    fn ping_serializes_to_p_hash() {
        assert_eq!(Upbv2Command::Ping.to_command_string(), "P#");
    }

    #[test]
    fn status_serializes_to_pa() {
        assert_eq!(Upbv2Command::Status.to_command_string(), "PA");
    }

    #[test]
    fn power_consumption_serializes_to_pc_not_ps() {
        assert_eq!(Upbv2Command::PowerConsumption.to_command_string(), "PC");
    }

    #[test]
    fn boot_state_serializes_to_ps() {
        assert_eq!(Upbv2Command::BootState.to_command_string(), "PS");
    }

    #[test]
    fn outputs_serialize_to_p1_through_p4() {
        let wire: Vec<String> = OutputId::ALL
            .iter()
            .map(|id| Upbv2Command::SetOutput(*id, true).to_command_string())
            .collect();
        assert_eq!(wire, ["P1:1", "P2:1", "P3:1", "P4:1"]);
    }

    #[test]
    fn output_off_serializes_zero() {
        let cmd = Upbv2Command::SetOutput(OutputId::ALL[0], false);
        assert_eq!(cmd.to_command_string(), "P1:0");
    }

    #[test]
    fn dew_channels_serialize_to_p5_through_p7() {
        let wire: Vec<String> = DewChannel::ALL
            .iter()
            .map(|ch| Upbv2Command::SetDew(*ch, PwmDuty(200)).to_command_string())
            .collect();
        assert_eq!(wire, ["P5:200", "P6:200", "P7:200"]);
    }

    #[test]
    fn variable_voltage_serializes_to_p8() {
        let volts = VariableVolts::new(5).unwrap();
        assert_eq!(
            Upbv2Command::SetVariableVoltage(volts).to_command_string(),
            "P8:5"
        );
    }

    #[test]
    fn usb_ports_serialize_to_u1_through_u6() {
        let wire: Vec<String> = UsbPortId::ALL
            .iter()
            .map(|id| Upbv2Command::SetUsb(*id, false).to_command_string())
            .collect();
        assert_eq!(wire, ["U1:0", "U2:0", "U3:0", "U4:0", "U5:0", "U6:0"]);
    }

    // ---- PwmDuty ---------------------------------------------------------

    #[test]
    fn pwm_duty_rounds_to_nearest() {
        assert_eq!(PwmDuty::from(127.6), PwmDuty(128));
    }

    #[test]
    fn pwm_duty_clamps_above_range() {
        assert_eq!(PwmDuty::from(300.0), PwmDuty(255));
    }

    #[test]
    fn pwm_duty_clamps_below_range() {
        assert_eq!(PwmDuty::from(-10.0), PwmDuty(0));
    }

    #[test]
    fn pwm_duty_maps_nan_to_zero() {
        assert_eq!(PwmDuty::from(f64::NAN), PwmDuty(0));
    }

    // ---- Frames captured from real hardware ------------------------------
    //
    // rig2's UPBv2, FTDIBUS\VID_0403+PID_6015+UPB248E11MA, read-only probe.

    /// `PA` exactly as the box answered it.
    const RIG_PA: &str = "UPB2:12.7:0.0:0:40.9:32:20.9:0010:110001:0:0:0:0:0:0:0:0:0:0:0000000:1";

    /// `PS` exactly as the box answered it.
    const RIG_PS: &str = "PS:110:6";

    /// `PC` exactly as the box answered it — no prefix, no echo.
    const RIG_PC: &str = "0.17:14.56:184.93:305389357";

    #[test]
    fn rig_pa_frame_parses() {
        let status: Upbv2Status = RIG_PA.parse().unwrap();
        assert!((status.voltage - 12.7).abs() < f64::EPSILON);
        assert_eq!(status.humidity, 32.0);
        // Magnus against 40.9 C / 32 % gives 20.95, so the device's own
        // dewpoint confirms the temperature and humidity slots are not
        // transposed.
        assert!((status.dewpoint - 20.9).abs() < f64::EPSILON);
        assert_eq!(status.outputs, [false, false, true, false]);
        assert_eq!(status.usb_ports, [true, true, false, false, false, true]);
        assert_eq!(status.overcurrent, [false; OVERCURRENT_COUNT]);
        // Auto-dew is on for all three channels on this box.
        assert_eq!(status.auto_dew.raw(), 1);
    }

    #[test]
    fn rig_ps_frame_parses() {
        let boot: Upbv2BootState = RIG_PS.parse().unwrap();
        assert_eq!(boot.variable_volts, 6);
        // Three characters for four outputs: read right-aligned, so the
        // missing leading character is output 1.
        assert_eq!(boot.boot_outputs, [false, true, true, false]);
    }

    #[test]
    fn ps_boot_flags_are_read_right_aligned() {
        // Every width the firmware can emit for four outputs, given it
        // prints the field as a number.
        for (raw, expected) in [
            ("1111", [true, true, true, true]),
            ("110", [false, true, true, false]),
            ("11", [false, false, true, true]),
            ("1", [false, false, false, true]),
            ("0", [false, false, false, false]),
            ("1000", [true, false, false, false]),
        ] {
            let frame = format!("PS:{raw}:8");
            let boot: Upbv2BootState = frame.parse().unwrap();
            assert_eq!(boot.boot_outputs, expected, "for PS field {raw}");
        }
    }

    #[test]
    fn ps_boot_flags_reject_more_flags_than_outputs() {
        let err = "PS:11111:8".parse::<Upbv2BootState>().unwrap_err();
        assert!(
            err.to_string().contains("boot port status"),
            "error should name the field: {err}"
        );
    }

    #[test]
    fn ps_boot_flags_reject_a_non_binary_character() {
        let err = "PS:1x0:8".parse::<Upbv2BootState>().unwrap_err();
        assert!(
            err.to_string().contains("boot port status"),
            "error should name the field: {err}"
        );
    }

    #[test]
    fn rig_pc_frame_is_recognised_as_power_counters() {
        let counters: Upbv2PowerConsumption = RIG_PC.parse().unwrap();
        assert!((counters.average_amps - 0.17).abs() < f64::EPSILON);
    }

    // ---- VariableVolts ---------------------------------------------------

    #[test]
    fn variable_volts_accepts_the_documented_range() {
        for v in VARIABLE_VOLTS_MIN..=VARIABLE_VOLTS_MAX {
            assert_eq!(VariableVolts::new(v).unwrap().volts(), v);
        }
    }

    #[test]
    fn variable_volts_rejects_below_three() {
        let err = VariableVolts::new(2).unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidValue(_)), "got {err:?}");
    }

    #[test]
    fn variable_volts_rejects_above_twelve() {
        let err = VariableVolts::new(13).unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidValue(_)), "got {err:?}");
    }

    #[test]
    fn variable_volts_from_switch_value_rounds_before_checking() {
        assert_eq!(VariableVolts::from_switch_value(4.6).unwrap().volts(), 5);
    }

    #[test]
    fn variable_volts_from_switch_value_rejects_out_of_range() {
        let err = VariableVolts::from_switch_value(24.0).unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidValue(_)), "got {err:?}");
    }

    #[test]
    fn variable_volts_from_switch_value_rejects_nan() {
        let err = VariableVolts::from_switch_value(f64::NAN).unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidValue(_)), "got {err:?}");
    }

    // ---- AutoDewMask -----------------------------------------------------

    #[test]
    fn auto_dew_mask_zero_controls_nothing() {
        let mask = AutoDewMask::from_raw(0);
        assert!(DewChannel::ALL.iter().all(|ch| !mask.controls(*ch)));
    }

    #[test]
    fn auto_dew_mask_one_controls_every_channel() {
        let mask = AutoDewMask::from_raw(1);
        assert!(DewChannel::ALL.iter().all(|ch| mask.controls(*ch)));
    }

    #[test]
    fn auto_dew_mask_enumerates_the_vendor_combinations() {
        // (mask, [A, B, C]) straight out of the vendor command table.
        let table = [
            (2_u8, [true, false, false]),
            (3, [false, true, false]),
            (4, [false, false, true]),
            (5, [true, true, false]),
            (6, [true, false, true]),
            (7, [false, true, true]),
        ];
        for (raw, expected) in table {
            let mask = AutoDewMask::from_raw(raw);
            let got = DewChannel::ALL.map(|ch| mask.controls(ch));
            assert_eq!(got, expected, "mask {raw}");
        }
    }

    #[test]
    fn auto_dew_mask_treats_an_aggressiveness_value_as_not_controlling() {
        // Above 7 the wire value is an aggressiveness setting, not a channel
        // mask. This driver never writes one; reading one must not make a
        // channel look controlled.
        let mask = AutoDewMask::from_raw(210);
        assert!(DewChannel::ALL.iter().all(|ch| !mask.controls(*ch)));
    }

    // ---- Channel newtypes ------------------------------------------------

    #[test]
    fn output_id_rejects_zero_and_five() {
        assert!(OutputId::new(0).is_err());
        assert!(OutputId::new(5).is_err());
    }

    #[test]
    fn output_id_indexes_from_zero() {
        assert_eq!(OutputId::ALL.map(OutputId::index), [0, 1, 2, 3]);
    }

    #[test]
    fn usb_port_id_rejects_zero_and_seven() {
        assert!(UsbPortId::new(0).is_err());
        assert!(UsbPortId::new(7).is_err());
    }

    #[test]
    fn dew_channel_indexes_from_zero() {
        assert_eq!(DewChannel::ALL.map(DewChannel::index), [0, 1, 2]);
    }

    // ---- PA parsing ------------------------------------------------------

    #[test]
    fn status_parses_environmental_fields() {
        let status: Upbv2Status = SAMPLE_PA.parse().unwrap();
        assert_eq!(status.voltage, 12.5);
        assert_eq!(status.current, 2.4);
        assert_eq!(status.power, 30);
        assert_eq!(status.temperature, 25.0);
        assert_eq!(status.humidity, 60.0);
        assert_eq!(status.dewpoint, 16.5);
    }

    #[test]
    fn status_parses_output_and_usb_flags() {
        let status: Upbv2Status = SAMPLE_PA.parse().unwrap();
        assert_eq!(status.outputs, [true, true, false, true]);
        assert_eq!(status.usb_ports, [true, true, true, true, false, true]);
    }

    #[test]
    fn status_parses_dew_duty() {
        let status: Upbv2Status = SAMPLE_PA.parse().unwrap();
        assert_eq!(status.dew_duty, [128, 64, 0]);
    }

    #[test]
    fn status_scales_output_currents_by_480() {
        let status: Upbv2Status = SAMPLE_PA.parse().unwrap();
        assert_eq!(status.output_current, [1.0, 2.0, 0.0, 0.5]);
    }

    #[test]
    fn status_scales_dew_c_current_by_700_not_480() {
        let status: Upbv2Status = SAMPLE_PA.parse().unwrap();
        // 240/480, 96/480, 350/700 — the third divisor is the whole point.
        assert_eq!(status.dew_current, [0.5, 0.2, 0.5]);
    }

    #[test]
    fn status_parses_overcurrent_flags_for_outputs_then_dew() {
        let frame = SAMPLE_PA.replace(":0000000:", ":0100001:");
        let status: Upbv2Status = frame.parse().unwrap();
        assert_eq!(
            status.overcurrent,
            [false, true, false, false, false, false, true]
        );
    }

    #[test]
    fn status_parses_auto_dew_mask() {
        let frame = format!("{}5", SAMPLE_PA.strip_suffix('0').unwrap());
        let status: Upbv2Status = frame.parse().unwrap();
        assert_eq!(status.auto_dew.raw(), 5);
    }

    #[test]
    fn status_accepts_the_upb_prefix_variant() {
        // The vendor table's worked example uses `UPB:` where its own field
        // legend says `UPB2:`; both parse.
        let frame = SAMPLE_PA.replacen("UPB2:", "UPB:", 1);
        let status: Upbv2Status = frame.parse().unwrap();
        assert_eq!(status.voltage, 12.5);
    }

    #[test]
    fn status_rejects_a_foreign_prefix() {
        let err = "PPBA:12.5:3.2:25.0:60:15.5:1:0:128:64:0:0:0"
            .parse::<Upbv2Status>()
            .unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidResponse(_)), "got {err:?}");
    }

    #[test]
    fn status_rejects_a_short_frame() {
        let err = "UPB2:12.5:2.4:30".parse::<Upbv2Status>().unwrap_err();
        let Upbv2Error::InvalidResponse(msg) = err else {
            panic!("expected InvalidResponse, got {err:?}");
        };
        assert!(
            msg.contains("21"),
            "message should name the expected count: {msg}"
        );
    }

    #[test]
    fn status_names_the_bad_field_in_a_parse_error() {
        let frame = SAMPLE_PA.replacen(":25.0:", ":not-a-number:", 1);
        let err = frame.parse::<Upbv2Status>().unwrap_err();
        let Upbv2Error::ParseError(msg) = err else {
            panic!("expected ParseError, got {err:?}");
        };
        assert!(
            msg.contains("temperature"),
            "message should name the field: {msg}"
        );
    }

    #[test]
    fn status_rejects_a_flag_field_of_the_wrong_width() {
        let frame = SAMPLE_PA.replacen(":1101:", ":110:", 1);
        let err = frame.parse::<Upbv2Status>().unwrap_err();
        assert!(matches!(err, Upbv2Error::ParseError(_)), "got {err:?}");
    }

    #[test]
    fn status_rejects_a_non_binary_flag_character() {
        let frame = SAMPLE_PA.replacen(":1101:", ":11X1:", 1);
        let err = frame.parse::<Upbv2Status>().unwrap_err();
        assert!(matches!(err, Upbv2Error::ParseError(_)), "got {err:?}");
    }

    // ---- PC parsing ------------------------------------------------------

    #[test]
    fn power_consumption_parses_a_bare_tuple() {
        let pc: Upbv2PowerConsumption = "1.85:0.42:5.1:3600000".parse().unwrap();
        assert_eq!(pc.average_amps, 1.85);
        assert_eq!(pc.amp_hours, 0.42);
        assert_eq!(pc.watt_hours, 5.1);
        assert_eq!(pc.uptime, Duration::from_millis(3_600_000));
    }

    #[test]
    fn power_consumption_tolerates_an_echoed_prefix() {
        let pc: Upbv2PowerConsumption = "PC:1.85:0.42:5.1:3600000".parse().unwrap();
        assert_eq!(pc.average_amps, 1.85);
    }

    #[test]
    fn power_consumption_converts_uptime_to_hours() {
        let pc: Upbv2PowerConsumption = "0:0:0:5400000".parse().unwrap();
        assert_eq!(pc.uptime_hours(), 1.5);
    }

    #[test]
    fn power_consumption_rejects_a_short_tuple() {
        let err = "1.85:0.42:5.1"
            .parse::<Upbv2PowerConsumption>()
            .unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidResponse(_)), "got {err:?}");
    }

    // ---- PS parsing ------------------------------------------------------

    #[test]
    fn boot_state_parses_the_variable_voltage() {
        let ps: Upbv2BootState = "PS:1101:12".parse().unwrap();
        assert_eq!(ps.variable_volts, 12);
        assert_eq!(ps.boot_outputs, [true, true, false, true]);
    }

    #[test]
    fn boot_state_rejects_a_missing_prefix() {
        let err = "1101:12".parse::<Upbv2BootState>().unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidResponse(_)), "got {err:?}");
    }

    // ---- ping ------------------------------------------------------------

    #[test]
    fn ping_accepts_the_upbv2_reply() {
        validate_ping_response("UPB2_OK\r\n").unwrap();
    }

    #[test]
    fn ping_reports_a_ppba_as_the_wrong_model_and_names_its_service() {
        let err = validate_ping_response("PPBA_OK").unwrap_err();
        let Upbv2Error::WrongModel { service, .. } = err else {
            panic!("expected WrongModel, got {err:?}");
        };
        assert_eq!(service, "ppba-driver");
    }

    #[test]
    fn ping_rejects_an_unrecognised_reply() {
        let err = validate_ping_response("HELLO").unwrap_err();
        assert!(matches!(err, Upbv2Error::InvalidResponse(_)), "got {err:?}");
    }
}

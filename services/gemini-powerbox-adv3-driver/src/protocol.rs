//! The Gemini Power & Data Hubs Advanced 3 wire protocol.
//!
//! Serial settings: 19200 baud, 8N1. A command is `>X…#` followed by `\n`;
//! a reply is `*X…#` with no line ending, `X` echoing the command letter.
//! Set commands draw no reply. The one status frame, `*G…#`, carries
//! seventeen suffix-tagged fields that the parser consumes **in order**,
//! because the tags repeat. See `docs/services/gemini-powerbox-adv3-driver.md`
//! for the command table, the frame layout and the measurements behind both.

use std::fmt;

use crate::error::{GeminiError, Result};

/// What the Advanced 3 answers to `>H#`, compared verbatim.
pub const IDENTITY: &str = "GeminiPowerBoxPlusAdv3";

/// The oldest firmware whose status frame this driver knows. Below it the
/// connect is refused; both open-source drivers refuse likewise.
pub const MIN_FIRMWARE: u16 = 308;

/// The firmware that fixes DS18B20 probe detection, which the connect
/// recommends when it finds [`MIN_FIRMWARE`].
pub const RECOMMENDED_FIRMWARE: u16 = 309;

/// The vendor's upgrade tool, named when a connect is refused for firmware.
pub const FIRMWARE_UPGRADE_TOOL: &str = "FirmwareUpgradeTool";

/// Switched 12 V outputs: DC2 to DC5. DC1 is always on and has no command.
pub const DC_OUTPUT_COUNT: usize = 4;
/// Switchable USB ports: A to F.
pub const USB_PORT_COUNT: usize = 6;
/// Dew heater channels: DEW6 and DEW7.
pub const DEW_CHANNEL_COUNT: usize = 2;
/// The highest manual duty the dew channels accept, in percent.
pub const DEW_DUTY_MAX: u8 = 100;

/// One of the four switched 12 V outputs, named as the box's panel names
/// them.
///
/// An enum rather than a bounded integer so that every lookup keyed on an
/// output — a name, a wire number, an array slot — is a total `match` with no
/// unreachable arm to justify and no cast to bounds-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcOutput {
    Dc2,
    Dc3,
    Dc4,
    Dc5,
}

impl DcOutput {
    /// Every switched output, in panel order.
    pub const ALL: [Self; DC_OUTPUT_COUNT] = [Self::Dc2, Self::Dc3, Self::Dc4, Self::Dc5];

    /// The output number `>O<n>#` and `>C<n>#` take: 2 to 5.
    #[must_use]
    pub const fn wire_number(self) -> u8 {
        match self {
            Self::Dc2 => 2,
            Self::Dc3 => 3,
            Self::Dc4 => 4,
            Self::Dc5 => 5,
        }
    }

    /// Zero-based slot in the frame's `D` field, which lists DC2 to DC5.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Dc2 => 0,
            Self::Dc3 => 1,
            Self::Dc4 => 2,
            Self::Dc5 => 3,
        }
    }

    /// The connector's name on the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dc2 => "DC2",
            Self::Dc3 => "DC3",
            Self::Dc4 => "DC4",
            Self::Dc5 => "DC5",
        }
    }
}

/// One of the six switchable USB ports. A and B are USB 3.2 Gen1, C to F
/// USB 2.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbPort {
    A,
    B,
    C,
    D,
    E,
    F,
}

impl UsbPort {
    /// Every port, in panel order.
    pub const ALL: [Self; USB_PORT_COUNT] = [Self::A, Self::B, Self::C, Self::D, Self::E, Self::F];

    /// The output number `>O<n>#` and `>C<n>#` take for this port: 6 to 11.
    /// Outputs 6 to 11 are the USB ports, not the dew channels, whose panel
    /// numbers happen to be 6 and 7.
    #[must_use]
    pub const fn wire_number(self) -> u8 {
        match self {
            Self::A => 6,
            Self::B => 7,
            Self::C => 8,
            Self::D => 9,
            Self::E => 10,
            Self::F => 11,
        }
    }

    /// Zero-based slot in the frame's `U` field.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::D => 3,
            Self::E => 4,
            Self::F => 5,
        }
    }

    /// The connector's name on the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::A => "USB A",
            Self::B => "USB B",
            Self::C => "USB C",
            Self::D => "USB D",
            Self::E => "USB E",
            Self::F => "USB F",
        }
    }

    /// Whether this is one of the two USB 3.2 Gen1 ports.
    #[must_use]
    pub const fn is_usb3(self) -> bool {
        matches!(self, Self::A | Self::B)
    }
}

/// One of the two dew heater channels. Numbered 6 and 7 on the panel and in
/// the vendor's names, but channel `1` and `2` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DewChannel {
    Dew6,
    Dew7,
}

impl DewChannel {
    /// Both channels, in panel order.
    pub const ALL: [Self; DEW_CHANNEL_COUNT] = [Self::Dew6, Self::Dew7];

    /// The channel digit `>Z<c>x#` and `>M<c>x#` take: 1 or 2.
    #[must_use]
    pub const fn wire_number(self) -> u8 {
        match self {
            Self::Dew6 => 1,
            Self::Dew7 => 2,
        }
    }

    /// The manual-duty command letter: `X` for DEW6, `Y` for DEW7.
    #[must_use]
    pub const fn duty_letter(self) -> char {
        match self {
            Self::Dew6 => 'X',
            Self::Dew7 => 'Y',
        }
    }

    /// Zero-based slot in the per-channel arrays of a [`StatusFrame`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Dew6 => 0,
            Self::Dew7 => 1,
        }
    }

    /// The connector's name on the panel.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dew6 => "DEW6",
            Self::Dew7 => "DEW7",
        }
    }
}

/// A dew channel's mode: the digit the frame reports and `>M<c><d>#` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DewMode {
    /// The hub's PID loop holds the lens probe above the dew point.
    Auto,
    /// The heater runs at the manual duty.
    Manual,
    /// The heater is plain DC, on or off.
    Switch,
}

impl DewMode {
    /// The wire digit.
    #[must_use]
    pub const fn digit(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Manual => 1,
            Self::Switch => 2,
        }
    }

    /// Build from the wire digit.
    #[must_use]
    pub const fn from_digit(digit: u8) -> Option<Self> {
        match digit {
            0 => Some(Self::Auto),
            1 => Some(Self::Manual),
            2 => Some(Self::Switch),
            _ => None,
        }
    }

    /// Build from an ASCOM switch value: 0, 1 or 2 once rounded to the
    /// nearest step, which is how a switch takes a value between steps.
    ///
    /// # Errors
    ///
    /// Returns [`GeminiError::InvalidValue`] for a value outside 0 to 2 or
    /// one that is not a number.
    pub fn from_switch_value(value: f64) -> Result<Self> {
        whole_number_in(value, 0, 2)
            .and_then(Self::from_digit)
            .ok_or_else(|| {
                GeminiError::InvalidValue(format!(
                    "dew mode must be 0 (Auto), 1 (Manual) or 2 (Switch), got {value}"
                ))
            })
    }

    /// The mode's name, as the error messages use it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Manual => "Manual",
            Self::Switch => "Switch",
        }
    }
}

/// A manual dew duty, 0 to 100 percent, as `>X<pct>#` / `>Y<pct>#` take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DewDuty(u8);

impl DewDuty {
    /// Build from an ASCOM switch value: a percentage from 0 to 100, rounded
    /// to the nearest whole one, which is the row's step.
    ///
    /// # Errors
    ///
    /// Returns [`GeminiError::InvalidValue`] for a value outside that range
    /// or one that is not a number; nothing is clamped before it reaches a
    /// heater.
    pub fn from_switch_value(value: f64) -> Result<Self> {
        whole_number_in(value, 0, DEW_DUTY_MAX)
            .map(Self)
            .ok_or_else(|| {
                GeminiError::InvalidValue(format!(
                    "dew duty must be from 0 to {DEW_DUTY_MAX} percent, got {value}"
                ))
            })
    }

    /// The duty in percent.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }
}

/// `value` rounded to the nearest whole number (half away from zero), as a
/// `u8`, when it is finite and lies in `min..=max`; `None` otherwise. NaN
/// fails the finiteness test, so it never passes as a 0. The range is
/// checked on the value as given, so `100.4` is refused like `101`, not
/// rounded down into range.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked finite and inside a u8 range on the lines above the cast"
)]
fn whole_number_in(value: f64, min: u8, max: u8) -> Option<u8> {
    if !value.is_finite() {
        return None;
    }
    if value < f64::from(min) || value > f64::from(max) {
        return None;
    }
    Some(value.round() as u8)
}

/// A command that changes the hub's state. Each draws no reply of its own,
/// so the codec sends it together with a `>G#` and reads the frame.
///
/// The set is the whole writable surface and nothing else: `>O1#` / `>C1#`
/// for the always-on DC1 have no variant, so they cannot be sent by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetCommand {
    /// A 12 V output on or off: `>O<n>#` / `>C<n>#`.
    Dc(DcOutput, bool),
    /// A USB port on or off: `>O<n>#` / `>C<n>#`, `n` from 6.
    Usb(UsbPort, bool),
    /// A channel's manual duty: `>X<pct>#` / `>Y<pct>#`.
    DewDuty(DewChannel, DewDuty),
    /// A channel enabled or disabled: `>Z<c>1#` / `>Z<c>0#`.
    DewEnabled(DewChannel, bool),
    /// A channel's mode: `>M<c><d>#`.
    DewMode(DewChannel, DewMode),
}

impl SetCommand {
    /// The command as it goes on the wire, terminator included, without
    /// the trailing newline.
    #[must_use]
    pub fn wire(&self) -> String {
        match *self {
            Self::Dc(output, on) => output_command(output.wire_number(), on),
            Self::Usb(port, on) => output_command(port.wire_number(), on),
            Self::DewDuty(channel, duty) => {
                format!(">{}{}#", channel.duty_letter(), duty.percent())
            }
            Self::DewEnabled(channel, enabled) => {
                format!(">Z{}{}#", channel.wire_number(), u8::from(enabled))
            }
            Self::DewMode(channel, mode) => {
                format!(">M{}{}#", channel.wire_number(), mode.digit())
            }
        }
    }
}

/// `>O<n>#` turns output `n` on, `>C<n>#` turns it off.
fn output_command(number: u8, on: bool) -> String {
    let letter = if on { 'O' } else { 'C' };
    format!(">{letter}{number}#")
}

/// Everything this driver sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiCommand {
    /// `>H#`: the identity string.
    Identity,
    /// `>V#`: the firmware version.
    Firmware,
    /// `>G#`: the status frame.
    Status,
    /// A set command, followed by `>G#` so the frame confirms it.
    Set(SetCommand),
}

impl GeminiCommand {
    /// The command as it goes on the wire, without the trailing newline.
    /// A set is the set command and `>G#`, each on its own line.
    #[must_use]
    pub fn wire(&self) -> String {
        match self {
            Self::Identity => ">H#".to_string(),
            Self::Firmware => ">V#".to_string(),
            Self::Status => ">G#".to_string(),
            Self::Set(set) => format!("{}\n>G#", set.wire()),
        }
    }
}

/// One dew channel's three wire quantities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DewChannelState {
    /// The `>Z<c>x#` flag: whether the channel is enabled.
    pub enabled: bool,
    /// The `>M<c>x#` mode.
    pub mode: DewMode,
    /// The duty the frame reports, in percent. Published as read; the frame
    /// decides what it means in each mode.
    pub duty: u8,
}

/// The `>G#` status frame, parsed.
///
/// Every numeric field is published as the frame carries it — nothing here
/// clamps or range-checks a reading — so an abnormal value reaches the
/// operator instead of hiding behind a limit.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusFrame {
    /// DC2 to DC5, on or off (field 0).
    pub dc: [bool; DC_OUTPUT_COUNT],
    /// USB A to F, on or off (field 1).
    pub usb: [bool; USB_PORT_COUNT],
    /// Whether the AHT20 ambient sensor is attached (field 2).
    pub ambient_sensor_attached: bool,
    /// Whether the DS18B20 lens probe is attached (field 3).
    pub lens_probe_attached: bool,
    /// DEW6 and DEW7 (fields 4 to 9).
    pub dew: [DewChannelState; DEW_CHANNEL_COUNT],
    /// Lens temperature from the DS18B20, °C (field 10).
    pub lens_temperature: f64,
    /// Ambient temperature from the AHT20, °C (field 11).
    pub ambient_temperature: f64,
    /// Relative humidity from the AHT20, % (field 12).
    pub humidity: f64,
    /// Dew point the hub computes, °C (field 13).
    pub dew_point: f64,
    /// Input voltage, V (field 14).
    pub input_voltage: f64,
    /// Total output current, A (field 15).
    pub output_current: f64,
    /// Instantaneous output power, W (field 16).
    pub output_power: f64,
}

impl StatusFrame {
    /// Whether `output` is on.
    #[must_use]
    pub fn dc_on(&self, output: DcOutput) -> bool {
        self.dc.get(output.index()).copied().unwrap_or(false)
    }

    /// Whether `port` is on.
    #[must_use]
    pub fn usb_on(&self, port: UsbPort) -> bool {
        self.usb.get(port.index()).copied().unwrap_or(false)
    }

    /// `channel`'s three quantities.
    #[must_use]
    pub fn dew(&self, channel: DewChannel) -> DewChannelState {
        self.dew
            .get(channel.index())
            .copied()
            .unwrap_or(DewChannelState {
                enabled: false,
                mode: DewMode::Manual,
                duty: 0,
            })
    }

    /// Whether the frame shows `set` applied: the one field the write
    /// addressed, compared with what was asked.
    #[must_use]
    pub fn confirms(&self, set: &SetCommand) -> bool {
        match *set {
            SetCommand::Dc(output, on) => self.dc_on(output) == on,
            SetCommand::Usb(port, on) => self.usb_on(port) == on,
            SetCommand::DewDuty(channel, duty) => self.dew(channel).duty == duty.percent(),
            SetCommand::DewEnabled(channel, enabled) => self.dew(channel).enabled == enabled,
            SetCommand::DewMode(channel, mode) => self.dew(channel).mode == mode,
        }
    }

    /// The value the frame reports for the field `set` addresses, as the
    /// switch publishes it — what a write that was not confirmed names.
    #[must_use]
    pub fn reported(&self, set: &SetCommand) -> f64 {
        match *set {
            SetCommand::Dc(output, _) => f64::from(u8::from(self.dc_on(output))),
            SetCommand::Usb(port, _) => f64::from(u8::from(self.usb_on(port))),
            SetCommand::DewDuty(channel, _) => f64::from(self.dew(channel).duty),
            SetCommand::DewEnabled(channel, _) => f64::from(u8::from(self.dew(channel).enabled)),
            SetCommand::DewMode(channel, _) => f64::from(self.dew(channel).mode.digit()),
        }
    }
}

/// Sequential reader over the frame's tagged fields.
///
/// Each field is the text up to the next ASCII upper-case letter, which must
/// be the tag the layout expects there. Reading in order is what makes the
/// repeated tags unambiguous, and it means a parse error always names the
/// field that was bad.
struct Fields<'a> {
    rest: &'a str,
    frame: &'a str,
}

impl<'a> Fields<'a> {
    const fn new(frame: &'a str) -> Self {
        Self { rest: frame, frame }
    }

    /// The raw text of the next field, whose tag must be `tag`.
    fn raw(&mut self, name: &str, tag: char) -> Result<&'a str> {
        let Some(at) = self.rest.find(|c: char| c.is_ascii_uppercase()) else {
            return Err(GeminiError::ParseError(format!(
                "frame ended before {name} ({tag}): {}",
                self.frame
            )));
        };
        let (value, tagged) = self.rest.split_at(at);
        let mut chars = tagged.chars();
        let found = chars.next().unwrap_or('?');
        if found != tag {
            return Err(GeminiError::ParseError(format!(
                "expected {name} tagged {tag}, found tag {found} after {value:?}: {}",
                self.frame
            )));
        }
        self.rest = chars.as_str();
        Ok(value)
    }

    /// A run of exactly `N` `0`/`1` digits.
    fn flags<const N: usize>(&mut self, name: &str, tag: char) -> Result<[bool; N]> {
        let raw = self.raw(name, tag)?;
        if raw.len() != N {
            return Err(GeminiError::ParseError(format!(
                "{name}: expected {N} flags, got {} in {raw:?}",
                raw.len()
            )));
        }
        let mut out = [false; N];
        for (slot, ch) in out.iter_mut().zip(raw.chars()) {
            *slot = flag(name, ch)?;
        }
        Ok(out)
    }

    /// A single `0`/`1` digit.
    fn flag(&mut self, name: &str, tag: char) -> Result<bool> {
        let raw = self.raw(name, tag)?;
        let mut chars = raw.chars();
        match (chars.next(), chars.next()) {
            (Some(ch), None) => flag(name, ch),
            _ => Err(GeminiError::ParseError(format!(
                "{name}: expected one digit, got {raw:?}"
            ))),
        }
    }

    /// A single mode digit.
    fn mode(&mut self, name: &str, tag: char) -> Result<DewMode> {
        let raw = self.raw(name, tag)?;
        raw.parse::<u8>()
            .ok()
            .and_then(DewMode::from_digit)
            .ok_or_else(|| {
                GeminiError::ParseError(format!("{name}: expected a mode 0-2, got {raw:?}"))
            })
    }

    /// An unsigned integer: the duty fields.
    fn integer(&mut self, name: &str, tag: char) -> Result<u8> {
        let raw = self.raw(name, tag)?;
        raw.trim()
            .parse::<u8>()
            .map_err(|_| GeminiError::ParseError(format!("{name}: expected a number, got {raw:?}")))
    }

    /// A decimal number, leading spaces allowed: the readings.
    fn number(&mut self, name: &str, tag: char) -> Result<f64> {
        let raw = self.raw(name, tag)?;
        let value: f64 = raw.trim().parse().map_err(|_| {
            GeminiError::ParseError(format!("{name}: expected a number, got {raw:?}"))
        })?;
        if !value.is_finite() {
            return Err(GeminiError::ParseError(format!(
                "{name}: expected a finite number, got {raw:?}"
            )));
        }
        Ok(value)
    }

    /// Nothing may follow the last field.
    fn finish(self) -> Result<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(GeminiError::ParseError(format!(
                "frame has {:?} after its last field: {}",
                self.rest, self.frame
            )))
        }
    }
}

fn flag(name: &str, ch: char) -> Result<bool> {
    match ch {
        '0' => Ok(false),
        '1' => Ok(true),
        other => Err(GeminiError::ParseError(format!(
            "{name}: expected a 0 or 1 flag, got {other:?}"
        ))),
    }
}

impl std::str::FromStr for StatusFrame {
    type Err = GeminiError;

    /// Parse the payload of a `*G…#` reply: the text between `*G` and `#`.
    fn from_str(payload: &str) -> Result<Self> {
        let mut f = Fields::new(payload);
        let dc = f.flags("DC2-DC5 state", 'D')?;
        let usb = f.flags("USB A-F state", 'U')?;
        let ambient_sensor_attached = f.flag("AHT20 attached", 'A')?;
        let lens_probe_attached = f.flag("DS18B20 attached", 'T')?;
        let dew6_enabled = f.flag("DEW6 enabled", 'A')?;
        let dew6_mode = f.mode("DEW6 mode", 'M')?;
        let dew7_enabled = f.flag("DEW7 enabled", 'B')?;
        let dew7_mode = f.mode("DEW7 mode", 'M')?;
        let dew6_duty = f.integer("DEW6 duty", 'C')?;
        let dew7_duty = f.integer("DEW7 duty", 'C')?;
        let lens_temperature = f.number("lens temperature", 'S')?;
        let ambient_temperature = f.number("ambient temperature", 'T')?;
        let humidity = f.number("humidity", 'H')?;
        let dew_point = f.number("dew point", 'D')?;
        let input_voltage = f.number("input voltage", 'V')?;
        let output_current = f.number("output current", 'C')?;
        let output_power = f.number("output power", 'P')?;
        f.finish()?;
        Ok(Self {
            dc,
            usb,
            ambient_sensor_attached,
            lens_probe_attached,
            dew: [
                DewChannelState {
                    enabled: dew6_enabled,
                    mode: dew6_mode,
                    duty: dew6_duty,
                },
                DewChannelState {
                    enabled: dew7_enabled,
                    mode: dew7_mode,
                    duty: dew7_duty,
                },
            ],
            lens_temperature,
            ambient_temperature,
            humidity,
            dew_point,
            input_voltage,
            output_current,
            output_power,
        })
    }
}

/// Parse the payload of a `*V…#` reply: a three-digit firmware integer,
/// `308` for 3.0.8.
///
/// # Errors
///
/// Returns [`GeminiError::ParseError`] when the payload is not a number.
pub fn parse_firmware(payload: &str) -> Result<u16> {
    payload.trim().parse().map_err(|_| {
        GeminiError::ParseError(format!("firmware: expected a number, got {payload:?}"))
    })
}

/// Render a firmware integer the way the vendor writes it: 308 is 3.0.8.
pub struct FirmwareVersion(pub u16);

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0.to_string();
        let mut parts = digits.chars().map(|c| c.to_string()).collect::<Vec<_>>();
        if parts.len() < 3 {
            // Not a version this driver has met; keep the raw number rather
            // than invent a dotted form for it.
            return write!(f, "{}", self.0);
        }
        let patch = parts.split_off(parts.len().saturating_sub(1));
        let minor = parts.split_off(parts.len().saturating_sub(1));
        write!(
            f,
            "{}.{}.{}",
            parts.concat(),
            minor.concat(),
            patch.concat()
        )
    }
}

/// Refuse a `>H#` reply that is not the Advanced 3, naming what answered.
///
/// # Errors
///
/// Returns [`GeminiError::WrongModel`] for any identity but [`IDENTITY`]:
/// another Gemini power box, the vendor's flat panel, or an unknown `*H`
/// text quoted as it came.
pub fn check_identity(identity: &str) -> Result<()> {
    if identity == IDENTITY {
        return Ok(());
    }
    let what = if identity.starts_with("GeminiPowerBox") {
        "a Gemini power box, but not the Advanced 3"
    } else if identity.starts_with("GeminiFlatPanel") {
        "Gemini's flat panel, not a power box"
    } else {
        "not a Gemini Power & Data Hubs Advanced 3"
    };
    Err(GeminiError::WrongModel(format!(
        "device answered *H{identity}# — {what}"
    )))
}

/// Gate the firmware: below [`MIN_FIRMWARE`] the frame layout is unknown
/// and the connect is refused.
///
/// # Errors
///
/// Returns [`GeminiError::FirmwareTooOld`] naming the version read and the
/// vendor's upgrade tool.
pub fn check_firmware(firmware: u16) -> Result<()> {
    if firmware < MIN_FIRMWARE {
        return Err(GeminiError::FirmwareTooOld {
            got: FirmwareVersion(firmware).to_string(),
            min: FirmwareVersion(MIN_FIRMWARE).to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The frame captured from the dev box's unit on 2026-10-26 (firmware
    /// 308), payload only: the text between `*G` and `#`.
    const CAPTURED_PAYLOAD: &str =
        "1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P";

    // ---- commands --------------------------------------------------------

    #[test]
    fn the_three_queries_serialize_to_their_letters() {
        assert_eq!(GeminiCommand::Identity.wire(), ">H#");
        assert_eq!(GeminiCommand::Firmware.wire(), ">V#");
        assert_eq!(GeminiCommand::Status.wire(), ">G#");
    }

    #[test]
    fn dc_outputs_serialize_to_outputs_two_through_five() {
        let on: Vec<String> = DcOutput::ALL
            .iter()
            .map(|o| SetCommand::Dc(*o, true).wire())
            .collect();
        assert_eq!(on, [">O2#", ">O3#", ">O4#", ">O5#"]);
        assert_eq!(SetCommand::Dc(DcOutput::Dc3, false).wire(), ">C3#");
    }

    #[test]
    fn usb_ports_serialize_to_outputs_six_through_eleven() {
        let off: Vec<String> = UsbPort::ALL
            .iter()
            .map(|p| SetCommand::Usb(*p, false).wire())
            .collect();
        assert_eq!(off, [">C6#", ">C7#", ">C8#", ">C9#", ">C10#", ">C11#"]);
        assert_eq!(SetCommand::Usb(UsbPort::A, true).wire(), ">O6#");
    }

    #[test]
    fn no_command_addresses_output_one() {
        // DC1 is always on; nothing in the command set can name it.
        for output in DcOutput::ALL {
            assert_ne!(output.wire_number(), 1);
        }
        for port in UsbPort::ALL {
            assert_ne!(port.wire_number(), 1);
        }
    }

    #[test]
    fn dew_duty_serializes_to_x_and_y_by_channel() {
        let fifty = DewDuty::from_switch_value(50.0).unwrap();
        assert_eq!(SetCommand::DewDuty(DewChannel::Dew6, fifty).wire(), ">X50#");
        assert_eq!(SetCommand::DewDuty(DewChannel::Dew7, fifty).wire(), ">Y50#");
    }

    #[test]
    fn dew_enabled_serializes_to_z_with_the_wire_channel_number() {
        // DEW6 is channel 1 and DEW7 channel 2 on the wire.
        assert_eq!(
            SetCommand::DewEnabled(DewChannel::Dew6, true).wire(),
            ">Z11#"
        );
        assert_eq!(
            SetCommand::DewEnabled(DewChannel::Dew7, false).wire(),
            ">Z20#"
        );
    }

    #[test]
    fn dew_mode_serializes_to_m_with_the_mode_digit() {
        assert_eq!(
            SetCommand::DewMode(DewChannel::Dew6, DewMode::Auto).wire(),
            ">M10#"
        );
        assert_eq!(
            SetCommand::DewMode(DewChannel::Dew7, DewMode::Switch).wire(),
            ">M22#"
        );
    }

    #[test]
    fn a_set_is_followed_by_a_status_request_on_the_next_line() {
        let cmd = GeminiCommand::Set(SetCommand::Dc(DcOutput::Dc3, true));
        assert_eq!(cmd.wire(), ">O3#\n>G#");
    }

    // ---- value newtypes --------------------------------------------------

    #[test]
    fn dew_duty_accepts_whole_percentages_from_zero_to_a_hundred() {
        assert_eq!(DewDuty::from_switch_value(0.0).unwrap().percent(), 0);
        assert_eq!(DewDuty::from_switch_value(100.0).unwrap().percent(), 100);
    }

    #[test]
    fn dew_duty_rejects_out_of_range_and_non_finite_values() {
        for bad in [-1.0, 101.0, 100.4, f64::NAN, f64::INFINITY] {
            let err = DewDuty::from_switch_value(bad).unwrap_err();
            assert!(
                matches!(err, GeminiError::InvalidValue(_)),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn a_value_between_steps_rounds_to_the_nearest_whole_percent() {
        // ConformU writes the quarter points of every range and expects
        // them taken; the row's step is 1, so they round, half up.
        assert_eq!(DewDuty::from_switch_value(50.5).unwrap().percent(), 51);
        assert_eq!(DewDuty::from_switch_value(0.25).unwrap().percent(), 0);
        assert_eq!(DewDuty::from_switch_value(99.75).unwrap().percent(), 100);
    }

    #[test]
    fn dew_mode_round_trips_its_digits_and_rejects_the_rest() {
        for mode in [DewMode::Auto, DewMode::Manual, DewMode::Switch] {
            assert_eq!(DewMode::from_digit(mode.digit()), Some(mode));
            assert_eq!(
                DewMode::from_switch_value(f64::from(mode.digit())).unwrap(),
                mode
            );
        }
        assert_eq!(DewMode::from_digit(3), None);
        for bad in [3.0, -1.0, 2.6, f64::NAN] {
            let err = DewMode::from_switch_value(bad).unwrap_err();
            assert!(
                matches!(err, GeminiError::InvalidValue(_)),
                "{bad}: {err:?}"
            );
        }
        // Between steps: the nearest mode, half away from zero.
        assert_eq!(DewMode::from_switch_value(0.25).unwrap(), DewMode::Auto);
        assert_eq!(DewMode::from_switch_value(0.5).unwrap(), DewMode::Manual);
        assert_eq!(DewMode::from_switch_value(1.5).unwrap(), DewMode::Switch);
    }

    // ---- the status frame ------------------------------------------------

    #[test]
    fn the_captured_frame_parses_field_for_field() {
        let frame: StatusFrame = CAPTURED_PAYLOAD.parse().unwrap();
        assert_eq!(frame.dc, [true; 4]);
        assert_eq!(frame.usb, [true; 6]);
        assert!(frame.ambient_sensor_attached);
        assert!(frame.lens_probe_attached);
        for channel in DewChannel::ALL {
            let state = frame.dew(channel);
            assert!(state.enabled, "{channel:?}");
            assert_eq!(state.mode, DewMode::Manual, "{channel:?}");
            assert_eq!(state.duty, 100, "{channel:?}");
        }
        assert_eq!(frame.lens_temperature, 23.25);
        assert_eq!(frame.ambient_temperature, 24.47);
        assert_eq!(frame.humidity, 67.76);
        assert_eq!(frame.dew_point, 18.12);
        assert_eq!(frame.input_voltage, 12.2);
        assert_eq!(frame.output_current, 0.02);
        assert_eq!(frame.output_power, 0.36);
    }

    #[test]
    fn the_sensor_trio_is_the_aht20_and_s_is_the_probe() {
        // Magnus against 24.47 °C / 67.76 % RH gives 18.1 °C, which is what
        // field 13 says: T/H/D belong together, so S is the lens probe.
        let frame: StatusFrame = CAPTURED_PAYLOAD.parse().unwrap();
        let t = frame.ambient_temperature;
        let rh = frame.humidity;
        let gamma = (17.62 * t) / (243.12 + t) + (rh / 100.0).ln();
        let magnus = 243.12 * gamma / (17.62 - gamma);
        assert!(
            (magnus - frame.dew_point).abs() < 0.1,
            "Magnus {magnus} vs frame {}",
            frame.dew_point
        );
    }

    #[test]
    fn fields_are_read_in_order_not_by_tag_lookup() {
        // DEW6 duty 25, DEW7 duty 75, output current 1.50: three `C` fields
        // that a tag lookup could not tell apart.
        let payload = CAPTURED_PAYLOAD
            .replace("100C100C", "25C75C")
            .replace(" 0.02C", " 1.50C");
        let frame: StatusFrame = payload.parse().unwrap();
        assert_eq!(frame.dew(DewChannel::Dew6).duty, 25);
        assert_eq!(frame.dew(DewChannel::Dew7).duty, 75);
        assert_eq!(frame.output_current, 1.5);
    }

    #[test]
    fn output_and_port_states_are_read_per_slot() {
        let payload = CAPTURED_PAYLOAD.replace("1111D111111U", "1010D100001U");
        let frame: StatusFrame = payload.parse().unwrap();
        assert!(frame.dc_on(DcOutput::Dc2));
        assert!(!frame.dc_on(DcOutput::Dc3));
        assert!(frame.dc_on(DcOutput::Dc4));
        assert!(!frame.dc_on(DcOutput::Dc5));
        assert!(frame.usb_on(UsbPort::A));
        assert!(!frame.usb_on(UsbPort::B));
        assert!(frame.usb_on(UsbPort::F));
    }

    #[test]
    fn the_two_dew_channels_keep_their_own_mode_and_flag() {
        // DEW6 disabled in Auto, DEW7 enabled in Switch.
        let payload = CAPTURED_PAYLOAD.replace("1A1M1B1M", "0A0M1B2M");
        let frame: StatusFrame = payload.parse().unwrap();
        let six = frame.dew(DewChannel::Dew6);
        let seven = frame.dew(DewChannel::Dew7);
        assert!(!six.enabled);
        assert_eq!(six.mode, DewMode::Auto);
        assert!(seven.enabled);
        assert_eq!(seven.mode, DewMode::Switch);
    }

    #[test]
    fn an_absent_sensor_is_a_zero_flag() {
        let payload = CAPTURED_PAYLOAD.replace("1A1T1A", "0A1T1A");
        let frame: StatusFrame = payload.parse().unwrap();
        assert!(!frame.ambient_sensor_attached);
        assert!(frame.lens_probe_attached);
    }

    #[test]
    fn a_reading_outside_its_switch_range_is_parsed_as_read() {
        // 24 V on the input: the row's ceiling is 19, and the parser does
        // not know or care.
        let payload = CAPTURED_PAYLOAD.replace("12.2V", "24.0V");
        let frame: StatusFrame = payload.parse().unwrap();
        assert_eq!(frame.input_voltage, 24.0);
    }

    #[test]
    fn a_negative_temperature_parses() {
        let payload = CAPTURED_PAYLOAD.replace(" 23.25S", "-12.50S");
        let frame: StatusFrame = payload.parse().unwrap();
        assert_eq!(frame.lens_temperature, -12.5);
    }

    #[test]
    fn a_tag_out_of_sequence_names_the_field() {
        // The USB field tagged D instead of U.
        let payload = CAPTURED_PAYLOAD.replace("111111U", "111111D");
        let err = payload.parse::<StatusFrame>().unwrap_err();
        let GeminiError::ParseError(msg) = err else {
            panic!("expected ParseError, got {err:?}");
        };
        assert!(msg.contains("USB A-F state"), "{msg}");
    }

    #[test]
    fn a_flag_run_of_the_wrong_width_is_rejected() {
        let payload = CAPTURED_PAYLOAD.replace("1111D", "111D");
        let err = payload.parse::<StatusFrame>().unwrap_err();
        assert!(matches!(err, GeminiError::ParseError(_)), "{err:?}");
        assert!(err.to_string().contains("DC2-DC5"), "{err}");
    }

    #[test]
    fn a_bad_number_names_the_field() {
        let payload = CAPTURED_PAYLOAD.replace(" 67.76H", " 6x.76H");
        let err = payload.parse::<StatusFrame>().unwrap_err();
        assert!(err.to_string().contains("humidity"), "{err}");
    }

    #[test]
    fn a_truncated_frame_names_the_field_it_ran_out_before() {
        let (payload, _) = CAPTURED_PAYLOAD.split_once('S').unwrap();
        let err = payload.parse::<StatusFrame>().unwrap_err();
        assert!(err.to_string().contains("lens temperature"), "{err}");
    }

    #[test]
    fn trailing_text_after_the_last_field_is_rejected() {
        let payload = format!("{CAPTURED_PAYLOAD}1Q");
        let err = payload.parse::<StatusFrame>().unwrap_err();
        assert!(matches!(err, GeminiError::ParseError(_)), "{err:?}");
    }

    #[test]
    fn a_mode_digit_above_two_is_rejected() {
        let payload = CAPTURED_PAYLOAD.replace("1A1M1B", "1A3M1B");
        let err = payload.parse::<StatusFrame>().unwrap_err();
        assert!(err.to_string().contains("DEW6 mode"), "{err}");
    }

    // ---- confirmation ----------------------------------------------------

    #[test]
    fn a_frame_confirms_the_field_a_write_addressed_and_nothing_else() {
        let frame: StatusFrame = CAPTURED_PAYLOAD.parse().unwrap();
        assert!(frame.confirms(&SetCommand::Dc(DcOutput::Dc3, true)));
        assert!(!frame.confirms(&SetCommand::Dc(DcOutput::Dc3, false)));
        assert!(frame.confirms(&SetCommand::Usb(UsbPort::F, true)));
        assert!(frame.confirms(&SetCommand::DewEnabled(DewChannel::Dew7, true)));
        assert!(frame.confirms(&SetCommand::DewMode(DewChannel::Dew6, DewMode::Manual)));
        assert!(!frame.confirms(&SetCommand::DewMode(DewChannel::Dew6, DewMode::Auto)));
        let hundred = DewDuty::from_switch_value(100.0).unwrap();
        let fifty = DewDuty::from_switch_value(50.0).unwrap();
        assert!(frame.confirms(&SetCommand::DewDuty(DewChannel::Dew6, hundred)));
        assert!(!frame.confirms(&SetCommand::DewDuty(DewChannel::Dew6, fifty)));
    }

    #[test]
    fn the_reported_value_is_what_the_switch_would_publish() {
        let frame: StatusFrame = CAPTURED_PAYLOAD.parse().unwrap();
        assert_eq!(frame.reported(&SetCommand::Dc(DcOutput::Dc2, false)), 1.0);
        assert_eq!(
            frame.reported(&SetCommand::DewMode(DewChannel::Dew7, DewMode::Auto)),
            1.0
        );
        let zero = DewDuty::from_switch_value(0.0).unwrap();
        assert_eq!(
            frame.reported(&SetCommand::DewDuty(DewChannel::Dew7, zero)),
            100.0
        );
    }

    // ---- identity and firmware -------------------------------------------

    #[test]
    fn the_advanced_3_identity_is_accepted_verbatim() {
        check_identity(IDENTITY).unwrap();
    }

    #[test]
    fn another_gemini_power_box_is_refused_by_family() {
        let err = check_identity("GeminiPowerBoxMiniV2").unwrap_err();
        let GeminiError::WrongModel(msg) = err else {
            panic!("expected WrongModel, got {err:?}");
        };
        assert!(msg.contains("not the Advanced 3"), "{msg}");
        assert!(msg.contains("GeminiPowerBoxMiniV2"), "{msg}");
    }

    #[test]
    fn the_flat_panel_is_refused_by_name() {
        let err = check_identity("GeminiFlatPanelV3").unwrap_err();
        assert!(err.to_string().contains("flat panel"), "{err}");
    }

    #[test]
    fn an_unknown_identity_is_quoted_as_it_came() {
        let err = check_identity("SomethingElse").unwrap_err();
        assert!(err.to_string().contains("*HSomethingElse#"), "{err}");
    }

    #[test]
    fn identity_comparison_is_exact() {
        // A prefix match would accept a future box that shares the prefix
        // and a frame this driver has never seen.
        check_identity("GeminiPowerBoxPlusAdv3X").unwrap_err();
        check_identity("geminipowerboxplusadv3").unwrap_err();
    }

    #[test]
    fn firmware_parses_the_three_digit_integer() {
        assert_eq!(parse_firmware("308").unwrap(), 308);
        assert!(parse_firmware("3.0.8").is_err());
    }

    #[test]
    fn firmware_below_308_is_refused_naming_the_version_and_the_tool() {
        let err = check_firmware(307).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("3.0.7"), "{text}");
        assert!(text.contains(FIRMWARE_UPGRADE_TOOL), "{text}");
        check_firmware(308).unwrap();
        check_firmware(309).unwrap();
    }

    #[test]
    fn firmware_renders_dotted() {
        assert_eq!(FirmwareVersion(308).to_string(), "3.0.8");
        assert_eq!(FirmwareVersion(1012).to_string(), "10.1.2");
        assert_eq!(FirmwareVersion(42).to_string(), "42");
    }

    // ---- channel newtypes ------------------------------------------------

    #[test]
    fn channel_indexes_are_contiguous_from_zero() {
        assert_eq!(DcOutput::ALL.map(DcOutput::index), [0, 1, 2, 3]);
        assert_eq!(UsbPort::ALL.map(UsbPort::index), [0, 1, 2, 3, 4, 5]);
        assert_eq!(DewChannel::ALL.map(DewChannel::index), [0, 1]);
    }

    #[test]
    fn only_a_and_b_are_usb3() {
        for port in UsbPort::ALL {
            assert_eq!(
                port.is_usb3(),
                matches!(port, UsbPort::A | UsbPort::B),
                "{port:?}"
            );
        }
    }
}

//! Mock `UPBv2` transport for testing without real hardware.
//!
//! Provides a [`TransportFactory`] that hands out a [`FrameTransport`]
//! backed by an in-memory `UPBv2` state machine. Persists state across
//! reconnects so tests can disconnect/reconnect and still observe their
//! prior writes (matches the behaviour of real hardware that doesn't
//! lose its settings when an ASCOM client cycles `Connected`).
//!
//! The simulated device models everything the driver reads back: four 12 V
//! outputs, three dew duties, six USB ports, the variable-output setpoint,
//! the auto-dew mask and the overcurrent flags. Writes mutate that state, so
//! a write-then-read round trip works the same way it does on the bench.
//!
//! Two knobs cannot be reached through the driver's own command set — the
//! driver never sends `PD:`, and nothing can ask a healthy box to trip a
//! rail. Both are therefore preset from the environment
//! ([`ENV_AUTO_DEW`], [`ENV_OVERCURRENT`]), which is the only channel a BDD
//! harness has into a binary it launches as a subprocess.

// `#[cfg(any(feature = "mock", test))]`-gated test-helper infrastructure
// that never ships in production builds. Excluded from coverage so the
// workspace coverage number reflects only production-shipped code —
// counting these never-shipped mock lines would produce false coverage
// figures.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tracing::debug;

use crate::protocol::{DEW_COUNT, OUTPUT_COUNT, OVERCURRENT_COUNT, PING_OK, USB_COUNT};

/// Environment variable that presets the simulated auto-dew mask.
///
/// Parsed as the raw 0-7 value the device reports in `PA` field 20. The
/// driver never writes `PD:`, so this is the only way a test can put a dew
/// channel under auto-dew control and exercise the per-channel `CanWrite`
/// gate from the read-only side.
pub const ENV_AUTO_DEW: &str = "UPBV2_MOCK_AUTO_DEW";

/// Environment variable that presets the simulated overcurrent flags.
///
/// Takes the seven-character `PA` field verbatim — outputs 1-4 then dew
/// A-C, `1` meaning tripped. `0100000` trips 12 V output 2.
pub const ENV_OVERCURRENT: &str = "UPBV2_MOCK_OVERCURRENT";

/// The `PA` prefix this mock emits. Written out rather than taken from
/// `protocol::STATUS_PREFIXES` because the parser accepts either spelling
/// and the mock has to pick exactly one; this is the one the vendor's field
/// legend gives.
const STATUS_PREFIX: &str = "UPB2:";

/// The `PV` reply. The wire form is a bare `n.n`, not an echo.
const FIRMWARE_VERSION: &str = "1.5";

/// What the device answers to a command it does not implement.
const ERROR_REPLY: &str = "ERR";

/// Highest auto-dew mask the device reports.
const AUTO_DEW_MASK_MAX: u8 = 7;

/// Auto-dew off — the default, so the dew heaters stay writable and a
/// `ConformU` run's dew-write tests pass without any preset.
const DEFAULT_AUTO_DEW: u8 = 0;

/// Nothing tripped — the default overcurrent field.
const DEFAULT_OVERCURRENT: [bool; OVERCURRENT_COUNT] = [false; OVERCURRENT_COUNT];

/// The `P<n>:` command number of dew channel A. B and C follow it.
const DEW_COMMAND_BASE: usize = 5;

/// In-memory `UPBv2` device state, plus a queue of responses each accepted
/// command appended.
#[derive(Debug, Default)]
struct MockState {
    response_queue: VecDeque<Vec<u8>>,
    device_state: MockDeviceState,
}

/// The simulated device's settings and readings.
///
/// The current fields hold **raw sense counts**, exactly as the wire carries
/// them: the parser divides outputs and dew A/B by 480 and dew C by 700, so
/// a mock that stored Amps would hide the divisor bug it exists to catch.
#[derive(Debug, Clone)]
struct MockDeviceState {
    /// On/off state of the four 12 V outputs (`PA` field 7).
    outputs: [bool; OUTPUT_COUNT],
    /// On/off state of the six USB ports (`PA` field 8).
    usb_ports: [bool; USB_COUNT],
    /// PWM duty of the three dew channels (`PA` fields 9-11).
    dew_duty: [u8; DEW_COUNT],
    /// Variable-output setpoint in Volts. Reported by `PS`, never by `PA`.
    variable_volts: u8,
    /// Auto-dew channel mask (`PA` field 20). Preset only — see
    /// [`ENV_AUTO_DEW`].
    auto_dew: u8,
    /// Power-on-boot state of the four outputs, the `PS` field the driver
    /// parses and discards.
    boot_outputs: [bool; OUTPUT_COUNT],
    voltage: f64,
    current: f64,
    power: u32,
    temperature: f64,
    humidity: f64,
    dewpoint: f64,
    /// Per-output raw sense counts; the parser divides these by 480.
    output_current: [u32; OUTPUT_COUNT],
    /// Per-dew-channel raw sense counts; A and B divide by 480, C by 700.
    dew_current: [u32; DEW_COUNT],
    /// Overcurrent flags, outputs 1-4 then dew A-C. Preset only — see
    /// [`ENV_OVERCURRENT`].
    overcurrent: [bool; OVERCURRENT_COUNT],
    average_amps: f64,
    amp_hours: f64,
    watt_hours: f64,
    uptime: Duration,
}

impl Default for MockDeviceState {
    fn default() -> Self {
        Self {
            // Output 3 off and USB port 5 off: a default frame where every
            // flag reads `1` cannot tell a working parser from one that
            // returns a constant.
            outputs: [true, true, false, true],
            usb_ports: [true, true, true, true, false, true],
            dew_duty: [128, 64, 0],
            variable_volts: 12,
            auto_dew: env_auto_dew(),
            boot_outputs: [true, true, false, true],
            voltage: 12.5,
            current: 2.4,
            power: 30,
            temperature: 25.0,
            humidity: 60.0,
            dewpoint: 16.5,
            // 1.0 A, 2.0 A, 0.0 A, 0.5 A once divided by 480.
            output_current: [480, 960, 0, 240],
            // 0.5 A and 0.2 A on A/B (÷480); 350 on C is 0.5 A only
            // because C divides by 700 — the value that proves the
            // per-channel divisor is applied.
            dew_current: [240, 96, 350],
            overcurrent: env_overcurrent(),
            average_amps: 1.85,
            amp_hours: 0.42,
            watt_hours: 5.1,
            uptime: Duration::from_hours(1),
        }
    }
}

/// Read the auto-dew preset from the environment, falling back to "off".
fn env_auto_dew() -> u8 {
    std::env::var(ENV_AUTO_DEW)
        .ok()
        .as_deref()
        .and_then(parse_auto_dew)
        .unwrap_or(DEFAULT_AUTO_DEW)
}

/// Read the overcurrent preset from the environment, falling back to
/// "nothing tripped".
fn env_overcurrent() -> [bool; OVERCURRENT_COUNT] {
    std::env::var(ENV_OVERCURRENT)
        .ok()
        .as_deref()
        .and_then(parse_overcurrent)
        .unwrap_or(DEFAULT_OVERCURRENT)
}

/// Parse an [`ENV_AUTO_DEW`] value: the raw 0-7 mask.
///
/// A typo yields `None` and the caller falls back to auto-dew off, which
/// leaves every dew channel writable — a preset that silently gated the dew
/// writes would make a feature file fail somewhere far from the typo.
fn parse_auto_dew(raw: &str) -> Option<u8> {
    raw.trim()
        .parse::<u8>()
        .ok()
        .filter(|mask| *mask <= AUTO_DEW_MASK_MAX)
}

/// Parse an [`ENV_OVERCURRENT`] value: the seven-character `PA` flag run,
/// outputs 1-4 then dew A-C.
fn parse_overcurrent(raw: &str) -> Option<[bool; OVERCURRENT_COUNT]> {
    let raw = raw.trim();
    if raw.chars().count() != OVERCURRENT_COUNT {
        return None;
    }
    let mut flags = DEFAULT_OVERCURRENT;
    for (slot, ch) in flags.iter_mut().zip(raw.chars()) {
        *slot = match ch {
            '0' => false,
            '1' => true,
            _ => return None,
        };
    }
    Some(flags)
}

/// Render a run of channel states as the `0`/`1` characters `PA` uses.
fn flags(states: &[bool]) -> String {
    states
        .iter()
        .map(|on| if *on { '1' } else { '0' })
        .collect()
}

/// Render a per-channel array as the consecutive colon-separated `PA`
/// tokens it occupies — dew duties, output currents, dew currents.
fn joined<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(":")
}

/// Split a `P3:128` / `U5:1` command into its 1-based channel number and
/// its value. Returns `None` for anything that isn't `<family><n>:<value>`.
fn split_indexed(command: &str, family: char) -> Option<(u8, &str)> {
    let (number, value) = command.strip_prefix(family)?.split_once(':')?;
    Some((number.parse().ok()?, value))
}

impl MockDeviceState {
    /// The `PA` frame: the prefix plus 20 data fields.
    fn status_response(&self) -> String {
        format!(
            "{STATUS_PREFIX}{voltage:.1}:{current:.1}:{power}:{temperature:.1}:{humidity}:\
             {dewpoint:.1}:{ports}:{usb}:{dew_duty}:{output_current}:{dew_current}:\
             {overcurrent}:{auto_dew}",
            voltage = self.voltage,
            current = self.current,
            power = self.power,
            temperature = self.temperature,
            humidity = self.humidity,
            dewpoint = self.dewpoint,
            ports = flags(&self.outputs),
            usb = flags(&self.usb_ports),
            dew_duty = joined(&self.dew_duty),
            output_current = joined(&self.output_current),
            dew_current = joined(&self.dew_current),
            overcurrent = flags(&self.overcurrent),
            auto_dew = self.auto_dew,
        )
    }

    /// The `PC` frame. The vendor documents it as a bare
    /// `avgAmps:ampHours:wattHours:uptime` tuple with no command echo, and
    /// the codec identifies it structurally — so emitting a prefix here
    /// would test a shape the device never sends.
    fn power_response(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.average_amps,
            self.amp_hours,
            self.watt_hours,
            self.uptime.as_millis()
        )
    }

    /// The `PS` frame: boot state and the variable-output setpoint.
    fn boot_state_response(&self) -> String {
        format!("PS:{}:{}", flags(&self.boot_outputs), self.variable_volts)
    }

    fn set_output(&mut self, number: u8, value: &str) -> String {
        let on = value == "1";
        if let Some(slot) = usize::from(number)
            .checked_sub(1)
            .and_then(|index| self.outputs.get_mut(index))
        {
            *slot = on;
        }
        format!("P{number}:{}", u8::from(on))
    }

    fn set_dew(&mut self, number: u8, value: &str) -> String {
        let Ok(duty) = value.parse::<u8>() else {
            return ERROR_REPLY.to_string();
        };
        if let Some(slot) = usize::from(number)
            .checked_sub(DEW_COMMAND_BASE)
            .and_then(|index| self.dew_duty.get_mut(index))
        {
            *slot = duty;
        }
        format!("P{number}:{duty}")
    }

    /// `P8:` is the one write the real device stores in EEPROM; here it just
    /// changes what `PS` reports, which is the only place it is readable.
    fn set_variable_volts(&mut self, value: &str) -> String {
        let Ok(volts) = value.parse::<u8>() else {
            return ERROR_REPLY.to_string();
        };
        self.variable_volts = volts;
        format!("P8:{volts}")
    }

    fn set_usb(&mut self, number: u8, value: &str) -> String {
        let on = value == "1";
        if let Some(slot) = usize::from(number)
            .checked_sub(1)
            .and_then(|index| self.usb_ports.get_mut(index))
        {
            *slot = on;
        }
        format!("U{number}:{}", u8::from(on))
    }

    /// Route a set command to its family, or report `ERR`.
    fn set_command(&mut self, command: &str) -> String {
        match split_indexed(command, 'P') {
            Some((number @ 1..=4, value)) => return self.set_output(number, value),
            Some((number @ 5..=7, value)) => return self.set_dew(number, value),
            Some((8, value)) => return self.set_variable_volts(value),
            _ => {}
        }
        if let Some((number @ 1..=6, value)) = split_indexed(command, 'U') {
            return self.set_usb(number, value);
        }
        debug!(command, "mock: unknown command");
        ERROR_REPLY.to_string()
    }

    fn respond(&mut self, command: &str) -> String {
        match command {
            "P#" => PING_OK.to_string(),
            "PV" => FIRMWARE_VERSION.to_string(),
            "PA" => self.status_response(),
            "PC" => self.power_response(),
            "PS" => self.boot_state_response(),
            other => self.set_command(other),
        }
    }
}

impl MockState {
    fn process_command(&mut self, command_bytes: &[u8]) {
        let command = std::str::from_utf8(command_bytes)
            .unwrap_or_default()
            .trim();
        debug!(
            command,
            outputs = %flags(&self.device_state.outputs),
            usb = %flags(&self.device_state.usb_ports),
            variable_volts = self.device_state.variable_volts,
            auto_dew = self.device_state.auto_dew,
            "mock processing command"
        );

        let response = self.device_state.respond(command);
        let mut frame = response.into_bytes();
        frame.push(b'\n');
        self.response_queue.push_back(frame);
    }
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings survive a reconnect cycle.
struct MockFrameTransport {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.state.lock().await.process_command(bytes);
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        let frame = self
            .state
            .lock()
            .await
            .response_queue
            .pop_front()
            .ok_or(TransportError::Eof)?;
        buf.clear();
        buf.extend_from_slice(&frame);
        Ok(())
    }
}

/// Mock factory for the `UPBv2` transport.
///
/// Maintains persistent device state across multiple open/close cycles so
/// tests can power-cycle the connection without losing the simulated
/// device's settings — matching the behaviour of real hardware.
///
/// [`Default`] reads the two presets from the environment; the constructors
/// below set them in-process for tests that don't launch a binary.
#[derive(Clone, Default)]
pub struct MockUpbv2TransportFactory {
    state: Arc<Mutex<MockState>>,
}

impl MockUpbv2TransportFactory {
    /// A factory whose simulated device reports `mask` as its auto-dew
    /// channel mask, the in-process twin of [`ENV_AUTO_DEW`].
    ///
    /// A mask above 7 is ignored and auto-dew stays off, so a bad value
    /// leaves the dew channels writable instead of gating them from
    /// somewhere the test can't see.
    #[must_use]
    pub fn with_auto_dew(mask: u8) -> Self {
        Self::with_device_state(MockDeviceState {
            auto_dew: if mask <= AUTO_DEW_MASK_MAX {
                mask
            } else {
                DEFAULT_AUTO_DEW
            },
            ..MockDeviceState::default()
        })
    }

    /// A factory whose simulated device reports `flags` as its overcurrent
    /// field — seven `0`/`1` characters, outputs 1-4 then dew A-C. The
    /// in-process twin of [`ENV_OVERCURRENT`]; a malformed string leaves
    /// nothing tripped.
    #[must_use]
    pub fn with_overcurrent(overcurrent: &str) -> Self {
        Self::with_device_state(MockDeviceState {
            overcurrent: parse_overcurrent(overcurrent).unwrap_or(DEFAULT_OVERCURRENT),
            ..MockDeviceState::default()
        })
    }

    fn with_device_state(device_state: MockDeviceState) -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                response_queue: VecDeque::new(),
                device_state,
            })),
        }
    }
}

#[async_trait]
impl TransportFactory for MockUpbv2TransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock UPBv2 transport opened");
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact default `PA` line. Pinned as a constant because the BDD
    /// feature files assert individual fields of it: a change here is a
    /// change to their expected values.
    const DEFAULT_STATUS_LINE: &str =
        "UPB2:12.5:2.4:30:25.0:60:16.5:1101:111101:128:64:0:480:960:0:240:240:96:350:0000000:0";

    /// The exact default `PC` line — a bare tuple, no command echo.
    /// 3600000 ms is one hour, which switch 38 reports as 1.0.
    const DEFAULT_POWER_LINE: &str = "1.85:0.42:5.1:3600000";

    /// The exact default `PS` line: boot state plus the 12 V setpoint.
    const DEFAULT_BOOT_STATE_LINE: &str = "PS:1101:12";

    async fn open(factory: &MockUpbv2TransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    /// Send one command and return the reply with its terminator trimmed.
    async fn exchange(transport: &mut Box<dyn FrameTransport>, command: &str) -> String {
        transport
            .send_frame(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        transport.recv_frame(&mut buf).await.unwrap();
        assert!(buf.ends_with(b"\n"), "reply must be newline-terminated");
        std::str::from_utf8(&buf).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn ping_identifies_the_device_as_a_upbv2() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "P#").await, "UPB2_OK");
    }

    #[tokio::test]
    async fn firmware_version_is_a_bare_number() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "PV").await, FIRMWARE_VERSION);
    }

    #[tokio::test]
    async fn status_frame_matches_the_pinned_default_line() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "PA").await, DEFAULT_STATUS_LINE);
    }

    #[tokio::test]
    async fn status_frame_carries_twenty_one_tokens() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(status.split(':').count(), 21, "PA frame: {status}");
    }

    #[tokio::test]
    async fn power_counters_arrive_as_a_bare_tuple() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        let power = exchange(&mut transport, "PC").await;
        assert_eq!(power, DEFAULT_POWER_LINE);
        assert!(
            !power.starts_with("PC:"),
            "the vendor documents PC without a command echo: {power}"
        );
    }

    #[tokio::test]
    async fn boot_state_reports_the_variable_setpoint() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(
            exchange(&mut transport, "PS").await,
            DEFAULT_BOOT_STATE_LINE
        );
    }

    #[tokio::test]
    async fn setting_an_output_echoes_and_shows_up_in_pa() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "P1:0").await, "P1:0");

        let status = exchange(&mut transport, "PA").await;
        let ports = status.split(':').nth(7).unwrap();
        assert_eq!(ports, "0101", "port status after P1:0 — {status}");
    }

    #[tokio::test]
    async fn setting_a_dew_duty_echoes_and_shows_up_in_pa() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "P7:200").await, "P7:200");

        let status = exchange(&mut transport, "PA").await;
        // Fields 9-11 are the three dew duties; C is the last of them.
        let dew_c = status.split(':').nth(11).unwrap();
        assert_eq!(dew_c, "200", "dew C duty after P7:200 — {status}");
    }

    #[tokio::test]
    async fn setting_a_usb_port_echoes_and_shows_up_in_pa() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "U5:1").await, "U5:1");

        let status = exchange(&mut transport, "PA").await;
        let usb = status.split(':').nth(8).unwrap();
        assert_eq!(usb, "111111", "usb status after U5:1 — {status}");
    }

    #[tokio::test]
    async fn setting_the_variable_voltage_echoes_and_shows_up_in_ps() {
        // PA never carries the setpoint, so PS is the only read-back path.
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "P8:5").await, "P8:5");
        assert_eq!(exchange(&mut transport, "PS").await, "PS:1101:5");
    }

    #[tokio::test]
    async fn a_non_numeric_dew_duty_is_rejected() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "P5:high").await, ERROR_REPLY);
    }

    #[tokio::test]
    async fn an_unknown_command_is_rejected() {
        // PL: is one the driver must never send — the mock refusing it means
        // a regression that starts sending it fails loudly.
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        assert_eq!(exchange(&mut transport, "PL:0").await, ERROR_REPLY);
    }

    #[tokio::test]
    async fn state_persists_across_reopens() {
        let factory = MockUpbv2TransportFactory::default();
        {
            let mut transport = open(&factory).await;
            exchange(&mut transport, "P1:0").await;
        }
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(status.split(':').nth(7).unwrap(), "0101");
    }

    #[tokio::test]
    async fn empty_queue_returns_eof() {
        let factory = MockUpbv2TransportFactory::default();
        let mut transport = open(&factory).await;
        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof));
    }

    #[tokio::test]
    async fn auto_dew_preset_shows_up_in_pa() {
        let factory = MockUpbv2TransportFactory::with_auto_dew(3);
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(
            status.split(':').next_back().unwrap(),
            "3",
            "auto-dew mask — {status}"
        );
    }

    #[tokio::test]
    async fn an_out_of_range_auto_dew_preset_leaves_auto_dew_off() {
        let factory = MockUpbv2TransportFactory::with_auto_dew(9);
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(status.split(':').next_back().unwrap(), "0");
    }

    #[tokio::test]
    async fn overcurrent_preset_shows_up_in_pa() {
        let factory = MockUpbv2TransportFactory::with_overcurrent("0100000");
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(
            status.split(':').nth(19).unwrap(),
            "0100000",
            "overcurrent flags — {status}"
        );
    }

    #[tokio::test]
    async fn a_malformed_overcurrent_preset_trips_nothing() {
        let factory = MockUpbv2TransportFactory::with_overcurrent("01");
        let mut transport = open(&factory).await;
        let status = exchange(&mut transport, "PA").await;
        assert_eq!(status.split(':').nth(19).unwrap(), "0000000");
    }

    #[test]
    fn parse_auto_dew_accepts_the_documented_range() {
        assert_eq!(parse_auto_dew("0"), Some(0));
        assert_eq!(parse_auto_dew(" 7 "), Some(7));
    }

    #[test]
    fn parse_auto_dew_rejects_out_of_range_and_garbage() {
        assert_eq!(parse_auto_dew("8"), None);
        assert_eq!(parse_auto_dew("all"), None);
        assert_eq!(parse_auto_dew(""), None);
    }

    #[test]
    fn parse_overcurrent_accepts_a_seven_flag_run() {
        assert_eq!(
            parse_overcurrent("0100000"),
            Some([false, true, false, false, false, false, false])
        );
    }

    #[test]
    fn parse_overcurrent_rejects_wrong_length_and_bad_characters() {
        assert_eq!(parse_overcurrent("010000"), None);
        assert_eq!(parse_overcurrent("01000000"), None);
        assert_eq!(parse_overcurrent("010000x"), None);
    }
}

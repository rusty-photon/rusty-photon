//! PPBA protocol implementation
//!
//! This module handles the serial protocol for the Pegasus Astro Pocket Powerbox Advance Gen2.
//!
//! Serial Settings: 9600 baud, 8N1, newline-terminated commands
//!
//! Commands return their echo followed by data, terminated by newline.

use std::time::Duration;

use crate::error::{PpbaError, Result};

/// Dew-heater PWM duty, clamped to the device's 0-255 range. Owns the
/// one analog-value (ASCOM switch `f64`) → wire-byte conversion so call
/// sites stay cast-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmDuty(pub u8);

impl From<f64> for PwmDuty {
    #[expect(
        clippy::as_conversions,
        reason = "clamped to u8's exact range on the line above; NaN clamps \
                  through the saturating cast to 0"
    )]
    fn from(value: f64) -> Self {
        Self(value.round().clamp(0.0, 255.0) as u8)
    }
}

/// Commands that can be sent to the PPBA device
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PpbaCommand {
    /// Ping/status check - returns `PPBA_OK`
    Ping,
    /// Get firmware version - returns n.n.n
    FirmwareVersion,
    /// Get full status (PA command)
    Status,
    /// Get power statistics (PS command)
    PowerStats,
    /// Set quad 12V output (0/1)
    SetQuad12V(bool),
    /// Set adjustable output (0=off, 1=on)
    SetAdjustable(bool),
    /// Set Dew Heater A PWM (0-255)
    SetDewA(PwmDuty),
    /// Set Dew Heater B PWM (0-255)
    SetDewB(PwmDuty),
    /// Set USB hub power (0/1)
    SetUsbHub(bool),
    /// Set Auto-Dew enable (0/1)
    SetAutoDew(bool),
}

impl PpbaCommand {
    /// Serialize the command to a string to send to the device
    #[must_use]
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Ping => "P#".to_string(),
            Self::FirmwareVersion => "PV".to_string(),
            Self::Status => "PA".to_string(),
            Self::PowerStats => "PS".to_string(),
            Self::SetQuad12V(on) => format!("P1:{}", i32::from(*on)),
            Self::SetAdjustable(on) => format!("P2:{}", i32::from(*on)),
            Self::SetDewA(pwm) => format!("P3:{}", pwm.0),
            Self::SetDewB(pwm) => format!("P4:{}", pwm.0),
            Self::SetUsbHub(on) => format!("PU:{}", i32::from(*on)),
            Self::SetAutoDew(on) => format!("PD:{}", i32::from(*on)),
        }
    }
}

/// Parsed status response from the PA command
///
/// Response format: `PPBA:voltage:current:temp:humidity:dewpoint:quad:adj:dewA:dewB:autodew:warn:pwradj`
#[derive(Debug, Clone, Default)]
pub struct PpbaStatus {
    /// Input voltage in Volts
    pub voltage: f64,
    /// Total current in Amps
    pub current: f64,
    /// Temperature in Celsius
    pub temperature: f64,
    /// Humidity percentage
    pub humidity: f64,
    /// Dewpoint in Celsius
    pub dewpoint: f64,
    /// Quad 12V output state (0=off, 1=on)
    pub quad_12v: bool,
    /// Adjustable output state (0=off, 1=on)
    pub adjustable_output: bool,
    /// Dew Heater A PWM value (0-255)
    pub dew_a: u8,
    /// Dew Heater B PWM value (0-255)
    pub dew_b: u8,
    /// Auto-Dew enabled
    pub auto_dew: bool,
    /// Power warning flag
    pub power_warning: bool,
    /// Adjustable output power level
    pub power_adj: u8,
}

/// Parsed power statistics response from the PS command
///
/// Response format: `PS:averageAmps:ampHours:wattHours:uptime_ms` (the wire
/// field is integer milliseconds; we hold it as a `Duration` internally and
/// flatten back to ms only at the boundary).
#[derive(Debug, Clone, Default)]
pub struct PpbaPowerStats {
    /// Average current in Amps
    pub average_amps: f64,
    /// Cumulative amp-hours
    pub amp_hours: f64,
    /// Cumulative watt-hours
    pub watt_hours: f64,
    /// Device uptime
    pub uptime: Duration,
}

impl PpbaPowerStats {
    /// Get uptime in hours
    #[must_use]
    pub fn uptime_hours(&self) -> f64 {
        self.uptime.as_secs_f64() / 3600.0
    }
}

/// Parse the PA status response
///
/// Expected format: `PPBA:voltage:current:temp:humidity:dewpoint:quad:adj:dewA:dewB:autodew:warn:pwradj`
pub fn parse_status_response(response: &str) -> Result<PpbaStatus> {
    let response = response.trim();

    // Check prefix
    if !response.starts_with("PPBA:") {
        return Err(PpbaError::InvalidResponse(format!(
            "Expected PPBA: prefix, got: {response}"
        )));
    }

    // Split by colon and skip the "PPBA" prefix
    let parts: Vec<&str> = response.split(':').collect();

    // Expect 13 parts: PPBA + 12 values; the trailing `..` tolerates
    // extra parts, matching the previous `len() < 13` check.
    let [_prefix, voltage, current, temperature, humidity, dewpoint, quad_12v, adjustable_output, dew_a, dew_b, auto_dew, power_warning, power_adj, ..] =
        parts.as_slice()
    else {
        return Err(PpbaError::InvalidResponse(format!(
            "Expected 13 parts in PA response, got {}: {}",
            parts.len(),
            response
        )));
    };

    let voltage = parse_f64(voltage, "voltage")?;
    let current = parse_f64(current, "current")?;
    let temperature = parse_f64(temperature, "temperature")?;
    let humidity = parse_f64(humidity, "humidity")?;
    let dewpoint = parse_f64(dewpoint, "dewpoint")?;
    let quad_12v = parse_bool(quad_12v, "quad_12v")?;
    let adjustable_output = parse_bool(adjustable_output, "adjustable_output")?;
    let dew_a = parse_u8(dew_a, "dew_a")?;
    let dew_b = parse_u8(dew_b, "dew_b")?;
    let auto_dew = parse_bool(auto_dew, "auto_dew")?;
    let power_warning = parse_bool(power_warning, "power_warning")?;
    let power_adj = parse_u8(power_adj, "power_adj")?;

    Ok(PpbaStatus {
        voltage,
        current,
        temperature,
        humidity,
        dewpoint,
        quad_12v,
        adjustable_output,
        dew_a,
        dew_b,
        auto_dew,
        power_warning,
        power_adj,
    })
}

/// Parse the PS power statistics response
///
/// Expected format: `PS:averageAmps:ampHours:wattHours:uptime_ms`
pub fn parse_power_stats_response(response: &str) -> Result<PpbaPowerStats> {
    let response = response.trim();

    // Check prefix
    if !response.starts_with("PS:") {
        return Err(PpbaError::InvalidResponse(format!(
            "Expected PS: prefix, got: {response}"
        )));
    }

    // Split by colon and skip the "PS" prefix
    let parts: Vec<&str> = response.split(':').collect();

    // Expect 5 parts: PS + 4 values; the trailing `..` tolerates extra
    // parts, matching the previous `len() < 5` check.
    let [_prefix, average_amps, amp_hours, watt_hours, uptime_ms, ..] = parts.as_slice() else {
        return Err(PpbaError::InvalidResponse(format!(
            "Expected 5 parts in PS response, got {}: {}",
            parts.len(),
            response
        )));
    };

    let average_amps = parse_f64(average_amps, "average_amps")?;
    let amp_hours = parse_f64(amp_hours, "amp_hours")?;
    let watt_hours = parse_f64(watt_hours, "watt_hours")?;
    let uptime = Duration::from_millis(parse_u64(uptime_ms, "uptime_ms")?);

    Ok(PpbaPowerStats {
        average_amps,
        amp_hours,
        watt_hours,
        uptime,
    })
}

/// Validate a ping response
pub fn validate_ping_response(response: &str) -> Result<()> {
    let response = response.trim();
    if response == "PPBA_OK" {
        Ok(())
    } else {
        Err(PpbaError::InvalidResponse(format!(
            "Expected PPBA_OK, got: {response}"
        )))
    }
}

/// Parse a set command response (echo of the command)
///
/// For example, `P1:1` for SetQuad12V(true)
pub fn validate_set_response(command: &PpbaCommand, response: &str) -> Result<()> {
    let expected = command.to_command_string();
    let response = response.trim();

    if response == expected {
        Ok(())
    } else {
        Err(PpbaError::InvalidResponse(format!(
            "Expected {expected}, got: {response}"
        )))
    }
}

// Helper parsing functions
fn parse_f64(s: &str, field: &str) -> Result<f64> {
    s.parse::<f64>()
        .map_err(|_| PpbaError::ParseError(format!("Invalid {field} value: {s}")))
}

fn parse_u8(s: &str, field: &str) -> Result<u8> {
    s.parse::<u8>()
        .map_err(|_| PpbaError::ParseError(format!("Invalid {field} value: {s}")))
}

fn parse_u64(s: &str, field: &str) -> Result<u64> {
    s.parse::<u64>()
        .map_err(|_| PpbaError::ParseError(format!("Invalid {field} value: {s}")))
}

fn parse_bool(s: &str, field: &str) -> Result<bool> {
    match s {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(PpbaError::ParseError(format!(
            "Invalid {field} boolean value: {s}"
        ))),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    mod status_parsing {
        use super::*;

        #[test]
        fn parses_valid_status_response() {
            let response = "PPBA:12.5:3.2:25.0:60:15.5:1:0:128:64:1:0:0";
            let status = parse_status_response(response).unwrap();

            assert_eq!(status.voltage, 12.5);
            assert_eq!(status.current, 3.2);
            assert_eq!(status.temperature, 25.0);
            assert_eq!(status.humidity, 60.0);
            assert_eq!(status.dewpoint, 15.5);
            assert!(status.quad_12v);
            assert!(!status.adjustable_output);
            assert_eq!(status.dew_a, 128);
            assert_eq!(status.dew_b, 64);
            assert!(status.auto_dew);
            assert!(!status.power_warning);
            assert_eq!(status.power_adj, 0);
        }

        #[test]
        fn parses_status_with_trailing_newline() {
            let response = "PPBA:12.0:2.0:20.0:50:10.0:0:1:0:255:0:1:12\n";
            let status = parse_status_response(response).unwrap();

            assert_eq!(status.voltage, 12.0);
            assert!(!status.quad_12v);
            assert!(status.adjustable_output);
            assert_eq!(status.dew_b, 255);
            assert!(status.power_warning);
            assert_eq!(status.power_adj, 12);
        }

        #[test]
        fn parses_status_with_negative_temperature() {
            let response = "PPBA:11.8:1.5:-5.0:80:-10.2:1:1:100:100:1:0:5";
            let status = parse_status_response(response).unwrap();

            assert_eq!(status.temperature, -5.0);
            assert_eq!(status.dewpoint, -10.2);
        }

        #[test]
        fn rejects_invalid_prefix() {
            let response = "INVALID:12.5:3.2:25.0:60:15.5:1:0:128:64:1:0:0";
            let result = parse_status_response(response);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_too_few_fields() {
            let response = "PPBA:12.5:3.2:25.0";
            let result = parse_status_response(response);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_invalid_float_field() {
            let response = "PPBA:invalid:3.2:25.0:60:15.5:1:0:128:64:1:0:0";
            let result = parse_status_response(response);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_invalid_boolean_field() {
            let response = "PPBA:12.5:3.2:25.0:60:15.5:2:0:128:64:1:0:0";
            let result = parse_status_response(response);
            assert!(result.is_err());
        }
    }

    mod power_stats_parsing {
        use super::*;

        #[test]
        fn parses_valid_power_stats() {
            let response = "PS:2.5:10.5:126.0:3600000";
            let stats = parse_power_stats_response(response).unwrap();

            assert_eq!(stats.average_amps, 2.5);
            assert_eq!(stats.amp_hours, 10.5);
            assert_eq!(stats.watt_hours, 126.0);
            assert_eq!(stats.uptime, Duration::from_hours(1));
        }

        #[test]
        fn calculates_uptime_hours_correctly() {
            let response = "PS:1.0:5.0:60.0:7200000"; // 2 hours
            let stats = parse_power_stats_response(response).unwrap();

            assert_eq!(stats.uptime_hours(), 2.0);
        }

        #[test]
        fn parses_power_stats_with_newline() {
            let response = "PS:0.5:1.0:12.0:1800000\n";
            let stats = parse_power_stats_response(response).unwrap();

            assert_eq!(stats.average_amps, 0.5);
            assert_eq!(stats.uptime_hours(), 0.5);
        }

        #[test]
        fn rejects_invalid_prefix() {
            let response = "INVALID:2.5:10.5:126.0:3600000";
            let result = parse_power_stats_response(response);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_too_few_fields() {
            let response = "PS:2.5:10.5";
            let result = parse_power_stats_response(response);
            assert!(result.is_err());
        }
    }

    mod ping_validation {
        use super::*;

        #[test]
        fn accepts_valid_ping_response() {
            assert!(validate_ping_response("PPBA_OK").is_ok());
        }

        #[test]
        fn accepts_ping_with_newline() {
            assert!(validate_ping_response("PPBA_OK\n").is_ok());
        }

        #[test]
        fn rejects_invalid_ping_response() {
            assert!(validate_ping_response("INVALID").is_err());
            assert!(validate_ping_response("").is_err());
            assert!(validate_ping_response("PPBA_ERROR").is_err());
        }
    }

    mod command_serialization {
        use super::*;

        #[test]
        fn serializes_ping_command() {
            assert_eq!(PpbaCommand::Ping.to_command_string(), "P#");
        }

        #[test]
        fn serializes_firmware_version_command() {
            assert_eq!(PpbaCommand::FirmwareVersion.to_command_string(), "PV");
        }

        #[test]
        fn serializes_status_command() {
            assert_eq!(PpbaCommand::Status.to_command_string(), "PA");
        }

        #[test]
        fn serializes_power_stats_command() {
            assert_eq!(PpbaCommand::PowerStats.to_command_string(), "PS");
        }

        #[test]
        fn serializes_set_quad_12v_on() {
            assert_eq!(PpbaCommand::SetQuad12V(true).to_command_string(), "P1:1");
        }

        #[test]
        fn serializes_set_quad_12v_off() {
            assert_eq!(PpbaCommand::SetQuad12V(false).to_command_string(), "P1:0");
        }

        #[test]
        fn serializes_set_adjustable() {
            assert_eq!(PpbaCommand::SetAdjustable(true).to_command_string(), "P2:1");
            assert_eq!(
                PpbaCommand::SetAdjustable(false).to_command_string(),
                "P2:0"
            );
        }

        #[test]
        fn serializes_set_dew_a_pwm() {
            assert_eq!(PpbaCommand::SetDewA(PwmDuty(0)).to_command_string(), "P3:0");
            assert_eq!(
                PpbaCommand::SetDewA(PwmDuty(128)).to_command_string(),
                "P3:128"
            );
            assert_eq!(
                PpbaCommand::SetDewA(PwmDuty(255)).to_command_string(),
                "P3:255"
            );
        }

        #[test]
        fn serializes_set_dew_b_pwm() {
            assert_eq!(PpbaCommand::SetDewB(PwmDuty(0)).to_command_string(), "P4:0");
            assert_eq!(
                PpbaCommand::SetDewB(PwmDuty(128)).to_command_string(),
                "P4:128"
            );
            assert_eq!(
                PpbaCommand::SetDewB(PwmDuty(255)).to_command_string(),
                "P4:255"
            );
        }

        #[test]
        fn serializes_set_usb_hub() {
            assert_eq!(PpbaCommand::SetUsbHub(true).to_command_string(), "PU:1");
            assert_eq!(PpbaCommand::SetUsbHub(false).to_command_string(), "PU:0");
        }

        #[test]
        fn serializes_set_auto_dew() {
            assert_eq!(PpbaCommand::SetAutoDew(true).to_command_string(), "PD:1");
            assert_eq!(PpbaCommand::SetAutoDew(false).to_command_string(), "PD:0");
        }
    }

    mod pwm_duty_conversion {
        use super::*;

        #[test]
        fn rounds_to_nearest_byte() {
            assert_eq!(PwmDuty::from(127.4), PwmDuty(127));
            assert_eq!(PwmDuty::from(127.5), PwmDuty(128));
        }

        #[test]
        fn clamps_to_device_range() {
            assert_eq!(PwmDuty::from(-1.0), PwmDuty(0));
            assert_eq!(PwmDuty::from(300.0), PwmDuty(255));
            assert_eq!(PwmDuty::from(f64::INFINITY), PwmDuty(255));
        }

        #[test]
        fn nan_clamps_to_zero() {
            // Documents the saturating-cast behavior; the switch layer
            // rejects NaN before it ever reaches this conversion.
            assert_eq!(PwmDuty::from(f64::NAN), PwmDuty(0));
        }
    }

    mod set_response_validation {
        use super::*;

        #[test]
        fn validates_matching_response() {
            let cmd = PpbaCommand::SetQuad12V(true);
            assert!(validate_set_response(&cmd, "P1:1").is_ok());
        }

        #[test]
        fn validates_response_with_newline() {
            let cmd = PpbaCommand::SetDewA(PwmDuty(128));
            assert!(validate_set_response(&cmd, "P3:128\n").is_ok());
        }

        #[test]
        fn rejects_mismatched_response() {
            let cmd = PpbaCommand::SetQuad12V(true);
            assert!(validate_set_response(&cmd, "P1:0").is_err());
        }
    }
}

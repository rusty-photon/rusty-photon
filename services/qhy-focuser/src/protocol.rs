//! QHY Q-Focuser JSON command/response protocol
//!
//! The Q-Focuser communicates via JSON objects over serial. Each command has a
//! `cmd_id` field, and responses echo the command ID in an `idx` field.

use serde_json::Value;
use tracing::debug;

use crate::error::{QhyFocuserError, Result};

/// Commands that can be sent to the QHY Q-Focuser
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Get firmware and board version (`cmd_id`: 1)
    GetVersion,
    /// Relative move (`cmd_id`: 2)
    RelativeMove {
        /// Direction: 1 = inward, -1 = outward
        direction: i8,
        steps: u32,
    },
    /// Abort current movement (`cmd_id`: 3)
    Abort,
    /// Read temperature and voltage (`cmd_id`: 4)
    ReadTemperature,
    /// Get current position (`cmd_id`: 5)
    GetPosition,
    /// Move to absolute position (`cmd_id`: 6)
    AbsoluteMove { position: i64 },
    /// Set reverse direction (`cmd_id`: 7)
    SetReverse { enabled: bool },
    /// Sync position counter (`cmd_id`: 11)
    SyncPosition { position: i64 },
    /// Set movement speed (`cmd_id`: 13)
    SetSpeed { speed: u8 },
    /// Set hold current (`cmd_id`: 16)
    SetHoldCurrent { ihold: u8, irun: u8 },
    /// Set power-down mode (`cmd_id`: 19)
    SetPdnMode { pdn: u8 },
}

impl Command {
    /// Get the command ID for this command
    #[must_use]
    pub const fn cmd_id(&self) -> u8 {
        match self {
            Self::GetVersion => 1,
            Self::RelativeMove { .. } => 2,
            Self::Abort => 3,
            Self::ReadTemperature => 4,
            Self::GetPosition => 5,
            Self::AbsoluteMove { .. } => 6,
            Self::SetReverse { .. } => 7,
            Self::SyncPosition { .. } => 11,
            Self::SetSpeed { .. } => 13,
            Self::SetHoldCurrent { .. } => 16,
            Self::SetPdnMode { .. } => 19,
        }
    }

    /// Serialize the command to a JSON string for serial transmission
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let json = match self {
            Self::GetVersion => {
                serde_json::json!({"cmd_id": 1})
            }
            Self::RelativeMove { direction, steps } => {
                serde_json::json!({
                    "cmd_id": 2,
                    "dir": direction,
                    "step": steps
                })
            }
            Self::Abort => {
                serde_json::json!({"cmd_id": 3})
            }
            Self::ReadTemperature => {
                serde_json::json!({"cmd_id": 4})
            }
            Self::GetPosition => {
                serde_json::json!({"cmd_id": 5})
            }
            Self::AbsoluteMove { position } => {
                serde_json::json!({
                    "cmd_id": 6,
                    "tar": position
                })
            }
            Self::SetReverse { enabled } => {
                serde_json::json!({
                    "cmd_id": 7,
                    "rev": i32::from(*enabled)
                })
            }
            Self::SyncPosition { position } => {
                serde_json::json!({
                    "cmd_id": 11,
                    "init_val": position
                })
            }
            Self::SetSpeed { speed } => {
                serde_json::json!({
                    "cmd_id": 13,
                    "speed": speed
                })
            }
            Self::SetHoldCurrent { ihold, irun } => {
                serde_json::json!({
                    "cmd_id": 16,
                    "ihold": ihold,
                    "irun": irun
                })
            }
            Self::SetPdnMode { pdn } => {
                serde_json::json!({
                    "cmd_id": 19,
                    "pdn_d": pdn
                })
            }
        };
        json.to_string()
    }
}

/// Parsed version response from `cmd_id` 1
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionResponse {
    pub firmware_version: String,
    pub board_version: String,
}

/// Parsed temperature/voltage response from `cmd_id` 4
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureResponse {
    /// Outer temperature in degrees Celsius
    pub outer_temp: f64,
    /// Chip temperature in degrees Celsius
    pub chip_temp: f64,
    /// Input voltage in volts
    pub voltage: f64,
}

/// Parsed position response from `cmd_id` 5
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionResponse {
    pub position: i64,
}

/// Parse a JSON response string and validate the `idx` (command ID) field
pub fn parse_response(response: &str, expected_cmd_id: u8) -> Result<Value> {
    debug!("Parsing response: {}", response);

    let value: Value = serde_json::from_str(response)
        .map_err(|e| QhyFocuserError::InvalidResponse(format!("Invalid JSON: {e}")))?;

    let idx = value
        .get("idx")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| QhyFocuserError::InvalidResponse("Missing 'idx' field".to_string()))?;

    if idx != u64::from(expected_cmd_id) {
        return Err(QhyFocuserError::InvalidResponse(format!(
            "Expected idx {expected_cmd_id}, got {idx}"
        )));
    }

    Ok(value)
}

/// Read the `idx` field off an already-parsed JSON response value.
pub fn extract_idx(value: &Value) -> Result<u8> {
    let idx = value
        .get("idx")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| QhyFocuserError::InvalidResponse("Missing 'idx' field".to_string()))?;
    u8::try_from(idx)
        .map_err(|_| QhyFocuserError::InvalidResponse(format!("idx {idx} out of range")))
}

/// Build a [`VersionResponse`] from an already-parsed JSON value.
#[must_use]
pub fn parse_version_value(value: &Value) -> VersionResponse {
    let firmware_version = value
        .get("firmware_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let board_version = value
        .get("board_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    VersionResponse {
        firmware_version,
        board_version,
    }
}

/// Build a [`TemperatureResponse`] from an already-parsed JSON value.
///
/// Raw values from the device: temp values divided by 1000, voltage by 10.
pub fn parse_temperature_value(value: &Value) -> Result<TemperatureResponse> {
    // `as_f64` returns `Some` for every JSON number, integers included
    // (serde_json without `arbitrary_precision`), so no integer fallback
    // arm is needed.
    let outer_temp_raw = value
        .get("o_t")
        .and_then(Value::as_f64)
        .ok_or_else(|| QhyFocuserError::ParseError("Missing 'o_t' field".to_string()))?;
    let chip_temp_raw = value
        .get("c_t")
        .and_then(Value::as_f64)
        .ok_or_else(|| QhyFocuserError::ParseError("Missing 'c_t' field".to_string()))?;
    let voltage_raw = value
        .get("c_r")
        .and_then(Value::as_f64)
        .ok_or_else(|| QhyFocuserError::ParseError("Missing 'c_r' field".to_string()))?;
    Ok(TemperatureResponse {
        outer_temp: outer_temp_raw / 1000.0,
        chip_temp: chip_temp_raw / 1000.0,
        voltage: voltage_raw / 10.0,
    })
}

/// Build a [`PositionResponse`] from an already-parsed JSON value.
pub fn parse_position_value(value: &Value) -> Result<PositionResponse> {
    let position = value
        .get("pos")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| QhyFocuserError::ParseError("Missing 'pos' field".to_string()))?;
    Ok(PositionResponse { position })
}

/// Parse a version response (`cmd_id` 1)
pub fn parse_version_response(response: &str) -> Result<VersionResponse> {
    let value = parse_response(response, 1)?;
    Ok(parse_version_value(&value))
}

/// Parse a temperature response (`cmd_id` 4)
///
/// Raw values from the device: temp values divided by 1000, voltage by 10
pub fn parse_temperature_response(response: &str) -> Result<TemperatureResponse> {
    let value = parse_response(response, 4)?;
    parse_temperature_value(&value)
}

/// Parse a position response (`cmd_id` 5)
pub fn parse_position_response(response: &str) -> Result<PositionResponse> {
    let value = parse_response(response, 5)?;
    parse_position_value(&value)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // ============================================================================
    // Command serialization tests
    // ============================================================================

    #[test]
    fn test_get_version_command() {
        let cmd = Command::GetVersion;
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 1);
        assert_eq!(cmd.cmd_id(), 1);
    }

    #[test]
    fn test_relative_move_command() {
        let cmd = Command::RelativeMove {
            direction: -1,
            steps: 1000,
        };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 2);
        assert_eq!(parsed["dir"], -1);
        assert_eq!(parsed["step"], 1000);
        assert_eq!(cmd.cmd_id(), 2);
    }

    #[test]
    fn test_abort_command() {
        let cmd = Command::Abort;
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 3);
        assert_eq!(cmd.cmd_id(), 3);
    }

    #[test]
    fn test_read_temperature_command() {
        let cmd = Command::ReadTemperature;
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 4);
        assert_eq!(cmd.cmd_id(), 4);
    }

    #[test]
    fn test_get_position_command() {
        let cmd = Command::GetPosition;
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 5);
        assert_eq!(cmd.cmd_id(), 5);
    }

    #[test]
    fn test_absolute_move_command() {
        let cmd = Command::AbsoluteMove { position: 32000 };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 6);
        assert_eq!(parsed["tar"], 32000);
        assert_eq!(cmd.cmd_id(), 6);
    }

    #[test]
    fn test_absolute_move_negative_position() {
        let cmd = Command::AbsoluteMove { position: -5000 };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["tar"], -5000);
    }

    #[test]
    fn test_set_reverse_enabled() {
        let cmd = Command::SetReverse { enabled: true };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 7);
        assert_eq!(parsed["rev"], 1);
    }

    #[test]
    fn test_set_reverse_disabled() {
        let cmd = Command::SetReverse { enabled: false };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["rev"], 0);
    }

    #[test]
    fn test_sync_position_command() {
        let cmd = Command::SyncPosition { position: 10000 };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 11);
        assert_eq!(parsed["init_val"], 10000);
    }

    #[test]
    fn test_set_speed_command() {
        let cmd = Command::SetSpeed { speed: 5 };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 13);
        assert_eq!(parsed["speed"], 5);
    }

    #[test]
    fn test_set_hold_current_command() {
        let cmd = Command::SetHoldCurrent {
            ihold: 10,
            irun: 20,
        };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 16);
        assert_eq!(parsed["ihold"], 10);
        assert_eq!(parsed["irun"], 20);
    }

    #[test]
    fn test_set_pdn_mode_command() {
        let cmd = Command::SetPdnMode { pdn: 1 };
        let json = cmd.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cmd_id"], 19);
        assert_eq!(parsed["pdn_d"], 1);
    }

    // ============================================================================
    // Response parsing tests
    // ============================================================================

    #[test]
    fn test_parse_response_valid_idx() {
        let response = r#"{"idx": 5, "pos": 1000}"#;
        let value = parse_response(response, 5).unwrap();
        assert_eq!(value["idx"], 5);
    }

    #[test]
    fn test_parse_response_wrong_idx() {
        let response = r#"{"idx": 3, "pos": 1000}"#;
        let err = parse_response(response, 5).unwrap_err();
        assert!(err.to_string().contains("Expected idx 5, got 3"));
    }

    #[test]
    fn test_parse_response_missing_idx() {
        let response = r#"{"pos": 1000}"#;
        let err = parse_response(response, 5).unwrap_err();
        assert!(err.to_string().contains("Missing 'idx' field"));
    }

    #[test]
    fn test_parse_response_invalid_json() {
        let err = parse_response("not json", 5).unwrap_err();
        assert!(err.to_string().contains("Invalid JSON"));
    }

    #[test]
    fn test_parse_version_response() {
        let response = r#"{"idx": 1, "firmware_version": "2.1.0", "board_version": "1.0"}"#;
        let version = parse_version_response(response).unwrap();
        assert_eq!(version.firmware_version, "2.1.0");
        assert_eq!(version.board_version, "1.0");
    }

    #[test]
    fn test_parse_version_response_missing_fields() {
        let response = r#"{"idx": 1}"#;
        let version = parse_version_response(response).unwrap();
        assert_eq!(version.firmware_version, "unknown");
        assert_eq!(version.board_version, "unknown");
    }

    #[test]
    fn test_parse_temperature_response() {
        // Raw: o_t=25000 -> 25.0°C, c_t=30000 -> 30.0°C, c_r=125 -> 12.5V
        let response = r#"{"idx": 4, "o_t": 25000, "c_t": 30000, "c_r": 125}"#;
        let temp = parse_temperature_response(response).unwrap();
        assert!((temp.outer_temp - 25.0).abs() < 0.001);
        assert!((temp.chip_temp - 30.0).abs() < 0.001);
        assert!((temp.voltage - 12.5).abs() < 0.001);
    }

    #[test]
    fn test_parse_temperature_response_negative_temp() {
        // Raw: o_t=-5000 -> -5.0°C
        let response = r#"{"idx": 4, "o_t": -5000, "c_t": 10000, "c_r": 120}"#;
        let temp = parse_temperature_response(response).unwrap();
        assert!((temp.outer_temp - (-5.0)).abs() < 0.001);
    }

    #[test]
    fn test_parse_temperature_response_missing_field() {
        let response = r#"{"idx": 4, "o_t": 25000, "c_t": 30000}"#;
        let err = parse_temperature_response(response).unwrap_err();
        assert!(err.to_string().contains("c_r"));
    }

    #[test]
    fn test_parse_position_response() {
        let response = r#"{"idx": 5, "pos": 32000}"#;
        let pos = parse_position_response(response).unwrap();
        assert_eq!(pos.position, 32000);
    }

    #[test]
    fn test_parse_position_response_negative() {
        let response = r#"{"idx": 5, "pos": -10000}"#;
        let pos = parse_position_response(response).unwrap();
        assert_eq!(pos.position, -10000);
    }

    #[test]
    fn test_parse_position_response_zero() {
        let response = r#"{"idx": 5, "pos": 0}"#;
        let pos = parse_position_response(response).unwrap();
        assert_eq!(pos.position, 0);
    }

    #[test]
    fn test_parse_position_response_missing_field() {
        let response = r#"{"idx": 5}"#;
        let err = parse_position_response(response).unwrap_err();
        assert!(err.to_string().contains("pos"));
    }

    // ============================================================================
    // Command properties tests
    // ============================================================================

    #[test]
    fn test_command_clone() {
        let cmd = Command::AbsoluteMove { position: 100 };
        let cloned = cmd.clone();
        assert_eq!(cmd, cloned);
    }

    #[test]
    fn test_command_debug() {
        let cmd = Command::GetVersion;
        let debug = format!("{cmd:?}");
        assert!(debug.contains("GetVersion"));
    }

    #[test]
    fn test_version_response_clone() {
        let resp = VersionResponse {
            firmware_version: "1.0".to_string(),
            board_version: "2.0".to_string(),
        };
        let cloned = resp.clone();
        assert_eq!(resp, cloned);
    }

    #[test]
    fn test_temperature_response_debug() {
        let resp = TemperatureResponse {
            outer_temp: 25.0,
            chip_temp: 30.0,
            voltage: 12.5,
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("25.0"));
    }

    #[test]
    fn test_position_response_debug() {
        let resp = PositionResponse { position: 1000 };
        let debug = format!("{resp:?}");
        assert!(debug.contains("1000"));
    }
}

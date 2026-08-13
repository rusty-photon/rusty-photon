//! Pegasus Astro Scops OAG ASCII command/response protocol.
//!
//! Commands are short ASCII strings terminated by LF (`\n`); the codec appends
//! the terminator. Responses are terminated by CRLF (`\r\n`), which the codec
//! trims. Every command returns exactly one response frame. See
//! `docs/services/pa-scops-oag.md#protocol-reference` for the source table.
//!
//! Two DMFC-family commands are deliberately absent: `N:` (reverse) and `C:`
//! (backlash). Both are rejected with `ERR:` by Scops firmware 1.2, and ASCOM
//! `IFocuserV4` has no reverse/backlash member — so the driver never issues
//! them. Keeping them out of the [`Command`] enum removes the temptation.

use crate::error::{Result, ScopsOagError};

/// Commands the driver issues to the Scops OAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `#` — identify / handshake. Expect `OK_SCOPS`.
    Handshake,
    /// `A` — consolidated status report. Expect
    /// `OK_SCOPS:<ver>:<motor>:<temp>:<pos>:<moving>:<led>:<rev>:<enc>:<backlash>`.
    Status,
    /// `M:<pos>` — move to absolute position. Echoes `M:<pos>`.
    MoveAbsolute { position: i64 },
    /// `W:<pos>` — set the current position without moving. Echoes `W:<pos>`.
    SyncPosition { position: i64 },
    /// `H` — halt. Returns a bare flag (`0`).
    Halt,
}

impl Command {
    /// Render the command into the exact ASCII payload the Scops OAG expects
    /// (without the LF terminator; the codec appends that). The `M:`/`W:` forms
    /// use the clean Pegasus syntax with no trailing `d` byte.
    #[must_use]
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Handshake => "#".to_string(),
            Self::Status => "A".to_string(),
            Self::MoveAbsolute { position } => format!("M:{position}"),
            Self::SyncPosition { position } => format!("W:{position}"),
            Self::Halt => "H".to_string(),
        }
    }
}

/// Parsed `A` status report.
///
/// Only the fields the ASCOM Focuser surface needs are retained; the
/// motor-type, temperature, LED, reverse, encoder, and backlash slots are
/// parsed for position but not surfaced (see the design doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopsStatus {
    /// Firmware version (field 2), e.g. `1.2`.
    pub firmware_version: String,
    /// Absolute position in ticks (field 5).
    pub position: i64,
    /// Whether the motor is moving (field 6).
    pub is_moving: bool,
}

/// The status-report signature token, shared by the `#` handshake and the first
/// colon-delimited field of the `A` report.
pub const STATUS_TOKEN: &str = "OK_SCOPS";

fn parse_bool(s: &str, field: &str) -> Result<bool> {
    match s.trim() {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(ScopsOagError::ParseError(format!(
            "{field}: expected '0' or '1', got {other:?}"
        ))),
    }
}

/// Parse the `A` status report (`OK_SCOPS:ver:motor:temp:pos:moving:...`).
///
/// Requires the `OK_SCOPS` prefix and at least the ten documented fields;
/// trailing fields a future firmware might add are tolerated. The temperature
/// slot (field 4) is intentionally ignored — the Scops OAG has no sensor.
pub fn parse_status(response: &str) -> Result<ScopsStatus> {
    let trimmed = response.trim();
    let parts: Vec<&str> = trimmed.split(':').collect();
    if parts.first() != Some(&STATUS_TOKEN) {
        return Err(ScopsOagError::InvalidResponse(format!(
            "status: expected '{STATUS_TOKEN}' prefix, got {response:?}"
        )));
    }
    let [_, firmware_raw, _, _, position_raw, moving_raw, _, _, _, _, ..] = parts.as_slice() else {
        return Err(ScopsOagError::InvalidResponse(format!(
            "status: expected at least 10 fields, got {} in {response:?}",
            parts.len()
        )));
    };
    let firmware_version = firmware_raw.trim().to_string();
    let position: i64 = position_raw
        .trim()
        .parse()
        .map_err(|e| ScopsOagError::ParseError(format!("status position: {e}")))?;
    let is_moving = parse_bool(moving_raw, "status is_moving")?;
    Ok(ScopsStatus {
        firmware_version,
        position,
        is_moving,
    })
}

/// Validate that `response` echoes the issued move/sync `command`.
///
/// The Scops echoes `M:<pos>` / `W:<pos>` verbatim. Only the echo-bearing
/// commands are accepted here; the others belong to their dedicated handling.
pub fn validate_echo(command: &Command, response: &str) -> Result<()> {
    let expected = match command {
        Command::MoveAbsolute { .. } | Command::SyncPosition { .. } => command.to_command_string(),
        Command::Handshake | Command::Status | Command::Halt => {
            return Err(ScopsOagError::InvalidResponse(format!(
                "validate_echo not applicable for {command:?}"
            )));
        }
    };
    if response.trim() == expected {
        Ok(())
    } else {
        Err(ScopsOagError::InvalidResponse(format!(
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
    fn handshake_serialises_to_hash() {
        assert_eq!(Command::Handshake.to_command_string(), "#");
    }

    #[test]
    fn status_serialises_to_a() {
        assert_eq!(Command::Status.to_command_string(), "A");
    }

    #[test]
    fn move_absolute_serialises_without_trailing_d() {
        assert_eq!(
            Command::MoveAbsolute { position: 22000 }.to_command_string(),
            "M:22000"
        );
    }

    #[test]
    fn sync_position_serialises_to_w() {
        assert_eq!(
            Command::SyncPosition { position: 1000 }.to_command_string(),
            "W:1000"
        );
    }

    #[test]
    fn halt_serialises_to_h() {
        assert_eq!(Command::Halt.to_command_string(), "H");
    }

    // ---- parse_status -----------------------------------------------------

    #[test]
    fn parse_status_happy_path() {
        // The exact frame captured on the reference unit (firmware 1.2).
        let status = parse_status("OK_SCOPS:1.2:1:0:22000:0:1:0:1:0").unwrap();
        assert_eq!(status.firmware_version, "1.2");
        assert_eq!(status.position, 22000);
        assert!(!status.is_moving);
    }

    #[test]
    fn parse_status_with_trailing_crlf() {
        let status = parse_status("OK_SCOPS:1.2:1:0:22000:0:1:0:1:0\r\n").unwrap();
        assert_eq!(status.position, 22000);
    }

    #[test]
    fn parse_status_reports_moving() {
        let status = parse_status("OK_SCOPS:1.2:1:0:5000:1:1:0:1:0").unwrap();
        assert!(status.is_moving);
        assert_eq!(status.position, 5000);
    }

    #[test]
    fn parse_status_tolerates_extra_trailing_fields() {
        let status = parse_status("OK_SCOPS:1.7:1:22:5000:0:1:0:1:0:99").unwrap();
        assert_eq!(status.firmware_version, "1.7");
        assert_eq!(status.position, 5000);
    }

    #[test]
    fn parse_status_rejects_wrong_prefix() {
        let err = parse_status("OK_DMFCN:1.2:1:0:22000:0:1:0:1:0").unwrap_err();
        assert!(matches!(err, ScopsOagError::InvalidResponse(_)));
    }

    #[test]
    fn parse_status_rejects_too_few_fields() {
        let err = parse_status("OK_SCOPS:1.2:1:0:22000").unwrap_err();
        assert!(matches!(err, ScopsOagError::InvalidResponse(_)));
    }

    #[test]
    fn parse_status_rejects_bad_position() {
        let err = parse_status("OK_SCOPS:1.2:1:0:abc:0:1:0:1:0").unwrap_err();
        assert!(matches!(err, ScopsOagError::ParseError(_)));
    }

    #[test]
    fn parse_status_rejects_bad_moving_flag() {
        let err = parse_status("OK_SCOPS:1.2:1:0:22000:2:1:0:1:0").unwrap_err();
        assert!(matches!(err, ScopsOagError::ParseError(_)));
    }

    // ---- validate_echo ----------------------------------------------------

    #[test]
    fn validate_echo_accepts_matching_move() {
        validate_echo(&Command::MoveAbsolute { position: 5000 }, "M:5000").unwrap();
    }

    #[test]
    fn validate_echo_accepts_matching_move_with_crlf() {
        validate_echo(&Command::MoveAbsolute { position: 5000 }, "M:5000\r\n").unwrap();
    }

    #[test]
    fn validate_echo_accepts_matching_sync() {
        validate_echo(&Command::SyncPosition { position: 22000 }, "W:22000").unwrap();
    }

    #[test]
    fn validate_echo_rejects_mismatched_move() {
        let err = validate_echo(&Command::MoveAbsolute { position: 5000 }, "M:6000").unwrap_err();
        assert!(matches!(err, ScopsOagError::InvalidResponse(_)));
    }

    #[test]
    fn validate_echo_rejects_non_echo_command() {
        let err = validate_echo(&Command::Halt, "0").unwrap_err();
        assert!(matches!(err, ScopsOagError::InvalidResponse(_)));
    }
}

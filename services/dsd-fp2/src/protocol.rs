//! Deep Sky Dad FP2 wire protocol.
//!
//! Commands are framed as `[CMD]`. Responses are framed as `(VALUE)` with
//! `)` as the terminator [`crate::transport::Fp2SerialTransportFactory`]
//! configures the `SerialFrameTransport` to read until. Body parsing of the
//! `(VALUE)` payload is command-specific, so it lives on [`RawResponse`]
//! rather than inside the codec.
//!
//! Reference: `indilib/indi/drivers/auxiliary/deepskydad_fp.cpp` (the INDI
//! driver covering FP1 + FP2).

use crate::error::{DsdFp2Error, Result};

/// The hardware brightness ceiling exposed by the FP2 firmware.
pub const MAX_BRIGHTNESS: u16 = 4096;

/// Target angle the FP2 considers "open".
pub const OPEN_ANGLE: u16 = 0;

/// Target angle the FP2 considers "closed".
pub const CLOSED_ANGLE: u16 = 270;

/// Commands recognised by the FP2 firmware that this driver issues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `[GFRM]` — firmware identification.
    GetFirmware,
    /// `[GOPS]` — FP2-style cover state (`(0)` closed, `(1)` open).
    GetCoverState,
    /// `[GMOV]` — motor running state.
    GetMotorState,
    /// `[STRG<deg>]` — set the target angle the next `[SMOV]` will drive to.
    SetTarget(u16),
    /// `[SMOV]` — start moving the cover to the current target.
    StartMove,
    /// `[GLON]` — light enable state.
    GetLight,
    /// `[SLON0]` / `[SLON1]` — toggle the EL panel.
    SetLight(bool),
    /// `[GLBR]` — brightness (0..=4096).
    GetBrightness,
    /// `[SLBR<NNNN>]` — brightness (zero-padded four digits).
    SetBrightness(u16),
    /// `[GHTT]` — heater temperature (°C, float).
    GetHeaterTemp,
}

impl Command {
    /// Encode the command to the bytes that go on the wire.
    ///
    /// The FP2 firmware uses `]` (not `\n`) as the end-of-command marker, so
    /// no terminator is appended here — the closing `]` is part of the
    /// payload itself. The `SerialFrameTransport`'s configured terminator
    /// (`)`) only applies to reads.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::GetFirmware => "[GFRM]".to_string(),
            Self::GetCoverState => "[GOPS]".to_string(),
            Self::GetMotorState => "[GMOV]".to_string(),
            Self::SetTarget(deg) => format!("[STRG{deg}]"),
            Self::StartMove => "[SMOV]".to_string(),
            Self::GetLight => "[GLON]".to_string(),
            Self::SetLight(on) => if *on { "[SLON1]" } else { "[SLON0]" }.to_string(),
            Self::GetBrightness => "[GLBR]".to_string(),
            Self::SetBrightness(b) => format!("[SLBR{b:04}]"),
            Self::GetHeaterTemp => "[GHTT]".to_string(),
        }
    }
}

/// A response from the FP2 with the surrounding `(`/`)` already stripped.
///
/// The codec produces these; per-command parsers below interpret the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    /// Trimmed inner body (no parentheses, no leading/trailing whitespace).
    pub body: String,
}

impl RawResponse {
    /// Build a `RawResponse` by stripping surrounding parentheses from a
    /// raw frame produced by `recv_frame`.
    pub fn from_frame(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let start = trimmed
            .find('(')
            .ok_or_else(|| DsdFp2Error::MalformedResponse(format!("missing '(': {raw:?}")))?;
        let end = trimmed
            .rfind(')')
            .ok_or_else(|| DsdFp2Error::MalformedResponse(format!("missing ')': {raw:?}")))?;
        // `get` returns `None` exactly when `end <= start`, so it is also
        // the inverted-parens check.
        let body = trimmed
            .get(start.saturating_add(1)..end)
            .ok_or_else(|| DsdFp2Error::MalformedResponse(format!("inverted parens: {raw:?}")))?;
        Ok(Self {
            body: body.trim().to_string(),
        })
    }

    /// Accept canonical `(OK)`. Anything else is an error.
    pub fn parse_ok(&self) -> Result<()> {
        if self.body == "OK" {
            Ok(())
        } else {
            Err(DsdFp2Error::MalformedResponse(format!(
                "expected OK body, got {:?}",
                self.body
            )))
        }
    }

    /// Parse a signed integer body.
    pub fn parse_int(&self) -> Result<i32> {
        self.body.parse::<i32>().map_err(|e| {
            DsdFp2Error::MalformedResponse(format!("not an integer ({e}): {:?}", self.body))
        })
    }

    /// Parse a non-negative integer that fits in `u16`.
    pub fn parse_u16(&self) -> Result<u16> {
        let n = self.parse_int()?;
        u16::try_from(n).map_err(|_| {
            DsdFp2Error::MalformedResponse(format!("value out of u16 range: {:?}", self.body))
        })
    }

    /// Parse `(0)` → `false`, `(1)` → `true`, anything else error.
    pub fn parse_bool(&self) -> Result<bool> {
        match self.parse_int()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DsdFp2Error::MalformedResponse(format!(
                "expected 0 or 1, got {other}"
            ))),
        }
    }

    /// Parse a temperature (`°C`, float). Tolerates `( 26.104242)` leading
    /// whitespace because the FP2 firmware emits that on `[GHTT]`.
    pub fn parse_temperature(&self) -> Result<f64> {
        self.body.parse::<f64>().map_err(|e| {
            DsdFp2Error::MalformedResponse(format!("not a float ({e}): {:?}", self.body))
        })
    }

    /// Parse a `Board=…, Version=…` body from `[GFRM]`.
    pub fn parse_firmware(&self) -> Result<FirmwareInfo> {
        let mut board: Option<String> = None;
        let mut version: Option<String> = None;
        for part in self.body.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("Board=") {
                board = Some(rest.trim().to_string());
            } else if let Some(rest) = part.strip_prefix("Version=") {
                version = Some(rest.trim().to_string());
            }
        }
        Ok(FirmwareInfo {
            board: board.ok_or_else(|| {
                DsdFp2Error::MalformedResponse(format!("missing Board= in {:?}", self.body))
            })?,
            version: version.ok_or_else(|| {
                DsdFp2Error::MalformedResponse(format!("missing Version= in {:?}", self.body))
            })?,
        })
    }
}

/// Firmware identification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareInfo {
    pub board: String,
    pub version: String,
}

impl FirmwareInfo {
    /// True if this firmware advertises the FP2 board.
    #[must_use]
    pub fn is_fp2(&self) -> bool {
        self.board.contains("DeepSkyDad.FP2")
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn encode_get_commands() {
        assert_eq!(Command::GetFirmware.encode(), "[GFRM]");
        assert_eq!(Command::GetCoverState.encode(), "[GOPS]");
        assert_eq!(Command::GetMotorState.encode(), "[GMOV]");
        assert_eq!(Command::GetLight.encode(), "[GLON]");
        assert_eq!(Command::GetBrightness.encode(), "[GLBR]");
        assert_eq!(Command::GetHeaterTemp.encode(), "[GHTT]");
        assert_eq!(Command::StartMove.encode(), "[SMOV]");
    }

    #[test]
    fn encode_set_target_open_and_closed() {
        assert_eq!(Command::SetTarget(OPEN_ANGLE).encode(), "[STRG0]");
        assert_eq!(Command::SetTarget(CLOSED_ANGLE).encode(), "[STRG270]");
    }

    #[test]
    fn encode_set_light_on_off() {
        assert_eq!(Command::SetLight(true).encode(), "[SLON1]");
        assert_eq!(Command::SetLight(false).encode(), "[SLON0]");
    }

    #[test]
    fn encode_set_brightness_zero_pads_to_four_digits() {
        assert_eq!(Command::SetBrightness(0).encode(), "[SLBR0000]");
        assert_eq!(Command::SetBrightness(7).encode(), "[SLBR0007]");
        assert_eq!(Command::SetBrightness(123).encode(), "[SLBR0123]");
        assert_eq!(Command::SetBrightness(4096).encode(), "[SLBR4096]");
    }

    #[test]
    fn from_frame_strips_parens_and_whitespace() {
        let r = RawResponse::from_frame("(OK)").unwrap();
        assert_eq!(r.body, "OK");
        let r = RawResponse::from_frame("   (123)\n").unwrap();
        assert_eq!(r.body, "123");
    }

    #[test]
    fn from_frame_handles_leading_space_inside_parens() {
        let r = RawResponse::from_frame("( 26.104242)").unwrap();
        assert_eq!(r.body, "26.104242");
    }

    #[test]
    fn from_frame_rejects_missing_parens() {
        assert!(matches!(
            RawResponse::from_frame("OK"),
            Err(DsdFp2Error::MalformedResponse(_))
        ));
        assert!(matches!(
            RawResponse::from_frame("(OK"),
            Err(DsdFp2Error::MalformedResponse(_))
        ));
        assert!(matches!(
            RawResponse::from_frame("OK)"),
            Err(DsdFp2Error::MalformedResponse(_))
        ));
        assert!(matches!(
            RawResponse::from_frame(")("),
            Err(DsdFp2Error::MalformedResponse(_))
        ));
    }

    #[test]
    fn parse_ok_accepts_only_ok_body() {
        RawResponse::from_frame("(OK)").unwrap().parse_ok().unwrap();
        let err = RawResponse::from_frame("(ERR)")
            .unwrap()
            .parse_ok()
            .unwrap_err();
        assert!(matches!(err, DsdFp2Error::MalformedResponse(_)));
    }

    #[test]
    fn parse_int_handles_negative_and_zero() {
        assert_eq!(
            RawResponse::from_frame("(0)").unwrap().parse_int().unwrap(),
            0
        );
        assert_eq!(
            RawResponse::from_frame("(270)")
                .unwrap()
                .parse_int()
                .unwrap(),
            270
        );
        assert_eq!(
            RawResponse::from_frame("(-1)")
                .unwrap()
                .parse_int()
                .unwrap(),
            -1
        );
    }

    #[test]
    fn parse_u16_rejects_out_of_range() {
        RawResponse::from_frame("(-1)")
            .unwrap()
            .parse_u16()
            .unwrap_err();
        RawResponse::from_frame("(65536)")
            .unwrap()
            .parse_u16()
            .unwrap_err();
        assert_eq!(
            RawResponse::from_frame("(4096)")
                .unwrap()
                .parse_u16()
                .unwrap(),
            4096
        );
    }

    #[test]
    fn parse_bool_accepts_zero_one_only() {
        assert!(!RawResponse::from_frame("(0)")
            .unwrap()
            .parse_bool()
            .unwrap());
        assert!(RawResponse::from_frame("(1)")
            .unwrap()
            .parse_bool()
            .unwrap());
        RawResponse::from_frame("(2)")
            .unwrap()
            .parse_bool()
            .unwrap_err();
    }

    #[test]
    fn parse_temperature_round_trip() {
        let t = RawResponse::from_frame("( 26.104242)")
            .unwrap()
            .parse_temperature()
            .unwrap();
        assert!((t - 26.104_242).abs() < 1e-6);
        let t = RawResponse::from_frame("(-5.5)")
            .unwrap()
            .parse_temperature()
            .unwrap();
        assert!((t - (-5.5)).abs() < 1e-6);
    }

    #[test]
    fn parse_firmware_extracts_board_and_version_and_detects_fp2() {
        let info = RawResponse::from_frame("(Board=DeepSkyDad.FP2, Version=1.0.14.2)")
            .unwrap()
            .parse_firmware()
            .unwrap();
        assert_eq!(info.board, "DeepSkyDad.FP2");
        assert_eq!(info.version, "1.0.14.2");
        assert!(info.is_fp2());

        let info = RawResponse::from_frame("(Board=DeepSkyDad.FP1, Version=1.0.0)")
            .unwrap()
            .parse_firmware()
            .unwrap();
        assert!(!info.is_fp2());
    }

    #[test]
    fn parse_firmware_requires_both_fields() {
        RawResponse::from_frame("(Board=DeepSkyDad.FP2)")
            .unwrap()
            .parse_firmware()
            .unwrap_err();
        RawResponse::from_frame("(Version=1.0.14.2)")
            .unwrap()
            .parse_firmware()
            .unwrap_err();
    }
}

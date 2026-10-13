//! PHD2 event types and application state

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::Phd2Error;

/// Deserialize an `f64` of seconds (the PHD2 wire format) into a [`Duration`].
/// `try_from_secs_f64` rejects NaN, infinity, negative, and out-of-range
/// values — all surfaced as a serde error rather than a panic.
fn duration_from_secs_f64<'de, D>(de: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let secs = f64::deserialize(de)?;
    Duration::try_from_secs_f64(secs)
        .map_err(|e| serde::de::Error::custom(format!("invalid seconds {secs}: {e}")))
}

/// PHD2 application state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppState {
    Stopped,
    Selected,
    Calibrating,
    Guiding,
    LostLock,
    Paused,
    Looping,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "Stopped"),
            Self::Selected => write!(f, "Selected"),
            Self::Calibrating => write!(f, "Calibrating"),
            Self::Guiding => write!(f, "Guiding"),
            Self::LostLock => write!(f, "LostLock"),
            Self::Paused => write!(f, "Paused"),
            Self::Looping => write!(f, "Looping"),
        }
    }
}

impl std::str::FromStr for AppState {
    type Err = Phd2Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Stopped" => Ok(Self::Stopped),
            "Selected" => Ok(Self::Selected),
            "Calibrating" => Ok(Self::Calibrating),
            "Guiding" => Ok(Self::Guiding),
            "LostLock" => Ok(Self::LostLock),
            "Paused" => Ok(Self::Paused),
            "Looping" => Ok(Self::Looping),
            _ => Err(Phd2Error::InvalidState(format!("Unknown state: {s}"))),
        }
    }
}

/// Guide step statistics from PHD2
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GuideStepStats {
    pub frame: u64,
    #[serde(rename = "Time", deserialize_with = "duration_from_secs_f64")]
    pub time: Duration,
    pub mount: String,
    #[serde(rename = "dx")]
    pub dx: f64,
    #[serde(rename = "dy")]
    pub dy: f64,
    #[serde(rename = "RADistanceRaw")]
    pub ra_distance_raw: Option<f64>,
    #[serde(rename = "DECDistanceRaw")]
    pub dec_distance_raw: Option<f64>,
    #[serde(rename = "RADistanceGuide")]
    pub ra_distance_guide: Option<f64>,
    #[serde(rename = "DECDistanceGuide")]
    pub dec_distance_guide: Option<f64>,
    #[serde(rename = "RADuration")]
    pub ra_duration: Option<i32>,
    #[serde(rename = "RADirection")]
    pub ra_direction: Option<String>,
    #[serde(rename = "DECDuration")]
    pub dec_duration: Option<i32>,
    #[serde(rename = "DECDirection")]
    pub dec_direction: Option<String>,
    #[serde(rename = "StarMass")]
    pub star_mass: Option<f64>,
    #[serde(rename = "SNR")]
    pub snr: Option<f64>,
    #[serde(rename = "HFD")]
    pub hfd: Option<f64>,
    #[serde(rename = "AvgDist")]
    pub avg_dist: Option<f64>,
    #[serde(rename = "RALimited")]
    pub ra_limited: Option<bool>,
    #[serde(rename = "DecLimited")]
    pub dec_limited: Option<bool>,
    #[serde(rename = "ErrorCode")]
    pub error_code: Option<i32>,
}

/// PHD2 event notification
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "Event")]
pub enum Phd2Event {
    /// Sent on connection, contains PHD2 version info
    Version {
        #[serde(rename = "PHDVersion")]
        phd_version: String,
        #[serde(rename = "PHDSubver")]
        phd_subver: Option<String>,
        #[serde(rename = "MsgVersion")]
        msg_version: Option<u32>,
        #[serde(rename = "OverlapSupport")]
        overlap_support: Option<bool>,
    },

    /// Application state changed
    AppState {
        #[serde(rename = "State")]
        state: String,
    },

    /// Guide step with statistics
    GuideStep(GuideStepStats),

    /// Dither operation completed
    GuidingDithered {
        #[serde(rename = "dx")]
        dx: f64,
        #[serde(rename = "dy")]
        dy: f64,
    },

    /// Settling completed
    SettleDone {
        #[serde(rename = "Status")]
        status: i32,
        #[serde(rename = "Error")]
        error: Option<String>,
    },

    /// Star was selected
    StarSelected {
        #[serde(rename = "X")]
        x: f64,
        #[serde(rename = "Y")]
        y: f64,
    },

    /// Star was lost
    StarLost {
        #[serde(rename = "Frame")]
        frame: u64,
        #[serde(rename = "Time")]
        time: f64,
        #[serde(rename = "StarMass")]
        star_mass: f64,
        #[serde(rename = "SNR")]
        snr: f64,
        #[serde(rename = "AvgDist")]
        avg_dist: Option<f64>,
        #[serde(rename = "ErrorCode")]
        error_code: Option<i32>,
        #[serde(rename = "Status")]
        status: String,
    },

    /// Lock position was set
    LockPositionSet {
        #[serde(rename = "X")]
        x: f64,
        #[serde(rename = "Y")]
        y: f64,
    },

    /// Lock shift limit reached
    LockPositionShiftLimitReached,

    /// Calibration in progress
    Calibrating {
        #[serde(rename = "Mount")]
        mount: String,
        #[serde(rename = "dir")]
        dir: String,
        #[serde(rename = "dist")]
        dist: f64,
        #[serde(rename = "dx")]
        dx: f64,
        #[serde(rename = "dy")]
        dy: f64,
        #[serde(rename = "pos")]
        pos: Vec<f64>,
        #[serde(rename = "step")]
        step: u32,
        #[serde(rename = "State")]
        state: String,
    },

    /// Calibration finished
    CalibrationComplete {
        #[serde(rename = "Mount")]
        mount: String,
    },

    /// Calibration failed
    CalibrationFailed {
        #[serde(rename = "Reason")]
        reason: String,
    },

    /// Calibration was flipped
    CalibrationDataFlipped {
        #[serde(rename = "Mount")]
        mount: String,
    },

    /// Looping exposures started
    LoopingExposures {
        #[serde(rename = "Frame")]
        frame: u64,
    },

    /// Looping exposures stopped
    LoopingExposuresStopped,

    /// Guiding was paused
    Paused,

    /// Guiding was resumed
    Resumed,

    /// Guide parameter changed
    GuideParamChange {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Value")]
        value: serde_json::Value,
    },

    /// Configuration changed
    ConfigurationChange,

    /// Alert message
    Alert {
        #[serde(rename = "Msg")]
        msg: String,
        #[serde(rename = "Type")]
        alert_type: String,
    },

    /// Start guiding event
    StartGuiding,

    /// Settling in progress
    Settling {
        #[serde(rename = "Distance")]
        distance: f64,
        #[serde(rename = "Time", deserialize_with = "duration_from_secs_f64")]
        time: Duration,
        #[serde(rename = "SettleTime", deserialize_with = "duration_from_secs_f64")]
        settle_time: Duration,
        #[serde(rename = "StarLocked")]
        star_locked: bool,
    },

    /// Guiding stopped
    GuidingStopped,

    /// A `capture_single_frame` frame ended (PHD2 2.6.14+). Sent on
    /// success and on failure; `path` names the saved file when the
    /// capture asked PHD2 to save it, also on a failed save.
    SingleFrameComplete {
        #[serde(rename = "Success")]
        success: bool,
        #[serde(rename = "Error", default)]
        error: Option<String>,
        #[serde(rename = "Path", default)]
        path: Option<String>,
    },

    // ========================================================================
    // Connection state events (internal, not from PHD2)
    // ========================================================================
    /// Connection to PHD2 was lost
    #[serde(skip)]
    ConnectionLost {
        /// Reason for the connection loss
        reason: String,
    },

    /// Attempting to reconnect to PHD2
    #[serde(skip)]
    Reconnecting {
        /// Current reconnection attempt number
        attempt: u32,
        /// Maximum attempts (None for unlimited)
        max_attempts: Option<u32>,
    },

    /// Successfully reconnected to PHD2
    #[serde(skip)]
    Reconnected,

    /// Reconnection failed after all attempts
    #[serde(skip)]
    ReconnectFailed {
        /// Reason for final failure
        reason: String,
    },
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_version_event_parsing() {
        let json = r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::Version { phd_version, .. } => {
                assert_eq!(phd_version, "2.6.11");
            }
            _ => panic!("Expected Version event"),
        }
    }

    #[test]
    fn test_app_state_event_parsing() {
        let json = r#"{"Event":"AppState","State":"Guiding"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::AppState { state } => {
                assert_eq!(state, "Guiding");
            }
            _ => panic!("Expected AppState event"),
        }
    }

    #[test]
    fn test_app_state_from_str() {
        assert_eq!("Stopped".parse::<AppState>().unwrap(), AppState::Stopped);
        assert_eq!("Guiding".parse::<AppState>().unwrap(), AppState::Guiding);
        assert_eq!(
            "Calibrating".parse::<AppState>().unwrap(),
            AppState::Calibrating
        );
        assert!("Unknown".parse::<AppState>().is_err());
    }

    #[test]
    fn test_guide_step_parsing() {
        let json = r#"{"Event":"GuideStep","Frame":1,"Time":1.5,"Mount":"Mount","dx":0.5,"dy":-0.3,"RADistanceRaw":0.4,"DECDistanceRaw":-0.2}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::GuideStep(stats) => {
                assert_eq!(stats.frame, 1);
                assert_eq!(stats.dx, 0.5);
                assert_eq!(stats.dy, -0.3);
            }
            _ => panic!("Expected GuideStep event"),
        }
    }

    #[test]
    fn test_star_lost_event_parsing() {
        let json = r#"{"Event":"StarLost","Frame":10,"Time":5.0,"StarMass":1000.0,"SNR":15.5,"Status":"Lost"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::StarLost { frame, snr, .. } => {
                assert_eq!(frame, 10);
                assert_eq!(snr, 15.5);
            }
            _ => panic!("Expected StarLost event"),
        }
    }

    #[test]
    fn test_settle_done_event_success() {
        let json = r#"{"Event":"SettleDone","Status":0}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::SettleDone { status, error } => {
                assert_eq!(status, 0);
                assert!(error.is_none());
            }
            _ => panic!("Expected SettleDone event"),
        }
    }

    #[test]
    fn test_settle_done_event_failure() {
        let json = r#"{"Event":"SettleDone","Status":1,"Error":"Star lost during settle"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::SettleDone { status, error } => {
                assert_eq!(status, 1);
                assert_eq!(error.unwrap(), "Star lost during settle");
            }
            _ => panic!("Expected SettleDone event"),
        }
    }

    #[test]
    fn test_guiding_dithered_event() {
        let json = r#"{"Event":"GuidingDithered","dx":2.5,"dy":-1.3}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::GuidingDithered { dx, dy } => {
                assert_eq!(dx, 2.5);
                assert_eq!(dy, -1.3);
            }
            _ => panic!("Expected GuidingDithered event"),
        }
    }

    #[test]
    fn test_settling_event_negative_time_is_serde_error() {
        let json = r#"{"Event":"Settling","Distance":1.2,"Time":-1.0,"SettleTime":10.0,"StarLocked":true}"#;
        let err = serde_json::from_str::<Phd2Event>(json).unwrap_err();
        assert!(
            err.to_string().contains("invalid seconds"),
            "expected boundary error, got: {err}"
        );
    }

    #[test]
    fn test_settling_event_overflow_time_is_serde_error() {
        // 1e308 seconds is far beyond Duration::MAX. Without try_from_secs_f64
        // this would panic; the deserializer must surface a serde error.
        let json = r#"{"Event":"Settling","Distance":1.2,"Time":1e308,"SettleTime":10.0,"StarLocked":true}"#;
        let err = serde_json::from_str::<Phd2Event>(json).unwrap_err();
        assert!(
            err.to_string().contains("invalid seconds"),
            "expected boundary error, got: {err}"
        );
    }

    #[test]
    fn test_settling_event() {
        let json =
            r#"{"Event":"Settling","Distance":1.2,"Time":3.5,"SettleTime":10.0,"StarLocked":true}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::Settling {
                distance,
                time,
                settle_time,
                star_locked,
            } => {
                assert_eq!(distance, 1.2);
                assert_eq!(time, Duration::from_millis(3500));
                assert_eq!(settle_time, Duration::from_secs(10));
                assert!(star_locked);
            }
            _ => panic!("Expected Settling event"),
        }
    }

    #[test]
    fn test_paused_event() {
        let json = r#"{"Event":"Paused"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::Paused));
    }

    #[test]
    fn test_resumed_event() {
        let json = r#"{"Event":"Resumed"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::Resumed));
    }

    #[test]
    fn test_start_guiding_event() {
        let json = r#"{"Event":"StartGuiding"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::StartGuiding));
    }

    #[test]
    fn test_guiding_stopped_event() {
        let json = r#"{"Event":"GuidingStopped"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::GuidingStopped));
    }

    #[test]
    fn a_saved_single_frame_reports_its_path() {
        // PHD2 2.6.14's event, as read off the wire on pier1.
        let json = r#"{"Event":"SingleFrameComplete","Timestamp":1791927328.093,"Host":"pier1","Inst":1,"Success":true,"Path":"/tmp/phd2probe-wd/path-2000.fits"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        let Phd2Event::SingleFrameComplete {
            success,
            error,
            path,
        } = event
        else {
            panic!("expected SingleFrameComplete, got {event:?}");
        };
        assert!(success);
        assert_eq!(error, None);
        assert_eq!(path.as_deref(), Some("/tmp/phd2probe-wd/path-2000.fits"));
    }

    #[test]
    fn a_failed_single_frame_reports_phd2s_error_text() {
        let json = r#"{"Event":"SingleFrameComplete","Success":false,"Error":"camera error -- see debug log","Path":"/tmp/x.fits"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        let Phd2Event::SingleFrameComplete { success, error, .. } = event else {
            panic!("expected SingleFrameComplete, got {event:?}");
        };
        assert!(!success);
        assert_eq!(error.as_deref(), Some("camera error -- see debug log"));
    }

    #[test]
    fn an_unsaved_single_frame_has_no_path() {
        let json = r#"{"Event":"SingleFrameComplete","Success":true}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(
            event,
            Phd2Event::SingleFrameComplete { path: None, .. }
        ));
    }

    #[test]
    fn test_looping_exposures_event() {
        let json = r#"{"Event":"LoopingExposures","Frame":42}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::LoopingExposures { frame } => {
                assert_eq!(frame, 42);
            }
            _ => panic!("Expected LoopingExposures event"),
        }
    }

    #[test]
    fn test_looping_exposures_stopped_event() {
        let json = r#"{"Event":"LoopingExposuresStopped"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::LoopingExposuresStopped));
    }

    #[test]
    fn test_connection_lost_event() {
        let event = Phd2Event::ConnectionLost {
            reason: "Connection closed by remote".to_string(),
        };
        match event {
            Phd2Event::ConnectionLost { reason } => {
                assert_eq!(reason, "Connection closed by remote");
            }
            _ => panic!("Expected ConnectionLost event"),
        }
    }

    #[test]
    fn test_reconnecting_event() {
        let event = Phd2Event::Reconnecting {
            attempt: 3,
            max_attempts: Some(5),
        };
        match event {
            Phd2Event::Reconnecting {
                attempt,
                max_attempts,
            } => {
                assert_eq!(attempt, 3);
                assert_eq!(max_attempts, Some(5));
            }
            _ => panic!("Expected Reconnecting event"),
        }
    }

    #[test]
    fn test_reconnected_event() {
        let event = Phd2Event::Reconnected;
        assert!(matches!(event, Phd2Event::Reconnected));
    }

    #[test]
    fn test_reconnect_failed_event() {
        let event = Phd2Event::ReconnectFailed {
            reason: "Max retries exceeded".to_string(),
        };
        match event {
            Phd2Event::ReconnectFailed { reason } => {
                assert_eq!(reason, "Max retries exceeded");
            }
            _ => panic!("Expected ReconnectFailed event"),
        }
    }

    // ============================================================================
    // AppState Display Tests
    // ============================================================================

    #[test]
    fn test_app_state_display() {
        assert_eq!(format!("{}", AppState::Stopped), "Stopped");
        assert_eq!(format!("{}", AppState::Selected), "Selected");
        assert_eq!(format!("{}", AppState::Calibrating), "Calibrating");
        assert_eq!(format!("{}", AppState::Guiding), "Guiding");
        assert_eq!(format!("{}", AppState::LostLock), "LostLock");
        assert_eq!(format!("{}", AppState::Paused), "Paused");
        assert_eq!(format!("{}", AppState::Looping), "Looping");
    }

    #[test]
    fn test_app_state_from_str_all_states() {
        assert_eq!("Stopped".parse::<AppState>().unwrap(), AppState::Stopped);
        assert_eq!("Selected".parse::<AppState>().unwrap(), AppState::Selected);
        assert_eq!(
            "Calibrating".parse::<AppState>().unwrap(),
            AppState::Calibrating
        );
        assert_eq!("Guiding".parse::<AppState>().unwrap(), AppState::Guiding);
        assert_eq!("LostLock".parse::<AppState>().unwrap(), AppState::LostLock);
        assert_eq!("Paused".parse::<AppState>().unwrap(), AppState::Paused);
        assert_eq!("Looping".parse::<AppState>().unwrap(), AppState::Looping);
    }

    // ============================================================================
    // Additional Event Parsing Tests
    // ============================================================================

    #[test]
    fn test_star_selected_event() {
        let json = r#"{"Event":"StarSelected","X":256.5,"Y":512.3}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::StarSelected { x, y } => {
                assert_eq!(x, 256.5);
                assert_eq!(y, 512.3);
            }
            _ => panic!("Expected StarSelected event"),
        }
    }

    #[test]
    fn test_lock_position_set_event() {
        let json = r#"{"Event":"LockPositionSet","X":100.0,"Y":200.0}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::LockPositionSet { x, y } => {
                assert_eq!(x, 100.0);
                assert_eq!(y, 200.0);
            }
            _ => panic!("Expected LockPositionSet event"),
        }
    }

    #[test]
    fn test_lock_position_shift_limit_reached_event() {
        let json = r#"{"Event":"LockPositionShiftLimitReached"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::LockPositionShiftLimitReached));
    }

    #[test]
    fn test_calibrating_event() {
        let json = r#"{"Event":"Calibrating","Mount":"Mount","dir":"West","dist":10.5,"dx":5.0,"dy":2.0,"pos":[100.0,200.0],"step":3,"State":"calibrating"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::Calibrating {
                mount,
                dir,
                dist,
                dx,
                dy,
                pos,
                step,
                state,
            } => {
                assert_eq!(mount, "Mount");
                assert_eq!(dir, "West");
                assert_eq!(dist, 10.5);
                assert_eq!(dx, 5.0);
                assert_eq!(dy, 2.0);
                assert_eq!(pos, vec![100.0, 200.0]);
                assert_eq!(step, 3);
                assert_eq!(state, "calibrating");
            }
            _ => panic!("Expected Calibrating event"),
        }
    }

    #[test]
    fn test_calibration_complete_event() {
        let json = r#"{"Event":"CalibrationComplete","Mount":"Mount"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::CalibrationComplete { mount } => {
                assert_eq!(mount, "Mount");
            }
            _ => panic!("Expected CalibrationComplete event"),
        }
    }

    #[test]
    fn test_calibration_failed_event() {
        let json = r#"{"Event":"CalibrationFailed","Reason":"Star lost during calibration"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::CalibrationFailed { reason } => {
                assert_eq!(reason, "Star lost during calibration");
            }
            _ => panic!("Expected CalibrationFailed event"),
        }
    }

    #[test]
    fn test_calibration_data_flipped_event() {
        let json = r#"{"Event":"CalibrationDataFlipped","Mount":"Mount"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::CalibrationDataFlipped { mount } => {
                assert_eq!(mount, "Mount");
            }
            _ => panic!("Expected CalibrationDataFlipped event"),
        }
    }

    #[test]
    fn test_guide_param_change_event() {
        let json = r#"{"Event":"GuideParamChange","Name":"Aggressiveness","Value":0.75}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::GuideParamChange { name, value } => {
                assert_eq!(name, "Aggressiveness");
                assert_eq!(value, serde_json::json!(0.75));
            }
            _ => panic!("Expected GuideParamChange event"),
        }
    }

    #[test]
    fn test_configuration_change_event() {
        let json = r#"{"Event":"ConfigurationChange"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        assert!(matches!(event, Phd2Event::ConfigurationChange));
    }

    #[test]
    fn test_alert_event() {
        let json = r#"{"Event":"Alert","Msg":"PHD2 message","Type":"info"}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::Alert { msg, alert_type } => {
                assert_eq!(msg, "PHD2 message");
                assert_eq!(alert_type, "info");
            }
            _ => panic!("Expected Alert event"),
        }
    }

    #[test]
    fn test_version_event_with_overlap_support() {
        let json = r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"dev3","MsgVersion":1,"OverlapSupport":true}"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::Version {
                phd_version,
                phd_subver,
                msg_version,
                overlap_support,
            } => {
                assert_eq!(phd_version, "2.6.11");
                assert_eq!(phd_subver, Some("dev3".to_string()));
                assert_eq!(msg_version, Some(1));
                assert_eq!(overlap_support, Some(true));
            }
            _ => panic!("Expected Version event"),
        }
    }

    #[test]
    fn test_guide_step_with_all_fields() {
        let json = r#"{
            "Event":"GuideStep",
            "Frame":100,
            "Time":5.5,
            "Mount":"Mount",
            "dx":0.5,
            "dy":-0.3,
            "RADistanceRaw":0.4,
            "DECDistanceRaw":-0.2,
            "RADistanceGuide":0.35,
            "DECDistanceGuide":-0.15,
            "RADuration":150,
            "RADirection":"West",
            "DECDuration":100,
            "DECDirection":"North",
            "StarMass":15000.0,
            "SNR":25.5,
            "HFD":3.2,
            "AvgDist":0.45,
            "RALimited":false,
            "DecLimited":false,
            "ErrorCode":0
        }"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::GuideStep(stats) => {
                assert_eq!(stats.frame, 100);
                assert_eq!(stats.time, Duration::from_millis(5500));
                assert_eq!(stats.mount, "Mount");
                assert_eq!(stats.dx, 0.5);
                assert_eq!(stats.dy, -0.3);
                assert_eq!(stats.ra_distance_raw, Some(0.4));
                assert_eq!(stats.dec_distance_raw, Some(-0.2));
                assert_eq!(stats.ra_distance_guide, Some(0.35));
                assert_eq!(stats.dec_distance_guide, Some(-0.15));
                assert_eq!(stats.ra_duration, Some(150));
                assert_eq!(stats.ra_direction, Some("West".to_string()));
                assert_eq!(stats.dec_duration, Some(100));
                assert_eq!(stats.dec_direction, Some("North".to_string()));
                assert_eq!(stats.star_mass, Some(15000.0));
                assert_eq!(stats.snr, Some(25.5));
                assert_eq!(stats.hfd, Some(3.2));
                assert_eq!(stats.avg_dist, Some(0.45));
                assert_eq!(stats.ra_limited, Some(false));
                assert_eq!(stats.dec_limited, Some(false));
                assert_eq!(stats.error_code, Some(0));
            }
            _ => panic!("Expected GuideStep event"),
        }
    }

    #[test]
    fn test_star_lost_with_all_fields() {
        let json = r#"{
            "Event":"StarLost",
            "Frame":50,
            "Time":10.0,
            "StarMass":5000.0,
            "SNR":5.0,
            "AvgDist":2.5,
            "ErrorCode":1,
            "Status":"LostLock"
        }"#;
        let event: Phd2Event = serde_json::from_str(json).unwrap();
        match event {
            Phd2Event::StarLost {
                frame,
                time,
                star_mass,
                snr,
                avg_dist,
                error_code,
                status,
            } => {
                assert_eq!(frame, 50);
                assert_eq!(time, 10.0);
                assert_eq!(star_mass, 5000.0);
                assert_eq!(snr, 5.0);
                assert_eq!(avg_dist, Some(2.5));
                assert_eq!(error_code, Some(1));
                assert_eq!(status, "LostLock");
            }
            _ => panic!("Expected StarLost event"),
        }
    }
}

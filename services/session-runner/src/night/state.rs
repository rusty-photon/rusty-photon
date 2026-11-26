//! `night.json`: where the schedule is in its night (design § Night
//! Schedule → Restarts).

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// The file's name in `state_dir`.
const FILE_NAME: &str = "night.json";

/// The schedule's phase in its night.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Waiting for dusk.
    Day,
    /// Dusk's wait for a safe roof and a free run slot.
    Dusk,
    PowerUp,
    /// The camera check, the covers and the cooldown.
    CameraCheck,
    Runs,
    Dawn,
    AfterDawn,
}

/// A power-box output, as the power map names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutputRef {
    pub switch_id: String,
    pub output: String,
}

/// An output dawn left on, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftOn {
    pub switch_id: String,
    pub output: String,
    pub reason: String,
}

/// The schedule's place in one night.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NightState {
    /// The date `get_twilight` reads the night for.
    pub night: NaiveDate,
    pub dusk: DateTime<Utc>,
    pub dawn: DateTime<Utc>,
    pub phase: Phase,
    /// The daily outputs that did not read on before dusk's first
    /// `power_up`.
    #[serde(default)]
    pub outputs_off_before_dusk: Vec<OutputRef>,
    /// The index in `night.runs` of the next run to start.
    #[serde(default)]
    pub next_run: usize,
    /// The calibrators whose covers dusk still tries to open.
    #[serde(default)]
    pub covers_to_open: Vec<String>,
    /// The cameras given up for the night.
    #[serde(default)]
    pub cameras_given_up: Vec<String>,
    /// When dawn called `start_warmup`.
    #[serde(default)]
    pub warmup_started_at: Option<DateTime<Utc>>,
    /// The outputs dawn left on.
    #[serde(default)]
    pub outputs_left_on: Vec<LeftOn>,
}

impl NightState {
    /// A night not yet begun: phase `day`, nothing recorded.
    #[must_use]
    pub const fn ahead(night: NaiveDate, dusk: DateTime<Utc>, dawn: DateTime<Utc>) -> Self {
        Self {
            night,
            dusk,
            dawn,
            phase: Phase::Day,
            outputs_off_before_dusk: Vec::new(),
            next_run: 0,
            covers_to_open: Vec::new(),
            cameras_given_up: Vec::new(),
            warmup_started_at: None,
            outputs_left_on: Vec::new(),
        }
    }

    /// `<state_dir>/night.json`.
    #[must_use]
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(FILE_NAME)
    }

    /// Read the file. A missing file is `None`; an unreadable one is
    /// logged and `None`, so the schedule starts as if there were none.
    pub async fn load(state_dir: &Path) -> Option<Self> {
        let path = Self::path(state_dir);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "cannot read night.json; starting without it");
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(state) => Some(state),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "unreadable night.json; starting without it");
                None
            }
        }
    }

    /// Write the file with the workspace atomic-write pattern the
    /// blackboard uses, so a crash never leaves half a file.
    ///
    /// # Errors
    ///
    /// The serialization or I/O error, with the path.
    pub async fn write(&self, state_dir: &Path) -> Result<(), String> {
        let path = Self::path(state_dir);
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let target = path.clone();
        tokio::task::spawn_blocking(move || crate::blackboard::write_atomic(&target, &bytes))
            .await
            .map_err(|e| {
                format!(
                    "cannot write {}: write task join error: {e}",
                    path.display()
                )
            })?
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> NightState {
        let mut state = NightState::ahead(
            NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
        );
        state.phase = Phase::AfterDawn;
        state.cameras_given_up.push("main-cam".into());
        state.outputs_left_on.push(LeftOn {
            switch_id: "ppba".into(),
            output: "Quad 12V Output".into(),
            reason: "main-cam's cooler is running".into(),
        });
        state
    }

    #[tokio::test]
    async fn the_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample();
        state.write(dir.path()).await.unwrap();
        assert_eq!(NightState::load(dir.path()).await, Some(state));
    }

    #[tokio::test]
    async fn no_file_is_no_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(NightState::load(dir.path()).await, None);
    }

    #[tokio::test]
    async fn an_unreadable_file_is_no_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(NightState::path(dir.path()), b"{ not json").unwrap();
        assert_eq!(NightState::load(dir.path()).await, None);
    }

    #[test]
    fn the_phases_serialize_as_the_design_names_them() {
        let names: Vec<_> = [
            Phase::Day,
            Phase::Dusk,
            Phase::PowerUp,
            Phase::CameraCheck,
            Phase::Runs,
            Phase::Dawn,
            Phase::AfterDawn,
        ]
        .iter()
        .map(|p| serde_json::to_value(p).unwrap())
        .collect();
        assert_eq!(
            serde_json::Value::Array(names),
            serde_json::json!([
                "day",
                "dusk",
                "power_up",
                "camera_check",
                "runs",
                "dawn",
                "after_dawn"
            ])
        );
    }

    #[test]
    fn a_file_with_only_the_night_and_its_phase_reads_with_empty_records() {
        let state: NightState = serde_json::from_value(serde_json::json!({
            "night": "2026-11-23",
            "dusk": "2026-11-24T00:58:00Z",
            "dawn": "2026-11-24T14:31:00Z",
            "phase": "day"
        }))
        .unwrap();
        assert_eq!(
            state,
            NightState::ahead(
                NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(),
                Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
            )
        );
    }
}

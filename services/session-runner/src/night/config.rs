//! The `night` configuration block (design § Night Schedule → `night`
//! configuration) and its load checks.

use std::path::Path;
use std::time::Duration;

use chrono::TimeDelta;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::document::{bind_parameters, resolve_workflow_path, Document};
use crate::runs::issue_messages;

/// The farthest an offset may move dusk or dawn from civil twilight.
const MAX_OFFSET: TimeDelta = TimeDelta::hours(6);

/// The longest `settle` `rp`'s `power_up` takes.
const MAX_SETTLE: Duration = Duration::from_mins(5);

/// The night schedule: when dusk and dawn fall, the times the power-up
/// and the warm-up get, and the runs in order.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NightConfig {
    /// Dusk, from the evening end of civil twilight.
    #[serde(default)]
    pub dusk_offset: SignedDuration,
    /// Dawn, from the morning end of civil twilight.
    #[serde(default)]
    pub dawn_offset: SignedDuration,
    /// Passed to `power_up`: the time between the outputs going on and
    /// the services restarting.
    #[serde(default = "default_settle", with = "humantime_serde")]
    pub settle: Duration,
    /// How long dawn waits for the coolers to stop, and how long an
    /// output whose camera cannot be read stays on after dawn's
    /// `start_warmup`.
    #[serde(default = "default_warmup_timeout", with = "humantime_serde")]
    pub warmup_timeout: Duration,
    /// The documents the night runs, in order.
    #[serde(default)]
    pub runs: Vec<NightRun>,
}

const fn default_settle() -> Duration {
    Duration::from_secs(30)
}

const fn default_warmup_timeout() -> Duration {
    Duration::from_mins(45)
}

/// One run of the night, as a `POST /runs` body names it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NightRun {
    pub workflow: String,
    /// The document's parameters; absent means `{}`.
    #[serde(default)]
    pub params: Option<Value>,
}

/// A duration with a sign: `"-30m"` is thirty minutes earlier, `"1h"`
/// or `"+1h"` an hour later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignedDuration(pub TimeDelta);

impl SignedDuration {
    /// Parse `[+|-]<humantime duration>`.
    ///
    /// # Errors
    ///
    /// A message naming the text when the duration does not parse or
    /// does not fit a [`TimeDelta`].
    pub fn parse(text: &str) -> Result<Self, String> {
        let out_of_range = || format!("`{text}` is out of range for a duration");
        let negative = text.starts_with('-');
        let magnitude = text.strip_prefix(['-', '+']).unwrap_or(text);
        let duration = humantime::parse_duration(magnitude)
            .map_err(|e| format!("`{text}` is not a signed duration: {e}"))?;
        let delta = TimeDelta::from_std(duration).map_err(|_| out_of_range())?;
        if negative {
            TimeDelta::zero()
                .checked_sub(&delta)
                .map(Self)
                .ok_or_else(out_of_range)
        } else {
            Ok(Self(delta))
        }
    }
}

impl<'de> Deserialize<'de> for SignedDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Check the `night` block as far as `session-runner` can without `rp`.
///
/// Each offset lies within ±6 h, `settle` within what `power_up` takes,
/// and every run's document resolves in `workflows_dir`, passes the
/// schema layer, and binds its `params`. The catalog layer runs when the
/// run starts, against the live `rp`.
///
/// # Errors
///
/// A message naming the field at fault.
pub fn check(night: &NightConfig, workflows_dir: &Path) -> Result<(), String> {
    for (field, offset) in [
        ("dusk_offset", night.dusk_offset),
        ("dawn_offset", night.dawn_offset),
    ] {
        if offset.0.abs() > MAX_OFFSET {
            return Err(format!(
                "night.{field} must lie within ±6h of civil twilight"
            ));
        }
    }
    if night.settle > MAX_SETTLE {
        return Err("night.settle must be at most 5m, the longest power_up takes".to_owned());
    }
    for (index, run) in night.runs.iter().enumerate() {
        check_run(index, run, workflows_dir)?;
    }
    Ok(())
}

fn check_run(index: usize, run: &NightRun, workflows_dir: &Path) -> Result<(), String> {
    let field = format!("night.runs[{index}]");
    let path = resolve_workflow_path(workflows_dir, &run.workflow)
        .map_err(|e| format!("{field}.workflow: {e}"))?;
    let source = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{field}.workflow: cannot read `{}` at {}: {e}",
            run.workflow,
            path.display()
        )
    })?;
    let document = Document::parse(&source).map_err(|issues| {
        format!(
            "{field}.workflow: `{}` failed validation: {}",
            run.workflow,
            issue_messages(&issues)
        )
    })?;
    let params = run
        .params
        .clone()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !params.is_object() {
        return Err(format!("{field}.params must be a JSON object"));
    }
    bind_parameters(&document.parameters, Some(&params))
        .map_err(|issues| format!("{field}.params: {}", issue_messages(&issues)))?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const FLATS: &str = r#"{
        "version": 1,
        "name": "flats",
        "parameters": { "camera_id": { "type": "string", "required": true } },
        "root": { "sequence": [] }
    }"#;

    fn night(json: &str) -> NightConfig {
        serde_json::from_str(json).unwrap()
    }

    fn workflows() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("flats.json"), FLATS).unwrap();
        dir
    }

    #[test]
    fn an_empty_block_takes_the_defaults() {
        let night = night("{}");
        assert_eq!(night.dusk_offset, SignedDuration(TimeDelta::zero()));
        assert_eq!(night.dawn_offset, SignedDuration(TimeDelta::zero()));
        assert_eq!(night.settle, Duration::from_secs(30));
        assert_eq!(night.warmup_timeout, Duration::from_mins(45));
        assert!(night.runs.is_empty());
    }

    #[test]
    fn an_offset_reads_with_its_sign() {
        assert_eq!(
            SignedDuration::parse("-30m").unwrap(),
            SignedDuration(TimeDelta::minutes(-30))
        );
        assert_eq!(
            SignedDuration::parse("+1h 15m").unwrap(),
            SignedDuration(TimeDelta::minutes(75))
        );
        assert_eq!(
            SignedDuration::parse("45s").unwrap(),
            SignedDuration(TimeDelta::seconds(45))
        );
        let err = SignedDuration::parse("soon").unwrap_err();
        assert!(err.contains("soon"), "{err}");
        let err = SignedDuration::parse("--30m").unwrap_err();
        assert!(err.contains("--30m"), "{err}");
    }

    #[test]
    fn an_unknown_key_in_the_block_is_refused() {
        let err = serde_json::from_str::<NightConfig>(r#"{ "dusk": "0s" }"#).unwrap_err();
        assert!(err.to_string().contains("dusk"), "{err}");
    }

    #[test]
    fn an_offset_past_six_hours_fails_the_check_naming_the_field() {
        let dir = workflows();
        let err = check(&night(r#"{ "dawn_offset": "-6h 1m" }"#), dir.path()).unwrap_err();
        assert!(err.contains("night.dawn_offset"), "{err}");
        check(&night(r#"{ "dusk_offset": "-6h" }"#), dir.path()).unwrap();
    }

    #[test]
    fn a_settle_past_five_minutes_fails_the_check() {
        let dir = workflows();
        let err = check(&night(r#"{ "settle": "6m" }"#), dir.path()).unwrap_err();
        assert!(err.contains("night.settle"), "{err}");
    }

    #[test]
    fn a_run_whose_document_does_not_resolve_fails_naming_its_index() {
        let dir = workflows();
        let block = night(
            r#"{ "runs": [ { "workflow": "flats", "params": { "camera_id": "main-cam" } },
                           { "workflow": "no_such_document" } ] }"#,
        );
        let err = check(&block, dir.path()).unwrap_err();
        assert!(err.starts_with("night.runs[1].workflow"), "{err}");
    }

    #[test]
    fn a_run_whose_params_do_not_bind_fails_naming_its_params() {
        let dir = workflows();
        let block =
            night(r#"{ "runs": [ { "workflow": "flats", "params": { "camera_id": 7 } } ] }"#);
        let err = check(&block, dir.path()).unwrap_err();
        assert!(err.starts_with("night.runs[0].params"), "{err}");
    }

    #[test]
    fn a_run_whose_document_fails_the_schema_layer_fails_the_check() {
        let dir = workflows();
        std::fs::write(dir.path().join("broken.json"), r#"{ "version": 1 }"#).unwrap();
        let err = check(
            &night(r#"{ "runs": [ { "workflow": "broken" } ] }"#),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.starts_with("night.runs[0].workflow"), "{err}");
    }

    #[test]
    fn a_run_with_good_params_passes() {
        let dir = workflows();
        let block = night(
            r#"{ "dusk_offset": "-30m", "runs": [ { "workflow": "flats", "params": { "camera_id": "main-cam" } } ] }"#,
        );
        check(&block, dir.path()).unwrap();
    }
}

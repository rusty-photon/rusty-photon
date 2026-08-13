use std::time::Duration;

use rusty_photon_config::actions::FieldError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The dark-library setpoint grid: `cooler_targets_c` values must be
/// multiples of [`COOLER_GRID_STEP_C`] within
/// [`COOLER_GRID_MIN_C`]..=[`COOLER_GRID_MAX_C`] (rp.md § Camera
/// Cooling). The same constants drive the JSON-Schema `enum` the web
/// UI renders as a checkbox grid, so schema and validation cannot
/// drift apart.
pub const COOLER_GRID_MIN_C: i32 = -40;
pub const COOLER_GRID_MAX_C: i32 = 15;
pub const COOLER_GRID_STEP_C: i32 = 5;
// A non-positive step would silently desynchronize the schema grid from
// the rem_euclid validation below; fail the build instead.
const _: () = assert!(COOLER_GRID_STEP_C > 0, "cooler grid step must be positive");

/// Every valid rung, ascending.
pub fn cooler_grid() -> impl Iterator<Item = i32> {
    // The step is a positive constant; the fallback keeps `step_by`'s
    // nonzero contract without a cast.
    (COOLER_GRID_MIN_C..=COOLER_GRID_MAX_C)
        .step_by(usize::try_from(COOLER_GRID_STEP_C).unwrap_or(1))
}

/// Schema for `cooler_targets_c`: an array whose items enumerate the
/// 5 °C grid. Expressed as `items.enum` (not `minimum`/`maximum` +
/// `multipleOf`) so a schema-driven UI can render one checkbox per
/// allowed value without knowing the grid.
fn cooler_targets_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": {
            "type": "integer",
            "enum": cooler_grid().collect::<Vec<_>>(),
        },
        "uniqueItems": true,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub alpaca_url: String,
    #[serde(default)]
    pub device_type: String,
    #[serde(default)]
    pub device_number: u32,
    /// The dark-library setpoint ladder (rp.md § Camera Cooling):
    /// exactly the sensor temperatures the operator maintains dark
    /// libraries for, as unique integers on the 5 °C grid. At session
    /// start rp selects the lowest rung the cooler can hold tonight
    /// and regulates there for the whole session. Empty (the default)
    /// means rp never touches this camera's cooler.
    #[serde(default)]
    #[schemars(schema_with = "cooler_targets_schema")]
    pub cooler_targets_c: Vec<i32>,
    #[serde(default)]
    pub gain: Option<i32>,
    #[serde(default)]
    pub offset: Option<i32>,
    /// Estimated sensor readout + download time for this camera, feeding
    /// the predictive exposure deadline (§2.4 of the predictive-deadlines
    /// plan): `predicted = exposure duration + readout_time_estimate`. There
    /// is no ASCOM Alpaca property for readout time — even where a driver
    /// knows it, it is not exposed — so this config value is the estimate
    /// source. Set it per-rig (a slow USB-2 CCD reads out far slower than a
    /// USB-3 CMOS); omitted → a conservative built-in default. rp does
    /// **not** enforce this deadline (the camera driver owns the exposure
    /// and rp keeps a separate, more generous readout backstop); it rides
    /// the `exposure_started` envelope for the Sentinel watchdog to track.
    /// Accepts a humantime string (e.g. `"8s"`). See `docs/services/rp.md`
    /// §"Event Envelope".
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub readout_time_estimate: Option<Duration>,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default)]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

impl CameraConfig {
    /// Range-validate the camera as field-level errors (empty = valid),
    /// with `index` naming its position in `equipment.cameras`. Shared by
    /// `load_config` (which aborts startup on the first error) and the REST
    /// `PUT /api/config` validation. Paths are dotted with the index
    /// (`equipment.cameras.0.focal_length_mm`) so a UI can map each error
    /// onto its field; the message names the camera id for humans.
    #[must_use]
    pub fn field_errors(&self, index: usize) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let off_grid: Vec<i32> = self
            .cooler_targets_c
            .iter()
            .copied()
            .filter(|t| {
                !(COOLER_GRID_MIN_C..=COOLER_GRID_MAX_C).contains(t)
                    || t.rem_euclid(COOLER_GRID_STEP_C) != 0
            })
            .collect();
        if !off_grid.is_empty() {
            errors.push(FieldError {
                path: format!("equipment.cameras.{index}.cooler_targets_c"),
                msg: format!(
                    "must be multiples of {COOLER_GRID_STEP_C} within \
                     {COOLER_GRID_MIN_C}..={COOLER_GRID_MAX_C}; got {off_grid:?} (camera '{}')",
                    self.id
                ),
            });
        }
        let mut seen = std::collections::HashSet::new();
        let duplicates: Vec<i32> = self
            .cooler_targets_c
            .iter()
            .copied()
            .filter(|t| !seen.insert(*t))
            .collect();
        if !duplicates.is_empty() {
            errors.push(FieldError {
                path: format!("equipment.cameras.{index}.cooler_targets_c"),
                msg: format!(
                    "must not contain duplicates; got {duplicates:?} (camera '{}')",
                    self.id
                ),
            });
        }
        errors
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    /// The pre-train `focal_length_mm` camera key was retired to
    /// `equipment.optical_trains[].focal_length_mm`; a config still
    /// carrying it must fail loudly at load (rp.md § Configuration).
    #[test]
    fn camera_config_rejects_retired_focal_length_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "focal_length_mm": 540.0
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("focal_length_mm"), "{err}");
    }

    #[test]
    fn camera_config_readout_time_estimate_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "readout_time_estimate": "8s"
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.equipment.cameras[0].readout_time_estimate,
            Some(std::time::Duration::from_secs(8))
        );
    }

    #[test]
    fn camera_config_readout_time_estimate_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120"
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.equipment.cameras[0]
                .readout_time_estimate
                .is_none(),
            "omitted readout_time_estimate must deserialize to None (the do_capture default applies)"
        );
    }

    #[test]
    fn camera_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "colour": "mono"
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("colour"), "{err}");
    }

    #[test]
    fn cooler_targets_on_the_grid_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "cooler_targets_c": [-10, 5]
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.equipment.cameras[0].cooler_targets_c, vec![-10, 5]);
    }

    #[test]
    fn cooler_targets_default_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120"
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.equipment.cameras[0].cooler_targets_c.is_empty(),
            "an omitted ladder must deserialize to empty (rp never touches the cooler)"
        );
    }

    #[test]
    fn cooler_targets_off_grid_value_is_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "cooler_targets_c": [-12]
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let msg = load_config(&path).unwrap_err().to_string();
        assert!(
            msg.contains("cooler_targets_c") && msg.contains("main-cam") && msg.contains("-12"),
            "expected grid diagnostic naming the camera and value, got: {msg}"
        );
    }

    #[test]
    fn cooler_targets_out_of_range_value_is_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "cooler_targets_c": [-45]
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let msg = load_config(&path).unwrap_err().to_string();
        assert!(
            msg.contains("cooler_targets_c") && msg.contains("-45"),
            "expected range diagnostic, got: {msg}"
        );
    }

    #[test]
    fn cooler_targets_duplicate_value_is_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {
                            "id": "main-cam",
                            "alpaca_url": "http://localhost:11120",
                            "cooler_targets_c": [-10, -10]
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let msg = load_config(&path).unwrap_err().to_string();
        assert!(
            msg.contains("cooler_targets_c") && msg.contains("duplicates"),
            "expected duplicate diagnostic, got: {msg}"
        );
    }

    /// The schema advertises the grid as `items.enum` so the web UI can
    /// render one checkbox per rung without hardcoding the values
    /// (docs/services/ui-htmx.md § Schema-driven rendering).
    #[test]
    fn cooler_targets_schema_enumerates_the_grid() {
        let schema = schemars::schema_for!(crate::config::CameraConfig);
        let value = serde_json::to_value(&schema).unwrap();
        let field = value
            .pointer("/properties/cooler_targets_c")
            .expect("schema must carry the cooler_targets_c property");
        assert_eq!(field.pointer("/type").unwrap(), "array");
        assert_eq!(field.pointer("/items/type").unwrap(), "integer");
        assert_eq!(field.pointer("/uniqueItems").unwrap(), true);
        let grid: Vec<i64> = field
            .pointer("/items/enum")
            .and_then(|v| v.as_array())
            .expect("items must enumerate the grid")
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(
            grid,
            vec![-40, -35, -30, -25, -20, -15, -10, -5, 0, 5, 10, 15],
            "the enum must list every 5 °C rung from -40 to +15, ascending"
        );
    }
}

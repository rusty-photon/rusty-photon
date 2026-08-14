//! The equipment roster domain: rp's config `equipment` block parsed into
//! renderable entries, the kind ⇄ ASCOM-type mapping, the roster-derived
//! config-page keys (`rp:{kind}:{id}`), and the read-modify-write **surgery**
//! that add/edit/remove perform on rp's config value before `PUT /api/config`.
//!
//! Everything here is pure data manipulation over `serde_json::Value` (rp's
//! config schema is the authority on entry shapes — the BFF hardcodes only the
//! ten equipment kinds, which are rp's wire contract per `rp.md`).

use serde_json::Value;
use strum::VariantArray;

/// The fixed id under which the singular mount is addressed in roster routes
/// and `rp:{kind}:{id}` keys (rp's mount entry has no id of its own).
pub const MOUNT_ID: &str = "mount";

/// One equipment kind — the ten keys of rp's config `equipment` block.
///
/// `Switches`, `Rotators`, `ObservingConditions`, and `Domes` cover roster
/// membership and connectivity status only (rp.md § Equipment Integration) —
/// same generic add/edit/remove/config-page treatment as every other kind,
/// but rp exposes no MCP tool integration for them yet.
///
/// Variant declaration order is `VariantArray::VARIANTS` order, which is the
/// order [`parse_roster`] walks the equipment block in and the order the
/// equipment page renders its sections in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray)]
pub enum EquipKind {
    Cameras,
    FilterWheels,
    CoverCalibrators,
    Focusers,
    SafetyMonitors,
    Switches,
    Rotators,
    ObservingConditions,
    Domes,
    Mount,
}

impl EquipKind {
    /// The key in rp's config `equipment` block (also the `{kind}` route
    /// segment and the middle of an `rp:{kind}:{id}` service key).
    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Cameras => "cameras",
            Self::FilterWheels => "filter_wheels",
            Self::CoverCalibrators => "cover_calibrators",
            Self::Focusers => "focusers",
            Self::SafetyMonitors => "safety_monitors",
            Self::Switches => "switches",
            Self::Rotators => "rotators",
            Self::ObservingConditions => "observing_conditions",
            Self::Domes => "domes",
            Self::Mount => "mount",
        }
    }

    /// The ASCOM Alpaca device type for entries of this kind — how a
    /// roster-derived config page addresses the device's own Alpaca server.
    #[must_use]
    pub const fn ascom_type(self) -> &'static str {
        match self {
            Self::Cameras => "camera",
            Self::FilterWheels => "filterwheel",
            Self::CoverCalibrators => "covercalibrator",
            Self::Focusers => "focuser",
            Self::SafetyMonitors => "safetymonitor",
            Self::Switches => "switch",
            Self::Rotators => "rotator",
            Self::ObservingConditions => "observingconditions",
            Self::Domes => "dome",
            Self::Mount => "telescope",
        }
    }

    /// Section heading on the equipment page.
    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Cameras => "Cameras",
            Self::FilterWheels => "Filter wheels",
            Self::CoverCalibrators => "Cover calibrators",
            Self::Focusers => "Focusers",
            Self::SafetyMonitors => "Safety monitors",
            Self::Switches => "Switches",
            Self::Rotators => "Rotators",
            Self::ObservingConditions => "Observing conditions",
            Self::Domes => "Domes",
            Self::Mount => "Mount",
        }
    }

    /// The mount is one-per-observatory (`rp.md`); every other kind is a list.
    #[must_use]
    pub const fn is_singular(self) -> bool {
        matches!(self, Self::Mount)
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::VARIANTS
            .iter()
            .copied()
            .find(|k| k.config_key() == key)
    }
}

/// One roster entry parsed from rp's config (secrets already redacted by rp).
#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub kind: EquipKind,
    /// The operator-supplied config id ([`MOUNT_ID`] for the singular mount).
    pub id: String,
    /// Optional display name from the entry's `name` field.
    pub name: Option<String>,
    pub alpaca_url: String,
    pub device_number: u32,
    /// Whether the entry carries device credentials (present but redacted) —
    /// a roster-derived config page probes without them, so this drives the
    /// "add a static drivers entry" hint.
    pub has_auth: bool,
    /// The raw entry value (redacted), the base blob for the edit form.
    pub raw: Value,
}

impl RosterEntry {
    /// The `/config/{service}` key for this entry's roster-derived config page.
    #[must_use]
    pub fn service_key(&self) -> String {
        format!("rp:{}:{}", self.kind.config_key(), self.id)
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

fn entry_from_value(kind: EquipKind, id: String, value: &Value) -> RosterEntry {
    RosterEntry {
        kind,
        id,
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        alpaca_url: value
            .get("alpaca_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        device_number: value
            .get("device_number")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        has_auth: value.get("auth").is_some_and(|a| !a.is_null()),
        raw: value.clone(),
    }
}

/// Parse rp's config value into the flat roster, in kind order then config
/// order. Entries rp's validation would reject can't appear here — the value
/// comes from rp's own `GET /api/config`.
pub fn parse_roster(config: &Value) -> Vec<RosterEntry> {
    let mut entries = Vec::new();
    let Some(equipment) = config.get("equipment") else {
        return entries;
    };
    for kind in EquipKind::VARIANTS.iter().copied() {
        let node = equipment.get(kind.config_key());
        if kind.is_singular() {
            if let Some(mount) = node.filter(|v| !v.is_null()) {
                entries.push(entry_from_value(kind, MOUNT_ID.to_string(), mount));
            }
            continue;
        }
        let Some(list) = node.and_then(Value::as_array) else {
            continue;
        };
        for item in list {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            entries.push(entry_from_value(kind, id, item));
        }
    }
    entries
}

/// Find one entry by kind + id ([`MOUNT_ID`] addresses the mount).
#[must_use]
pub fn find_entry(config: &Value, kind: EquipKind, id: &str) -> Option<RosterEntry> {
    parse_roster(config)
        .into_iter()
        .find(|e| e.kind == kind && e.id == id)
}

/// A roster mutation that could not be applied to the current config value
/// (the roster changed underneath the form, or a duplicate id was submitted).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SurgeryError {
    #[error("no {0} entry with id \"{1}\" — the roster may have changed; reload the page")]
    NotFound(&'static str, String),
    #[error("a {0} entry with id \"{1}\" already exists")]
    DuplicateId(&'static str, String),
    #[error("a mount is already configured — edit or remove it instead")]
    MountAlreadyPresent,
    #[error("the rp config has no editable equipment block")]
    MalformedConfig,
}

/// The dotted path prefix rp's validation errors carry for the touched entry
/// (e.g. `equipment.cameras.2.`) — used to re-anchor them onto the form's
/// relative field names.
fn error_prefix(kind: EquipKind, index: Option<usize>) -> String {
    match index {
        Some(i) => format!("equipment.{}.{i}.", kind.config_key()),
        None => format!("equipment.{}.", kind.config_key()),
    }
}

/// Get the equipment block as a mutable object, creating it if absent.
fn equipment_mut(config: &mut Value) -> Result<&mut serde_json::Map<String, Value>, SurgeryError> {
    let root = config
        .as_object_mut()
        .ok_or(SurgeryError::MalformedConfig)?;
    let equipment = root
        .entry("equipment".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    equipment
        .as_object_mut()
        .ok_or(SurgeryError::MalformedConfig)
}

/// Insert a new entry; returns the dotted error prefix for the new position.
pub fn insert_entry(
    config: &mut Value,
    kind: EquipKind,
    entry: Value,
) -> Result<String, SurgeryError> {
    if kind.is_singular() {
        let equipment = equipment_mut(config)?;
        if equipment.get("mount").is_some_and(|m| !m.is_null()) {
            return Err(SurgeryError::MountAlreadyPresent);
        }
        equipment.insert("mount".to_string(), entry);
        return Ok(error_prefix(kind, None));
    }
    let submitted_id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
    if find_in_kind(config, kind, submitted_id).is_some() {
        return Err(SurgeryError::DuplicateId(
            kind.config_key(),
            submitted_id.to_string(),
        ));
    }
    let equipment = equipment_mut(config)?;
    let list = equipment
        .entry(kind.config_key().to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(items) = list else {
        return Err(SurgeryError::MalformedConfig);
    };
    items.push(entry);
    Ok(error_prefix(kind, Some(items.len().saturating_sub(1))))
}

/// Replace the entry at kind+id; returns the dotted error prefix of its
/// position. A rename (the submitted entry carries a different `id`) is
/// allowed, but not onto an id another entry already holds.
pub fn replace_entry(
    config: &mut Value,
    kind: EquipKind,
    id: &str,
    entry: Value,
) -> Result<String, SurgeryError> {
    if kind.is_singular() {
        let equipment = equipment_mut(config)?;
        if equipment
            .get("mount")
            .is_none_or(serde_json::Value::is_null)
        {
            return Err(SurgeryError::NotFound(kind.config_key(), id.to_string()));
        }
        equipment.insert("mount".to_string(), entry);
        return Ok(error_prefix(kind, None));
    }
    let index = index_of(config, kind, id)
        .ok_or(SurgeryError::NotFound(kind.config_key(), id.to_string()))?;
    let new_id = entry.get("id").and_then(Value::as_str).unwrap_or_default();
    if new_id != id && find_in_kind(config, kind, new_id).is_some() {
        return Err(SurgeryError::DuplicateId(
            kind.config_key(),
            new_id.to_string(),
        ));
    }
    let equipment = equipment_mut(config)?;
    let Some(Value::Array(items)) = equipment.get_mut(kind.config_key()) else {
        return Err(SurgeryError::MalformedConfig);
    };
    // `index_of` found the entry in this same array just above.
    let Some(slot) = items.get_mut(index) else {
        return Err(SurgeryError::MalformedConfig);
    };
    *slot = entry;
    Ok(error_prefix(kind, Some(index)))
}

/// Remove the entry at kind+id (the mount becomes `null`).
pub fn remove_entry(config: &mut Value, kind: EquipKind, id: &str) -> Result<(), SurgeryError> {
    if kind.is_singular() {
        let equipment = equipment_mut(config)?;
        if equipment
            .get("mount")
            .is_none_or(serde_json::Value::is_null)
        {
            return Err(SurgeryError::NotFound(kind.config_key(), id.to_string()));
        }
        equipment.insert("mount".to_string(), Value::Null);
        return Ok(());
    }
    let index = index_of(config, kind, id)
        .ok_or(SurgeryError::NotFound(kind.config_key(), id.to_string()))?;
    let equipment = equipment_mut(config)?;
    let Some(Value::Array(items)) = equipment.get_mut(kind.config_key()) else {
        return Err(SurgeryError::MalformedConfig);
    };
    items.remove(index);
    Ok(())
}

fn find_in_kind<'a>(config: &'a Value, kind: EquipKind, id: &str) -> Option<&'a Value> {
    config
        .pointer(&format!("/equipment/{}", kind.config_key()))?
        .as_array()?
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
}

fn index_of(config: &Value, kind: EquipKind, id: &str) -> Option<usize> {
    config
        .pointer(&format!("/equipment/{}", kind.config_key()))?
        .as_array()?
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
}

/// Parse an `rp:{kind}:{id}` service key (the roster-derived config targets).
/// The id may itself contain `:` — only the first two separators split.
#[must_use]
pub fn parse_service_key(service: &str) -> Option<(EquipKind, &str)> {
    let rest = service.strip_prefix("rp:")?;
    let (kind_key, id) = rest.split_once(':')?;
    let kind = EquipKind::from_key(kind_key)?;
    if id.is_empty() {
        return None;
    }
    Some((kind, id))
}

/// Re-anchor rp's absolute validation-error paths (`equipment.cameras.2.gain`)
/// onto the entry form's relative field names (`gain`). Errors outside the
/// touched entry keep their absolute path (rendered in the form's banner).
#[must_use]
pub fn relativize_errors(
    errors: Vec<rusty_photon_config::actions::FieldError>,
    prefix: &str,
) -> Vec<rusty_photon_config::actions::FieldError> {
    errors
        .into_iter()
        .map(|e| match e.path.strip_prefix(prefix) {
            Some(rel) => rusty_photon_config::actions::FieldError {
                path: rel.to_string(),
                msg: e.msg,
            },
            None => e,
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_config() -> Value {
        json!({
            "server": { "port": 11115 },
            "equipment": {
                "cameras": [
                    { "id": "main-cam", "name": "Main Camera",
                      "alpaca_url": "http://127.0.0.1:11121", "device_number": 0,
                      "auth": { "username": "obs", "password": "********" } },
                    { "id": "guide-cam", "alpaca_url": "http://127.0.0.1:11122" }
                ],
                "focusers": [
                    { "id": "main-focuser", "alpaca_url": "http://127.0.0.1:11113",
                      "device_number": 1 }
                ],
                "switches": [
                    { "id": "ppba", "name": "Pegasus PPBA",
                      "alpaca_url": "http://127.0.0.1:11112", "device_number": 0 }
                ],
                "mount": { "alpaca_url": "http://127.0.0.1:11116", "device_number": 0 }
            }
        })
    }

    /// The order `parse_roster` walks the equipment block in and the order the
    /// equipment page renders its sections in.
    #[test]
    fn all_kinds_are_listed_in_declaration_order() {
        let all: &[EquipKind] = EquipKind::VARIANTS;
        assert_eq!(
            all.to_vec(),
            vec![
                EquipKind::Cameras,
                EquipKind::FilterWheels,
                EquipKind::CoverCalibrators,
                EquipKind::Focusers,
                EquipKind::SafetyMonitors,
                EquipKind::Switches,
                EquipKind::Rotators,
                EquipKind::ObservingConditions,
                EquipKind::Domes,
                EquipKind::Mount,
            ]
        );
    }

    /// rp's on-disk config object keys, the `/equipment/{kind}` route segments
    /// and the middle of every `rp:{kind}:{id}` service key — a wire contract
    /// shared with rp and with committed snapshots.
    #[test]
    fn config_keys_are_pinned() {
        let keys: Vec<&str> = EquipKind::VARIANTS.iter().map(|k| k.config_key()).collect();
        assert_eq!(
            keys,
            vec![
                "cameras",
                "filter_wheels",
                "cover_calibrators",
                "focusers",
                "safety_monitors",
                "switches",
                "rotators",
                "observing_conditions",
                "domes",
                "mount",
            ]
        );
    }

    /// The ASCOM Alpaca device-type path segments a roster-derived config page
    /// addresses the device's own server with.
    #[test]
    fn ascom_types_are_pinned() {
        let types: Vec<&str> = EquipKind::VARIANTS.iter().map(|k| k.ascom_type()).collect();
        assert_eq!(
            types,
            vec![
                "camera",
                "filterwheel",
                "covercalibrator",
                "focuser",
                "safetymonitor",
                "switch",
                "rotator",
                "observingconditions",
                "dome",
                "telescope",
            ]
        );
    }

    /// Sentence-case section headings on the equipment page.
    #[test]
    fn display_names_are_pinned() {
        let names: Vec<&str> = EquipKind::VARIANTS.iter().map(|k| k.display()).collect();
        assert_eq!(
            names,
            vec![
                "Cameras",
                "Filter wheels",
                "Cover calibrators",
                "Focusers",
                "Safety monitors",
                "Switches",
                "Rotators",
                "Observing conditions",
                "Domes",
                "Mount",
            ]
        );
    }

    #[test]
    fn from_key_round_trips_every_kind() {
        for kind in EquipKind::VARIANTS.iter().copied() {
            assert_eq!(EquipKind::from_key(kind.config_key()), Some(kind));
        }
    }

    #[test]
    fn from_key_rejects_near_misses() {
        // Singular/plural, casing, the dotted-path spelling, and the ASCOM type
        // names are all distinct from the config keys.
        for key in [
            "camera",
            "Cameras",
            "filterwheel",
            "filter_wheel",
            "cover_calibrator",
            "observing_condition",
            "safetymonitor",
            "telescope",
            "mounts",
            "equipment.cameras",
            "",
        ] {
            assert_eq!(EquipKind::from_key(key), None, "{key}");
        }
    }

    #[test]
    fn parse_roster_lists_every_entry_in_kind_order() {
        let roster = parse_roster(&sample_config());
        let keys: Vec<String> = roster.iter().map(RosterEntry::service_key).collect();
        assert_eq!(
            keys,
            vec![
                "rp:cameras:main-cam",
                "rp:cameras:guide-cam",
                "rp:focusers:main-focuser",
                "rp:switches:ppba",
                "rp:mount:mount",
            ]
        );
    }

    #[test]
    fn parse_roster_reads_entry_fields() {
        let roster = parse_roster(&sample_config());
        let main = &roster[0];
        assert_eq!(main.display_name(), "Main Camera");
        assert_eq!(main.alpaca_url, "http://127.0.0.1:11121");
        assert_eq!(main.device_number, 0);
        assert!(main.has_auth);
        let guide = &roster[1];
        assert_eq!(guide.display_name(), "guide-cam");
        assert!(!guide.has_auth);
        let focuser = &roster[2];
        assert_eq!(focuser.device_number, 1);
        assert_eq!(focuser.kind.ascom_type(), "focuser");
        let switch = &roster[3];
        assert_eq!(switch.display_name(), "Pegasus PPBA");
        assert_eq!(switch.kind.ascom_type(), "switch");
    }

    #[test]
    fn parse_roster_without_equipment_is_empty() {
        assert!(parse_roster(&json!({ "server": {} })).is_empty());
        assert!(parse_roster(&json!({ "equipment": {} })).is_empty());
        assert!(parse_roster(&json!({ "equipment": { "mount": null } })).is_empty());
    }

    #[test]
    fn insert_appends_and_reports_the_error_prefix() {
        let mut config = sample_config();
        let prefix = insert_entry(
            &mut config,
            EquipKind::Cameras,
            json!({ "id": "third-cam", "alpaca_url": "http://x:1" }),
        )
        .unwrap();
        assert_eq!(prefix, "equipment.cameras.2.");
        assert_eq!(
            config
                .pointer("/equipment/cameras/2/id")
                .and_then(Value::as_str),
            Some("third-cam")
        );
    }

    #[test]
    fn insert_rejects_a_duplicate_id() {
        let mut config = sample_config();
        let err = insert_entry(
            &mut config,
            EquipKind::Cameras,
            json!({ "id": "main-cam", "alpaca_url": "http://x:1" }),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SurgeryError::DuplicateId("cameras", "main-cam".to_string())
        );
    }

    #[test]
    fn insert_creates_a_missing_kind_array() {
        let mut config = json!({ "equipment": {} });
        let prefix = insert_entry(
            &mut config,
            EquipKind::FilterWheels,
            json!({ "id": "fw", "alpaca_url": "http://x:1" }),
        )
        .unwrap();
        assert_eq!(prefix, "equipment.filter_wheels.0.");
    }

    #[test]
    fn insert_mount_only_when_absent() {
        let mut config = sample_config();
        let err = insert_entry(
            &mut config,
            EquipKind::Mount,
            json!({ "alpaca_url": "http://x:1" }),
        )
        .unwrap_err();
        assert_eq!(err, SurgeryError::MountAlreadyPresent);

        remove_entry(&mut config, EquipKind::Mount, MOUNT_ID).unwrap();
        let prefix = insert_entry(
            &mut config,
            EquipKind::Mount,
            json!({ "alpaca_url": "http://x:1" }),
        )
        .unwrap();
        assert_eq!(prefix, "equipment.mount.");
    }

    #[test]
    fn replace_swaps_the_entry_in_place() {
        let mut config = sample_config();
        let prefix = replace_entry(
            &mut config,
            EquipKind::Cameras,
            "guide-cam",
            json!({ "id": "guide-cam", "alpaca_url": "http://new:2" }),
        )
        .unwrap();
        assert_eq!(prefix, "equipment.cameras.1.");
        assert_eq!(
            config
                .pointer("/equipment/cameras/1/alpaca_url")
                .and_then(Value::as_str),
            Some("http://new:2")
        );
        // The sibling entry is untouched.
        assert_eq!(
            config
                .pointer("/equipment/cameras/0/id")
                .and_then(Value::as_str),
            Some("main-cam")
        );
    }

    #[test]
    fn replace_missing_entry_is_not_found() {
        let mut config = sample_config();
        let err = replace_entry(&mut config, EquipKind::Focusers, "nope", json!({})).unwrap_err();
        assert_eq!(err, SurgeryError::NotFound("focusers", "nope".to_string()));
    }

    #[test]
    fn replace_allows_rename_but_not_onto_an_existing_id() {
        let mut config = sample_config();
        // Renaming guide-cam onto main-cam's id would create a duplicate key.
        let err = replace_entry(
            &mut config,
            EquipKind::Cameras,
            "guide-cam",
            json!({ "id": "main-cam", "alpaca_url": "http://x:1" }),
        )
        .unwrap_err();
        assert_eq!(
            err,
            SurgeryError::DuplicateId("cameras", "main-cam".to_string())
        );

        // A rename to a fresh id is fine.
        replace_entry(
            &mut config,
            EquipKind::Cameras,
            "guide-cam",
            json!({ "id": "wide-cam", "alpaca_url": "http://x:1" }),
        )
        .unwrap();
        assert!(find_entry(&config, EquipKind::Cameras, "wide-cam").is_some());
        assert!(find_entry(&config, EquipKind::Cameras, "guide-cam").is_none());
    }

    #[test]
    fn remove_deletes_list_entries_and_nulls_the_mount() {
        let mut config = sample_config();
        remove_entry(&mut config, EquipKind::Cameras, "main-cam").unwrap();
        assert_eq!(
            config
                .pointer("/equipment/cameras/0/id")
                .and_then(Value::as_str),
            Some("guide-cam")
        );
        remove_entry(&mut config, EquipKind::Mount, MOUNT_ID).unwrap();
        assert!(config.pointer("/equipment/mount").unwrap().is_null());
        let err = remove_entry(&mut config, EquipKind::Mount, MOUNT_ID).unwrap_err();
        assert_eq!(err, SurgeryError::NotFound("mount", "mount".to_string()));
    }

    #[test]
    fn service_keys_parse_and_reject() {
        assert!(matches!(
            parse_service_key("rp:cameras:main-cam"),
            Some((EquipKind::Cameras, "main-cam"))
        ));
        // An id containing a colon splits only on the first two separators.
        assert!(matches!(
            parse_service_key("rp:focusers:oag:fine"),
            Some((EquipKind::Focusers, "oag:fine"))
        ));
        assert!(parse_service_key("rp:spectrographs:x").is_none()); // unknown kind
        assert!(parse_service_key("rp:cameras:").is_none()); // empty id
        assert!(parse_service_key("dsd-fp2").is_none()); // not roster-derived
    }

    #[test]
    fn relativize_errors_strips_the_touched_entry_prefix_only() {
        let errors = vec![
            rusty_photon_config::actions::FieldError {
                path: "equipment.cameras.2.gain".to_string(),
                msg: "out of range".to_string(),
            },
            rusty_photon_config::actions::FieldError {
                path: "site.latitude_degrees".to_string(),
                msg: "unrelated".to_string(),
            },
        ];
        let rel = relativize_errors(errors, "equipment.cameras.2.");
        assert_eq!(rel[0].path, "gain");
        assert_eq!(rel[1].path, "site.latitude_degrees");
    }

    #[test]
    fn find_entry_returns_the_raw_blob_for_edit_forms() {
        let entry = find_entry(&sample_config(), EquipKind::Cameras, "main-cam").unwrap();
        assert_eq!(
            entry.raw.pointer("/auth/password").and_then(Value::as_str),
            Some("********")
        );
        assert!(find_entry(&sample_config(), EquipKind::Cameras, "nope").is_none());
        let mount = find_entry(&sample_config(), EquipKind::Mount, MOUNT_ID).unwrap();
        assert_eq!(mount.alpaca_url, "http://127.0.0.1:11116");
    }
}

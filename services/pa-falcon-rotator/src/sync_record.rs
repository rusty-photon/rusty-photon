//! The sync-offset record: the driver-side `Sync` offset, kept beside the
//! config file so that it outlives the process, as ASCOM requires of a
//! rotator's sync offset.
//!
//! See the design doc's
//! [§"The sync-offset record"](../../../docs/services/falcon-rotator.md#the-sync-offset-record).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::units::SyncOffset;

/// What the record holds on disk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sync_offset_deg: f64,
}

/// Where the record for the config file at `config_path` lives: beside
/// it, named after it (`pa-falcon-rotator.json` →
/// `pa-falcon-rotator.sync-offset.json`).
#[must_use]
pub fn record_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map_or_else(|| "pa-falcon-rotator".into(), |s| s.to_string_lossy());
    config_path.with_file_name(format!("{stem}.sync-offset.json"))
}

/// What reading the record found.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadRecord {
    /// No record: no `Sync` yet.
    Absent,
    Offset(SyncOffset),
    /// A record that cannot be read or does not parse, with why.
    Unreadable(String),
}

/// Read the record at `path`. Blocking file I/O: call it on the blocking
/// pool.
#[must_use]
pub fn read(path: &Path) -> ReadRecord {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Record>(&text) {
            Ok(record) if record.sync_offset_deg.is_finite() => {
                ReadRecord::Offset(SyncOffset::new(record.sync_offset_deg))
            }
            Ok(record) => ReadRecord::Unreadable(format!(
                "{} holds a non-finite offset {}",
                path.display(),
                record.sync_offset_deg
            )),
            Err(e) => ReadRecord::Unreadable(format!("{} does not parse: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReadRecord::Absent,
        Err(e) => ReadRecord::Unreadable(format!("{} cannot be read: {e}", path.display())),
    }
}

/// Replace the record at `path` with `offset`, atomically and durably.
/// Blocking file I/O: call it on the blocking pool.
///
/// # Errors
///
/// Returns the I/O error of the step of the atomic write that failed.
pub fn write(path: &Path, offset: SyncOffset) -> std::io::Result<()> {
    let value = serde_json::to_value(Record {
        sync_offset_deg: offset.value(),
    })
    .map_err(std::io::Error::other)?;
    rusty_photon_config::save(path, &value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_record_is_named_after_the_config_file_beside_it() {
        assert_eq!(
            record_path(Path::new("/etc/rp/pa-falcon-rotator.json")),
            PathBuf::from("/etc/rp/pa-falcon-rotator.sync-offset.json")
        );
    }

    /// `pkg/doctor.toml` declares the record as this service's state file,
    /// so doctor never flags it as a misnamed config (docs/services/doctor.md
    /// §The derived catalog); the name must be the one the driver writes
    /// beside its packaged config.
    #[test]
    fn the_doctor_catalog_declares_the_record_as_the_state_file() {
        let meta =
            rusty_photon_server_config::doctor_toml::parse(include_str!("../pkg/doctor.toml"))
                .unwrap();
        let record = record_path(Path::new("pa-falcon-rotator.json"));
        assert_eq!(
            meta.state_file.as_deref(),
            record.file_name().and_then(|name| name.to_str())
        );
    }

    #[test]
    fn no_record_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read(&dir.path().join("config.sync-offset.json")),
            ReadRecord::Absent
        );
    }

    #[test]
    fn a_written_offset_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        write(&path, SyncOffset::new(254.2)).unwrap();
        assert_eq!(read(&path), ReadRecord::Offset(SyncOffset::new(254.2)));
    }

    #[test]
    fn a_record_with_finer_digits_reads_back_to_a_hundredth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        std::fs::write(&path, "{\"sync_offset_deg\": 255.20000000000005}").unwrap();
        let ReadRecord::Offset(offset) = read(&path) else {
            panic!("expected an offset");
        };
        assert_eq!(offset.value(), 255.2);
    }

    #[test]
    fn the_record_holds_the_offset_under_its_documented_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        write(&path, SyncOffset::new(37.5)).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({ "sync_offset_deg": 37.5 }));
    }

    #[test]
    fn a_record_that_does_not_parse_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        std::fs::write(&path, "{\"sync_offset\": 12}").unwrap();
        let ReadRecord::Unreadable(why) = read(&path) else {
            panic!("expected an unreadable record");
        };
        assert!(why.contains("does not parse"), "{why}");
    }

    #[test]
    fn a_directory_in_place_of_the_record_is_unreadable_and_unwritable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read(&path), ReadRecord::Unreadable(_)));
        write(&path, SyncOffset::new(37.5)).unwrap_err();
    }
}

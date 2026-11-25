//! Schema version and migration steps for the `meta["schema_version"]` key.
//!
//! Distinct from redb's own file-format generation (see
//! [`crate::TargetStoreError::RedbUpgradeRequired`]): this versions the
//! *shape* of the serialized [`crate::Target`] value. Additive,
//! non-breaking field changes need no migration step at all — value structs
//! `#[serde(default)]` their new fields and tolerate unknown ones, so an
//! old value deserializes into the new `Target` directly. A step is only
//! authored for a breaking re-shape (rename, split, type change). Removing
//! a field an older build requires needs no step either, but bumps the
//! version: an older build then refuses the database at open, where it
//! would otherwise fail on every row this build has written.

use crate::error::TargetStoreError;

/// The schema version this build writes for a fresh database and expects
/// after migration.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Checks a database's on-disk `schema_version` against
/// [`CURRENT_SCHEMA_VERSION`].
///
/// A row this build writes lacks version 1's required `priority`, and a
/// version 1 row keeps it, which this build ignores on read, so no
/// migration step exists and this function only classifies `found`
/// against `CURRENT_SCHEMA_VERSION`. A breaking
/// `Target` re-shape adds an ordered step here, run inside the caller's
/// single write transaction before the version key is bumped.
///
/// # Errors
///
/// Returns [`TargetStoreError::UnsupportedSchemaVersion`] if `found` is
/// newer than this build supports.
pub const fn check_schema_version(found: u32) -> Result<(), TargetStoreError> {
    if found > CURRENT_SCHEMA_VERSION {
        return Err(TargetStoreError::UnsupportedSchemaVersion {
            found,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    // found < CURRENT_SCHEMA_VERSION runs the ordered migration steps here;
    // from version 1 there are none.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_passes() {
        check_schema_version(CURRENT_SCHEMA_VERSION).unwrap();
    }

    #[test]
    fn older_version_passes_with_no_migration_step_to_run() {
        check_schema_version(CURRENT_SCHEMA_VERSION - 1).unwrap();
    }

    #[test]
    fn newer_version_is_rejected() {
        let err = check_schema_version(CURRENT_SCHEMA_VERSION + 1).unwrap_err();
        assert!(matches!(
            err,
            TargetStoreError::UnsupportedSchemaVersion { .. }
        ));
    }
}

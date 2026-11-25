//! [`RedbTargetStore`], the shipped [`crate::TargetStore`] implementation.
//! See `docs/crates/rp-targets.md` "`RedbTargetStore` (raw-redb
//! implementation)" for the on-disk layout and migration contract.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::error::TargetStoreError;
use crate::migrate::{check_schema_version, CURRENT_SCHEMA_VERSION};
use crate::model::{validate_goals, AcquisitionGoal, Target, TargetSlug, WriteStamp};
use crate::TargetStore;

const TARGETS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("targets");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// [`TargetStore`] backed by a single-file `redb` database. Opened once at
/// `rp` startup and shared behind an `Arc`; every trait method runs its
/// redb transaction on the Tokio blocking pool.
#[derive(Debug, Clone)]
pub struct RedbTargetStore {
    db: Arc<Database>,
}

impl RedbTargetStore {
    /// Opens (creating if absent) the redb database at `path`, initializing
    /// a fresh database's `schema_version` or migrating an existing one.
    ///
    /// # Errors
    ///
    /// Returns [`TargetStoreError::RedbUpgradeRequired`] if the file was
    /// written by an older redb file-format generation,
    /// [`TargetStoreError::UnsupportedSchemaVersion`] if it was written by
    /// a newer build of this crate, or the relevant I/O variant otherwise.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, TargetStoreError> {
        let path = path.into();
        let db = tokio::task::spawn_blocking(move || open_and_init(&path))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))??;
        Ok(Self { db: Arc::new(db) })
    }
}

fn open_and_init(path: &Path) -> Result<Database, TargetStoreError> {
    let db = Database::create(path).map_err(|e| match e {
        redb::DatabaseError::UpgradeRequired(_) => TargetStoreError::RedbUpgradeRequired,
        other => TargetStoreError::Open(other),
    })?;

    let write_txn = db.begin_write()?;
    {
        // Touch the targets table so a fresh file always has both tables,
        // even before the first target is written.
        write_txn.open_table(TARGETS_TABLE)?;

        let mut meta = write_txn.open_table(META_TABLE)?;
        let found = match meta.get(SCHEMA_VERSION_KEY)? {
            None => None,
            Some(bytes) => Some(decode_schema_version(bytes.value())?),
        };
        match found {
            None => {
                meta.insert(
                    SCHEMA_VERSION_KEY,
                    CURRENT_SCHEMA_VERSION.to_le_bytes().as_slice(),
                )?;
            }
            Some(found) => {
                check_schema_version(found)?;
                if found < CURRENT_SCHEMA_VERSION {
                    meta.insert(
                        SCHEMA_VERSION_KEY,
                        CURRENT_SCHEMA_VERSION.to_le_bytes().as_slice(),
                    )?;
                }
            }
        }
    }
    write_txn.commit()?;

    Ok(db)
}

fn decode_schema_version(bytes: &[u8]) -> Result<u32, TargetStoreError> {
    let array: [u8; 4] = bytes.try_into().map_err(|_| {
        TargetStoreError::Corrupt(format!(
            "schema_version value is {} bytes, expected 4",
            bytes.len()
        ))
    })?;
    Ok(u32::from_le_bytes(array))
}

fn get_target_sync(db: &Database, slug: &str) -> Result<Option<Target>, TargetStoreError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(TARGETS_TABLE)?;
    let target = match table.get(slug)? {
        Some(value) => Some(serde_json::from_slice(value.value())?),
        None => None,
    };
    Ok(target)
}

fn list_targets_sync(db: &Database) -> Result<Vec<Target>, TargetStoreError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(TARGETS_TABLE)?;
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        out.push(serde_json::from_slice(value.value())?);
    }
    Ok(out)
}

fn upsert_target_sync(db: &Database, mut target: Target) -> Result<(), TargetStoreError> {
    validate_goals(&target.goals)?;
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(TARGETS_TABLE)?;
        let prior = match table.get(target.slug.as_str())? {
            Some(existing) => {
                let existing: Target = serde_json::from_slice(existing.value())?;
                Some((existing.created_at, existing.created_by))
            }
            None => None,
        };
        if let Some((created_at, created_by)) = prior {
            target.created_at = created_at;
            target.created_by = created_by;
        }
        let bytes = serde_json::to_vec(&target)?;
        table.insert(target.slug.as_str(), bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

fn delete_target_sync(db: &Database, slug: &str) -> Result<bool, TargetStoreError> {
    let write_txn = db.begin_write()?;
    let existed = {
        let mut table = write_txn.open_table(TARGETS_TABLE)?;
        let removed = table.remove(slug)?.is_some();
        removed
    };
    write_txn.commit()?;
    Ok(existed)
}

fn set_goals_sync(
    db: &Database,
    slug: &str,
    goals: Vec<AcquisitionGoal>,
    stamp: WriteStamp,
) -> Result<(), TargetStoreError> {
    validate_goals(&goals)?;
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(TARGETS_TABLE)?;
        let mut target: Target = match table.get(slug)? {
            Some(existing) => serde_json::from_slice(existing.value())?,
            None => {
                return Err(TargetStoreError::NotFound {
                    slug: slug.to_string(),
                });
            }
        };
        target.goals = goals;
        target.updated_at = stamp.updated_at;
        target.updated_by = stamp.updated_by;
        let bytes = serde_json::to_vec(&target)?;
        table.insert(slug, bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

#[async_trait]
impl TargetStore for RedbTargetStore {
    async fn upsert_target(&self, target: Target) -> Result<(), TargetStoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || upsert_target_sync(&db, target))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))?
    }

    async fn get_target(&self, slug: &TargetSlug) -> Result<Option<Target>, TargetStoreError> {
        let db = Arc::clone(&self.db);
        let slug = slug.as_str().to_string();
        tokio::task::spawn_blocking(move || get_target_sync(&db, &slug))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))?
    }

    async fn list_targets(&self) -> Result<Vec<Target>, TargetStoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || list_targets_sync(&db))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))?
    }

    async fn delete_target(&self, slug: &TargetSlug) -> Result<bool, TargetStoreError> {
        let db = Arc::clone(&self.db);
        let slug = slug.as_str().to_string();
        tokio::task::spawn_blocking(move || delete_target_sync(&db, &slug))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))?
    }

    async fn set_goals(
        &self,
        slug: &TargetSlug,
        goals: Vec<AcquisitionGoal>,
        stamp: WriteStamp,
    ) -> Result<(), TargetStoreError> {
        let db = Arc::clone(&self.db);
        let slug = slug.as_str().to_string();
        tokio::task::spawn_blocking(move || set_goals_sync(&db, &slug, goals, stamp))
            .await
            .map_err(|e| TargetStoreError::Join(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_target(slug: &str) -> Target {
        Target {
            slug: TargetSlug::new(slug).unwrap(),
            display_name: "NGC 7000".to_string(),
            coord: crate::IcrsCoord::try_new(20.9738, 44.5197).unwrap(),
            catalog_ref: Some("NGC 7000".to_string()),
            object_type: Some("Nebula".to_string()),
            magnitude: Some(4.0),
            size_arcmin: Some(120.0),
            position_angle_degrees: None,
            active: false,
            goals: Vec::new(),
            scheduling: None,
            grading: None,
            notes: None,
            created_at: "2026-07-22T00:00:00Z".to_string(),
            updated_at: "2026-07-22T00:00:00Z".to_string(),
            created_by: "operator".to_string(),
            updated_by: "operator".to_string(),
        }
    }

    async fn open_temp() -> (RedbTargetStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbTargetStore::open(dir.path().join("targets.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn get_absent_target_returns_none() {
        let (store, _dir) = open_temp().await;
        let slug = TargetSlug::new("ngc7000").unwrap();
        assert_eq!(store.get_target(&slug).await.unwrap(), None);
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let (store, _dir) = open_temp().await;
        let target = sample_target("ngc7000");
        store.upsert_target(target.clone()).await.unwrap();
        let slug = TargetSlug::new("ngc7000").unwrap();
        assert_eq!(store.get_target(&slug).await.unwrap(), Some(target));
    }

    #[tokio::test]
    async fn upsert_of_existing_slug_overwrites_and_keeps_created_at() {
        let (store, _dir) = open_temp().await;
        let mut target = sample_target("ngc7000");
        target.created_at = "2020-01-01T00:00:00Z".to_string();
        store.upsert_target(target).await.unwrap();

        let mut updated = sample_target("ngc7000");
        updated.display_name = "NGC 7000 — North America Nebula".to_string();
        updated.created_at = "2026-07-22T00:00:00Z".to_string();
        store.upsert_target(updated).await.unwrap();

        let slug = TargetSlug::new("ngc7000").unwrap();
        let stored = store.get_target(&slug).await.unwrap().unwrap();
        assert_eq!(stored.created_at, "2020-01-01T00:00:00Z");
        assert_eq!(stored.display_name, "NGC 7000 — North America Nebula");
        assert_eq!(store.list_targets().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_targets_is_sorted_by_slug() {
        let (store, _dir) = open_temp().await;
        store
            .upsert_target(sample_target("ngc7000-2"))
            .await
            .unwrap();
        store.upsert_target(sample_target("ngc7000")).await.unwrap();
        store.upsert_target(sample_target("m33")).await.unwrap();

        let slugs: Vec<String> = store
            .list_targets()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.slug.as_str().to_string())
            .collect();
        assert_eq!(slugs, vec!["m33", "ngc7000", "ngc7000-2"]);
    }

    #[tokio::test]
    async fn delete_present_returns_true_and_removes() {
        let (store, _dir) = open_temp().await;
        store.upsert_target(sample_target("ngc7000")).await.unwrap();
        let slug = TargetSlug::new("ngc7000").unwrap();
        assert!(store.delete_target(&slug).await.unwrap());
        assert_eq!(store.get_target(&slug).await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_absent_returns_false() {
        let (store, _dir) = open_temp().await;
        let slug = TargetSlug::new("ngc7000").unwrap();
        assert!(!store.delete_target(&slug).await.unwrap());
    }

    fn sample_stamp() -> WriteStamp {
        WriteStamp {
            updated_at: "2026-07-23T00:00:00Z".to_string(),
            updated_by: "operator".to_string(),
        }
    }

    #[tokio::test]
    async fn set_goals_on_absent_slug_is_not_found() {
        let (store, _dir) = open_temp().await;
        let slug = TargetSlug::new("ngc7000").unwrap();
        let err = store
            .set_goals(&slug, Vec::new(), sample_stamp())
            .await
            .unwrap_err();
        assert!(matches!(err, TargetStoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn set_goals_replaces_and_stamps_leaving_rest_of_row_untouched() {
        let (store, _dir) = open_temp().await;
        let target = sample_target("ngc7000");
        store.upsert_target(target.clone()).await.unwrap();

        let slug = TargetSlug::new("ngc7000").unwrap();
        let goals = vec![AcquisitionGoal {
            filter: "Ha".to_string(),
            binning: crate::Binning { x: 1, y: 1 },
            exposure_duration: std::time::Duration::from_mins(5),
            desired_count: 20,
        }];
        store
            .set_goals(&slug, goals.clone(), sample_stamp())
            .await
            .unwrap();

        let stored = store.get_target(&slug).await.unwrap().unwrap();
        assert_eq!(stored.goals, goals);
        assert_eq!(stored.display_name, target.display_name);
        assert_eq!(stored.updated_at, "2026-07-23T00:00:00Z");
        assert_eq!(stored.updated_by, "operator");
    }

    #[tokio::test]
    async fn upsert_preserves_created_by_and_created_at_of_existing_row() {
        let (store, _dir) = open_temp().await;
        let mut imported = sample_target("ngc7000");
        imported.created_by = "planetarium-bridge".to_string();
        imported.updated_by = "planetarium-bridge".to_string();
        store.upsert_target(imported).await.unwrap();

        // A later write claiming operator creation must not rewrite the
        // creation attribution — only the update attribution.
        let overwrite = sample_target("ngc7000");
        store.upsert_target(overwrite).await.unwrap();

        let slug = TargetSlug::new("ngc7000").unwrap();
        let stored = store.get_target(&slug).await.unwrap().unwrap();
        assert_eq!(stored.created_by, "planetarium-bridge");
        assert_eq!(stored.created_at, "2026-07-22T00:00:00Z");
        assert_eq!(stored.updated_by, "operator");
    }

    #[tokio::test]
    async fn reopen_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("targets.redb");
        {
            let store = RedbTargetStore::open(&db_path).await.unwrap();
            store.upsert_target(sample_target("ngc7000")).await.unwrap();
        }
        let store = RedbTargetStore::open(&db_path).await.unwrap();
        let slug = TargetSlug::new("ngc7000").unwrap();
        assert!(store.get_target(&slug).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn open_rejects_newer_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("targets.redb");

        // Write a database whose schema_version is newer than this build
        // supports, bypassing the normal open path.
        let db = Database::create(&db_path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let mut meta = write_txn.open_table(META_TABLE).unwrap();
            meta.insert(
                SCHEMA_VERSION_KEY,
                (CURRENT_SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
            )
            .unwrap();
        }
        write_txn.commit().unwrap();
        drop(db);

        let err = RedbTargetStore::open(&db_path).await.unwrap_err();
        assert!(matches!(
            err,
            TargetStoreError::UnsupportedSchemaVersion { .. }
        ));
    }
}

//! The flat-timing store (docs/services/calibrator-flats.md § Store).
//!
//! One redb file holding a [`FlatRecord`] per train and filter, with the
//! `rp-targets` conventions — a `meta` table carrying `schema_version`,
//! serde-tolerant record values, and a refusal to open a file written by
//! a newer build.
//!
//! The store is pure storage. Who writes (a converged `train_flats`
//! search, and nothing else) and what a stale record means for a caller
//! are the workflow's business; this module only knows how to compare a
//! record against the camera facts it was trained at.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

/// The schema version this build writes for a fresh store.
///
/// Additive record changes need no bump (new fields `#[serde(default)]`);
/// a breaking re-shape adds a migration step in [`open_and_init`] and
/// bumps this.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("flat_records");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Separates the train id from the filter name in a record key: the
/// ASCII unit separator, which no train id or filter name carries.
const KEY_SEPARATOR: char = '\u{1f}';

/// What a converged search learned for one train and filter, plus the
/// camera facts it is only valid at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatRecord {
    pub train_id: String,
    /// The wheel filter name; `None` for a filterless train, which
    /// stores under the train id alone.
    #[serde(default)]
    pub filter: Option<String>,
    /// The exposure that produced the 50 % flat.
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    /// The ladder level the search settled on; `take_flats` relights
    /// the panel here.
    pub brightness: u32,
    /// The median the search converged at.
    pub median_adu: u32,
    // --- the camera facts the record is only valid at ---
    pub max_adu: u32,
    pub bin_x: u32,
    pub bin_y: u32,
    #[serde(default)]
    pub gain: Option<i32>,
    #[serde(default)]
    pub offset: Option<i32>,
    pub camera_id: String,
    /// RFC 3339, UTC.
    pub trained_at: String,
}

/// What `rp` reports for a train's camera right now — the facts a
/// record is judged against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CameraFacts {
    pub camera_id: String,
    pub max_adu: u32,
    pub bin_x: u32,
    pub bin_y: u32,
    pub gain: Option<i32>,
    pub offset: Option<i32>,
}

/// One camera fact a record no longer matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleField {
    pub field: &'static str,
    pub recorded: String,
    pub current: String,
}

impl fmt::Display for StaleField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changed from {} to {}",
            self.field, self.recorded, self.current
        )
    }
}

fn fmt_optional(value: Option<i32>) -> String {
    value.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

impl FlatRecord {
    /// The store key for this record.
    #[must_use]
    pub fn key(&self) -> String {
        record_key(&self.train_id, self.filter.as_deref())
    }

    /// Every camera fact that differs between the record and `facts`,
    /// in a fixed order. Empty means the record is current.
    #[must_use]
    pub fn stale_fields(&self, facts: &CameraFacts) -> Vec<StaleField> {
        let mut stale = Vec::new();
        let mut check = |field: &'static str, recorded: String, current: String| {
            if recorded != current {
                stale.push(StaleField {
                    field,
                    recorded,
                    current,
                });
            }
        };
        check("camera_id", self.camera_id.clone(), facts.camera_id.clone());
        check(
            "max_adu",
            self.max_adu.to_string(),
            facts.max_adu.to_string(),
        );
        check("bin_x", self.bin_x.to_string(), facts.bin_x.to_string());
        check("bin_y", self.bin_y.to_string(), facts.bin_y.to_string());
        check("gain", fmt_optional(self.gain), fmt_optional(facts.gain));
        check(
            "offset",
            fmt_optional(self.offset),
            fmt_optional(facts.offset),
        );
        stale
    }
}

/// The store key for a train and filter: the train id, the separator,
/// and the filter name (empty for a filterless train).
#[must_use]
pub fn record_key(train_id: &str, filter: Option<&str>) -> String {
    format!("{train_id}{KEY_SEPARATOR}{}", filter.unwrap_or(""))
}

/// Errors from the store. See the `rp-targets` crate design for the
/// redb-generation vs schema-version distinction.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The parent directory of the store file could not be created.
    #[error("failed to create the store directory '{}': {source}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Failed to open or create the redb file. Never constructed for a
    /// redb-format generation bump — see [`Self::RedbUpgradeRequired`].
    #[error("failed to open the flat-timing store: {0}")]
    Open(redb::DatabaseError),
    #[error("failed to begin transaction: {0}")]
    Txn(#[from] redb::TransactionError),
    #[error("failed to open table: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("failed to commit transaction: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("failed to encode/decode a flat record: {0}")]
    Encode(#[from] serde_json::Error),
    /// The redb file-format generation on disk is older than this
    /// build's redb understands; run the documented one-time
    /// `redb::Database::upgrade()`.
    #[error(
        "the flat-timing store's file format requires a one-time redb upgrade (see docs/crates/rp-targets.md)"
    )]
    RedbUpgradeRequired,
    /// The on-disk `schema_version` is newer than this build supports.
    #[error("on-disk schema version {found} is newer than this build supports (max {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    /// The blocking task running a redb operation panicked or was
    /// cancelled.
    #[error("flat-timing store blocking task join error: {0}")]
    Join(String),
    /// A `meta` value was not shaped as this module writes it.
    #[error("the flat-timing store's meta table is corrupt: {0}")]
    Corrupt(String),
}

/// The store: one redb file, opened once at startup and shared behind an
/// `Arc`. Every operation runs its transaction on the Tokio blocking
/// pool.
#[derive(Debug, Clone)]
pub struct FlatStore {
    db: Arc<Database>,
}

impl FlatStore {
    /// Open (creating if absent) the store at `path`, creating its parent
    /// directory, initializing a fresh file's `schema_version` or
    /// checking an existing one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::CreateDir`] if the parent directory cannot
    /// be created, [`StoreError::RedbUpgradeRequired`] if the file was
    /// written by an older redb generation,
    /// [`StoreError::UnsupportedSchemaVersion`] if it was written by a
    /// newer build, or the relevant I/O variant otherwise.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let db = tokio::task::spawn_blocking(move || open_and_init(&path))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))??;
        Ok(Self { db: Arc::new(db) })
    }

    /// Write `record`, overwriting any record for the same train and
    /// filter.
    ///
    /// # Errors
    ///
    /// Returns the redb or encoding variant of [`StoreError`].
    pub async fn put(&self, record: FlatRecord) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || put_sync(&db, &record))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))?
    }

    /// The record for `train_id` and `filter`, if any.
    ///
    /// # Errors
    ///
    /// Returns the redb or decoding variant of [`StoreError`].
    pub async fn get(
        &self,
        train_id: &str,
        filter: Option<&str>,
    ) -> Result<Option<FlatRecord>, StoreError> {
        let db = Arc::clone(&self.db);
        let key = record_key(train_id, filter);
        tokio::task::spawn_blocking(move || get_sync(&db, &key))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))?
    }

    /// Every record for `train_id`, in key order (filterless first, then
    /// filters by name).
    ///
    /// # Errors
    ///
    /// Returns the redb or decoding variant of [`StoreError`].
    pub async fn list(&self, train_id: &str) -> Result<Vec<FlatRecord>, StoreError> {
        let db = Arc::clone(&self.db);
        let train_id = train_id.to_owned();
        tokio::task::spawn_blocking(move || list_sync(&db, &train_id))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))?
    }
}

fn open_and_init(path: &Path) -> Result<Database, StoreError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let db = Database::create(path).map_err(|e| match e {
        redb::DatabaseError::UpgradeRequired(_) => StoreError::RedbUpgradeRequired,
        other => StoreError::Open(other),
    })?;

    let write_txn = db.begin_write()?;
    {
        // Touch the records table so a fresh file always has both tables.
        write_txn.open_table(RECORDS_TABLE)?;

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
            Some(found) if found > CURRENT_SCHEMA_VERSION => {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
            Some(found) => {
                // found < CURRENT_SCHEMA_VERSION would run ordered
                // migration steps here; none exist yet.
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

fn decode_schema_version(bytes: &[u8]) -> Result<u32, StoreError> {
    let array: [u8; 4] = bytes.try_into().map_err(|_| {
        StoreError::Corrupt(format!(
            "schema_version value is {} bytes, expected 4",
            bytes.len()
        ))
    })?;
    Ok(u32::from_le_bytes(array))
}

fn put_sync(db: &Database, record: &FlatRecord) -> Result<(), StoreError> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(RECORDS_TABLE)?;
        let bytes = serde_json::to_vec(record)?;
        table.insert(record.key().as_str(), bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

fn get_sync(db: &Database, key: &str) -> Result<Option<FlatRecord>, StoreError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    match table.get(key)? {
        Some(value) => Ok(Some(serde_json::from_slice(value.value())?)),
        None => Ok(None),
    }
}

fn list_sync(db: &Database, train_id: &str) -> Result<Vec<FlatRecord>, StoreError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    let prefix = record_key(train_id, None);
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if key.value().starts_with(&prefix) {
            out.push(serde_json::from_slice(value.value())?);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn facts() -> CameraFacts {
        CameraFacts {
            camera_id: "main-cam".into(),
            max_adu: 65_535,
            bin_x: 1,
            bin_y: 1,
            gain: Some(100),
            offset: Some(10),
        }
    }

    fn record(train: &str, filter: Option<&str>) -> FlatRecord {
        FlatRecord {
            train_id: train.into(),
            filter: filter.map(str::to_owned),
            duration: Duration::from_millis(1200),
            brightness: 127,
            median_adu: 32_100,
            max_adu: 65_535,
            bin_x: 1,
            bin_y: 1,
            gain: Some(100),
            offset: Some(10),
            camera_id: "main-cam".into(),
            trained_at: "2026-09-05T19:02:11Z".into(),
        }
    }

    async fn open_temp() -> (FlatStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FlatStore::open(dir.path().join("calibrator-flats.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn open_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state").join("flats.redb");
        FlatStore::open(&path).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn get_absent_record_returns_none() {
        let (store, _dir) = open_temp().await;
        assert_eq!(store.get("main", Some("Luminance")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let (store, _dir) = open_temp().await;
        let record = record("main", Some("Luminance"));
        store.put(record.clone()).await.unwrap();
        assert_eq!(
            store.get("main", Some("Luminance")).await.unwrap(),
            Some(record)
        );
    }

    #[tokio::test]
    async fn a_put_for_the_same_train_and_filter_overwrites() {
        let (store, _dir) = open_temp().await;
        store.put(record("main", Some("Luminance"))).await.unwrap();
        let mut newer = record("main", Some("Luminance"));
        newer.duration = Duration::from_secs(3);
        newer.brightness = 63;
        store.put(newer.clone()).await.unwrap();

        assert_eq!(
            store.get("main", Some("Luminance")).await.unwrap(),
            Some(newer)
        );
        assert_eq!(store.list("main").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_filterless_train_stores_under_the_train_id_alone() {
        let (store, _dir) = open_temp().await;
        let osc = record("osc", None);
        store.put(osc.clone()).await.unwrap();
        assert_eq!(store.get("osc", None).await.unwrap(), Some(osc));
        assert!(store.get("osc", Some("")).await.unwrap().is_some());
        assert_eq!(record_key("osc", None), "osc\u{1f}");
        assert_eq!(record_key("osc", Some("L")), "osc\u{1f}L");
    }

    #[tokio::test]
    async fn list_returns_only_the_trains_records_in_key_order() {
        let (store, _dir) = open_temp().await;
        store.put(record("main", Some("Red"))).await.unwrap();
        store.put(record("main", Some("Blue"))).await.unwrap();
        store.put(record("main", None)).await.unwrap();
        store.put(record("guide", Some("Red"))).await.unwrap();
        // "main-2" shares a prefix with "main" but is a different train.
        store.put(record("main-2", Some("Red"))).await.unwrap();

        let filters: Vec<Option<String>> = store
            .list("main")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.filter)
            .collect();
        assert_eq!(filters, vec![None, Some("Blue".into()), Some("Red".into())]);
    }

    #[tokio::test]
    async fn reopen_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flats.redb");
        {
            let store = FlatStore::open(&path).await.unwrap();
            store.put(record("main", Some("Red"))).await.unwrap();
        }
        let store = FlatStore::open(&path).await.unwrap();
        assert!(store.get("main", Some("Red")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn open_rejects_a_newer_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flats.redb");
        let db = Database::create(&path).unwrap();
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

        let err = FlatStore::open(&path).await.unwrap_err();
        assert!(
            matches!(err, StoreError::UnsupportedSchemaVersion { found, .. } if found == CURRENT_SCHEMA_VERSION + 1),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_record_written_by_an_older_shape_still_reads() {
        // Serde tolerance: a stored value without the optional fields
        // deserializes with them defaulted.
        let (store, _dir) = open_temp().await;
        let db = Arc::clone(&store.db);
        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(RECORDS_TABLE).unwrap();
                let legacy = serde_json::json!({
                    "train_id": "main", "duration": "1s", "brightness": 255,
                    "median_adu": 30000, "max_adu": 65535, "bin_x": 1, "bin_y": 1,
                    "camera_id": "main-cam", "trained_at": "2026-01-01T00:00:00Z",
                    "future_field": true
                });
                table
                    .insert(
                        record_key("main", None).as_str(),
                        serde_json::to_vec(&legacy).unwrap().as_slice(),
                    )
                    .unwrap();
            }
            write_txn.commit().unwrap();
        })
        .await
        .unwrap();

        let stored = store.get("main", None).await.unwrap().unwrap();
        assert_eq!(stored.filter, None);
        assert_eq!(stored.gain, None);
        assert_eq!(stored.offset, None);
    }

    #[test]
    fn a_matching_record_has_no_stale_fields() {
        assert!(record("main", Some("L")).stale_fields(&facts()).is_empty());
    }

    #[test]
    fn every_camera_fact_is_judged_by_name() {
        let record = record("main", Some("L"));

        let mut changed = facts();
        changed.camera_id = "new-cam".into();
        let stale = record.stale_fields(&changed);
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].to_string(),
            "camera_id changed from main-cam to new-cam"
        );

        let mut changed = facts();
        changed.max_adu = 4095;
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "max_adu changed from 65535 to 4095"
        );

        let mut changed = facts();
        changed.bin_x = 2;
        changed.bin_y = 2;
        let stale: Vec<String> = record
            .stale_fields(&changed)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            stale,
            vec!["bin_x changed from 1 to 2", "bin_y changed from 1 to 2"]
        );

        let mut changed = facts();
        changed.gain = Some(120);
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "gain changed from 100 to 120"
        );

        let mut changed = facts();
        changed.offset = None;
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "offset changed from 10 to none"
        );
    }

    #[test]
    fn a_driver_without_gain_matches_a_record_without_gain() {
        let mut record = record("main", Some("L"));
        record.gain = None;
        record.offset = None;
        let mut facts = facts();
        facts.gain = None;
        facts.offset = None;
        assert!(record.stale_fields(&facts).is_empty());
    }
}

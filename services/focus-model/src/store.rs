//! The focus store (docs/services/focus-model.md § Store).
//!
//! One redb file holding a [`FocusRecord`] per optical train, with the
//! `rp-targets` conventions — a `meta` table carrying `schema_version`,
//! serde-tolerant record values, and a refusal to open a file written
//! by a newer build.
//!
//! The store is pure storage. Which tool writes what, and what a stale
//! record means for a caller, are the workflow's business; this module
//! knows how to hold a record, cap its history and compare it against
//! the train as `rp` reports it now.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::prediction::Prediction;
use crate::sizing::SweepSource;
use crate::sweep::{CurvePoint, SweepOutcome};

/// The schema version this build writes for a fresh store.
///
/// Additive record changes need no bump (new fields `#[serde(default)]`);
/// a breaking re-shape adds a migration step in [`open_and_init`] and
/// bumps this.
/// The samples one train's history may hold, across every run it
/// keeps. `runs_kept` caps the runs; this caps what they weigh, since
/// a coarse grid measured over several attempts is a hundred samples
/// a run rather than nine.
pub const MAX_STORED_CURVE_POINTS: usize = 100_000;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("focus_records");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// How a focus run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The confirmation frame vouched for the fitted vertex.
    Confirmed,
    /// The fit held but the confirmation did not; the focuser sits at
    /// the lowest measured sample.
    Fallback,
    /// Too few samples survived the sparse gate.
    NotEnoughStars,
    /// No minimum inside the sampled range.
    MonotonicCurve,
    /// The caller went away mid-sweep.
    Cancelled,
    /// A primitive failed; `error` carries the text.
    Error,
}

impl RunOutcome {
    /// Whether this run measured a position worth remembering as the
    /// filter's last good focus.
    #[must_use]
    pub const fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// The most recent confirmed focus on one filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastGood {
    /// The wheel filter; `None` on a filterless train.
    #[serde(default)]
    pub filter: Option<String>,
    pub position: i32,
    #[serde(default)]
    pub temperature_c: Option<f64>,
    pub hfr: f64,
    /// RFC 3339, UTC.
    pub at: String,
}

/// One focus run, successful or not: the whole measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusRun {
    /// RFC 3339, UTC.
    pub at: String,
    #[serde(default)]
    pub filter: Option<String>,
    pub outcome: RunOutcome,
    /// The failure text, for the `error` outcome.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub hfr: Option<f64>,
    #[serde(default)]
    pub best_position: Option<i32>,
    #[serde(default)]
    pub best_hfr: Option<f64>,
    #[serde(default)]
    pub fit_r_squared: Option<f64>,
    #[serde(default)]
    pub samples_used: Option<usize>,
    #[serde(default)]
    pub attempts: Option<u32>,
    #[serde(default)]
    pub wing_slope: Option<f64>,
    #[serde(default)]
    pub temperature_c: Option<f64>,
    pub step_size: i32,
    pub half_width: i32,
    pub sweep_source: SweepSource,
    #[serde(default)]
    pub prediction: Option<Prediction>,
    /// The sweep's samples, exactly as it measured them.
    #[serde(default)]
    pub curve_points: Vec<CurvePoint>,
}

/// A run without its samples: what `get_focus_model` reports for the
/// most recent one, the points themselves being `get_focus_runs`'
/// business (docs/services/focus-model.md § `get_focus_model`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunSummary {
    pub at: String,
    pub filter: Option<String>,
    pub outcome: RunOutcome,
    pub error: Option<String>,
    pub position: Option<i32>,
    pub hfr: Option<f64>,
    pub best_position: Option<i32>,
    pub best_hfr: Option<f64>,
    pub fit_r_squared: Option<f64>,
    pub samples_used: Option<usize>,
    pub attempts: Option<u32>,
    pub wing_slope: Option<f64>,
    pub temperature_c: Option<f64>,
    pub step_size: i32,
    pub half_width: i32,
    pub sweep_source: SweepSource,
    pub prediction: Option<Prediction>,
    /// How many samples the run measured. The samples are read with
    /// `get_focus_runs`.
    pub curve_points_recorded: usize,
}

impl From<&FocusRun> for RunSummary {
    fn from(run: &FocusRun) -> Self {
        Self {
            at: run.at.clone(),
            filter: run.filter.clone(),
            outcome: run.outcome,
            error: run.error.clone(),
            position: run.position,
            hfr: run.hfr,
            best_position: run.best_position,
            best_hfr: run.best_hfr,
            fit_r_squared: run.fit_r_squared,
            samples_used: run.samples_used,
            attempts: run.attempts,
            wing_slope: run.wing_slope,
            temperature_c: run.temperature_c,
            step_size: run.step_size,
            half_width: run.half_width,
            sweep_source: run.sweep_source,
            prediction: run.prediction.clone(),
            curve_points_recorded: run.curve_points.len(),
        }
    }
}

impl FocusRun {
    /// A run that has everything but its measurement: the shape a
    /// failure is recorded in, and the base a success fills out.
    #[must_use]
    pub const fn new(
        at: String,
        filter: Option<String>,
        outcome: RunOutcome,
        step_size: i32,
        half_width: i32,
        sweep_source: SweepSource,
    ) -> Self {
        Self {
            at,
            filter,
            outcome,
            error: None,
            position: None,
            hfr: None,
            best_position: None,
            best_hfr: None,
            fit_r_squared: None,
            samples_used: None,
            attempts: None,
            wing_slope: None,
            temperature_c: None,
            step_size,
            half_width,
            sweep_source,
            prediction: None,
            curve_points: Vec::new(),
        }
    }

    /// Fill in what a completed sweep measured.
    #[must_use]
    pub fn with_outcome(mut self, outcome: &SweepOutcome) -> Self {
        self.outcome = if outcome.confirmed {
            RunOutcome::Confirmed
        } else {
            RunOutcome::Fallback
        };
        self.position = Some(outcome.position);
        self.hfr = Some(outcome.hfr);
        self.best_position = Some(outcome.best_position);
        self.best_hfr = Some(outcome.best_hfr);
        self.fit_r_squared = Some(outcome.fit_r_squared);
        self.samples_used = Some(outcome.samples_used);
        self.attempts = Some(outcome.attempts);
        self.wing_slope = outcome.wing_slope;
        self.curve_points.clone_from(&outcome.curve_points);
        self
    }

    /// The last good entry this run earns, if any.
    #[must_use]
    pub fn last_good(&self) -> Option<LastGood> {
        if !self.outcome.is_confirmed() {
            return None;
        }
        Some(LastGood {
            filter: self.filter.clone(),
            position: self.position?,
            temperature_c: self.temperature_c,
            hfr: self.hfr?,
            at: self.at.clone(),
        })
    }
}

/// What `rp` reports for a train right now — the facts a record is
/// judged against.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TrainFacts {
    pub focuser_id: Option<String>,
    pub camera_id: Option<String>,
    /// The wheel's filter names in position order; `None` without a
    /// sole wheel.
    pub filters: Option<Vec<String>>,
}

/// One train fact a record no longer matches.
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

fn fmt_optional(value: Option<&String>) -> String {
    value.map_or_else(|| "none".to_owned(), Clone::clone)
}

pub(crate) fn fmt_filters(value: Option<&Vec<String>>) -> String {
    value.map_or_else(|| "none".to_owned(), |names| names.join(", "))
}

/// Whether two wheels hold different filters, order disregarded. A
/// wheel that reports names and one that reports none are different.
pub(crate) fn filter_sets_differ(
    recorded: Option<&Vec<String>>,
    current: Option<&Vec<String>>,
) -> bool {
    match (recorded, current) {
        (None, None) => false,
        (Some(recorded), Some(current)) => {
            let mut recorded: Vec<&String> = recorded.iter().collect();
            let mut current: Vec<&String> = current.iter().collect();
            recorded.sort_unstable();
            current.sort_unstable();
            recorded != current
        }
        _ => true,
    }
}

/// What one optical train's focus model knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusRecord {
    pub train_id: String,
    // --- the identity the record is only valid at ---
    #[serde(default)]
    pub focuser_id: Option<String>,
    #[serde(default)]
    pub camera_id: Option<String>,
    #[serde(default)]
    pub filters: Option<Vec<String>>,
    // --- what it learned ---
    #[serde(default)]
    pub reference_filter: Option<String>,
    #[serde(default)]
    pub offsets: BTreeMap<String, i32>,
    #[serde(default)]
    pub temperature_coefficient: Option<f64>,
    #[serde(default)]
    pub coefficient_runs: Option<usize>,
    #[serde(default)]
    pub coefficient_span_c: Option<f64>,
    /// One entry per filter, the most recent confirmed result on it.
    #[serde(default)]
    pub last_good: Vec<LastGood>,
    /// The most recent runs, newest last.
    #[serde(default)]
    pub runs: Vec<FocusRun>,
    /// RFC 3339, UTC.
    #[serde(default)]
    pub updated_at: String,
}

impl FocusRecord {
    /// A fresh record for a train, carrying the identity it is valid
    /// at and nothing else.
    #[must_use]
    pub fn new(
        train_id: &str,
        focuser_id: Option<&str>,
        camera_id: Option<&str>,
        filters: Option<Vec<String>>,
    ) -> Self {
        Self {
            train_id: train_id.to_owned(),
            focuser_id: focuser_id.map(str::to_owned),
            camera_id: camera_id.map(str::to_owned),
            filters,
            reference_filter: None,
            offsets: BTreeMap::new(),
            temperature_coefficient: None,
            coefficient_runs: None,
            coefficient_span_c: None,
            last_good: Vec::new(),
            runs: Vec::new(),
            updated_at: String::new(),
        }
    }

    /// The identity this record was written at.
    #[must_use]
    pub fn facts(&self) -> TrainFacts {
        TrainFacts {
            focuser_id: self.focuser_id.clone(),
            camera_id: self.camera_id.clone(),
            filters: self.filters.clone(),
        }
    }

    /// Every train fact that differs from `facts`, in a fixed order.
    /// Empty means the record still describes this train.
    ///
    /// The filters are judged as a set: the same names in another
    /// wheel order are the same optics, and each filter's offset and
    /// last good focus are keyed by name, not by position. The message
    /// keeps the order each side reported.
    #[must_use]
    pub fn stale_fields(&self, facts: &TrainFacts) -> Vec<StaleField> {
        let mut stale = Vec::new();
        let mut check = |field: &'static str, differs: bool, recorded: String, current: String| {
            if differs {
                stale.push(StaleField {
                    field,
                    recorded,
                    current,
                });
            }
        };
        check(
            "focuser_id",
            self.focuser_id != facts.focuser_id,
            fmt_optional(self.focuser_id.as_ref()),
            fmt_optional(facts.focuser_id.as_ref()),
        );
        check(
            "camera_id",
            self.camera_id != facts.camera_id,
            fmt_optional(self.camera_id.as_ref()),
            fmt_optional(facts.camera_id.as_ref()),
        );
        check(
            "filters",
            filter_sets_differ(self.filters.as_ref(), facts.filters.as_ref()),
            fmt_filters(self.filters.as_ref()),
            fmt_filters(facts.filters.as_ref()),
        );
        stale
    }

    /// The most recent confirmed focus, whatever its filter.
    #[must_use]
    pub fn most_recent_last_good(&self) -> Option<&LastGood> {
        // `at` is RFC 3339 UTC at second resolution, so the strings
        // sort chronologically.
        self.last_good.iter().max_by(|a, b| a.at.cmp(&b.at))
    }

    /// The last good focus on one filter.
    #[must_use]
    pub fn last_good_for(&self, filter: Option<&str>) -> Option<&LastGood> {
        self.last_good
            .iter()
            .find(|entry| entry.filter.as_deref() == filter)
    }

    /// One filter's offset from the reference; `None` for a filter the
    /// record has no offset for, and for a filterless slot.
    #[must_use]
    pub fn offset_for(&self, filter: Option<&str>) -> Option<i32> {
        self.offsets.get(filter?).copied()
    }

    /// Record a filter's last good focus, replacing its earlier entry.
    pub fn set_last_good(&mut self, entry: LastGood) {
        self.last_good.retain(|held| held.filter != entry.filter);
        self.last_good.push(entry);
    }

    /// Write the reference filter and the offsets, the reference at 0.
    pub fn set_offsets(&mut self, reference: Option<&str>, offsets: BTreeMap<String, i32>) {
        self.reference_filter = reference.map(str::to_owned);
        self.offsets = offsets;
        if let Some(reference) = reference {
            self.offsets.insert(reference.to_owned(), 0);
        }
    }

    /// Write the temperature model.
    pub const fn set_temperature_coefficient(
        &mut self,
        coefficient: Option<f64>,
        runs: usize,
        span_c: f64,
    ) {
        self.temperature_coefficient = coefficient;
        self.coefficient_runs = Some(runs);
        self.coefficient_span_c = Some(span_c);
    }

    /// Append a run, dropping the oldest once `runs_kept` is reached.
    pub fn push_run(&mut self, run: FocusRun, runs_kept: usize) {
        if let Some(entry) = run.last_good() {
            self.set_last_good(entry);
        }
        self.runs.push(run);
        let keep = runs_kept.max(1);
        if self.runs.len() > keep {
            let excess = self.runs.len().saturating_sub(keep);
            self.runs.drain(..excess);
        }
        self.trim_to_point_cap();
    }

    /// Drop the oldest runs until the samples they hold fit under
    /// [`MAX_STORED_CURVE_POINTS`].
    ///
    /// The run count alone does not bound the file: a coarse grid,
    /// several attempts and a long history multiply, and an
    /// unattended rig would find that out by filling its state
    /// volume. The newest run is always kept whole, however many
    /// points it measured.
    fn trim_to_point_cap(&mut self) {
        let mut held: usize = self
            .runs
            .iter()
            .map(|run| run.curve_points.len())
            .fold(0, usize::saturating_add);
        let mut dropped: usize = 0;
        while held > MAX_STORED_CURVE_POINTS && dropped.saturating_add(1) < self.runs.len() {
            let oldest = self
                .runs
                .get(dropped)
                .map_or(0, |run| run.curve_points.len());
            held = held.saturating_sub(oldest);
            dropped = dropped.saturating_add(1);
        }
        if dropped > 0 {
            tracing::debug!(
                train_id = %self.train_id,
                dropped,
                "the oldest runs were dropped to keep the record under the sample cap"
            );
            self.runs.drain(..dropped);
        }
    }

    /// The runs newest first, at most `limit`, optionally one filter's.
    #[must_use]
    pub fn recent_runs(&self, limit: usize, filter: Option<&str>) -> Vec<&FocusRun> {
        self.runs
            .iter()
            .rev()
            .filter(|run| filter.is_none_or(|name| run.filter.as_deref() == Some(name)))
            .take(limit)
            .collect()
    }

    /// How many runs the record holds, optionally on one filter.
    #[must_use]
    pub fn run_count(&self, filter: Option<&str>) -> usize {
        self.runs
            .iter()
            .filter(|run| filter.is_none_or(|name| run.filter.as_deref() == Some(name)))
            .count()
    }

    /// Forget what a re-homed focuser invalidated: the runs, the last
    /// good positions and the temperature model. The reference and the
    /// offsets stay — they are differences between filters.
    pub fn reset_measurements(&mut self) {
        self.runs.clear();
        self.last_good.clear();
        self.temperature_coefficient = None;
        self.coefficient_runs = None;
        self.coefficient_span_c = None;
    }
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
    #[error("failed to open the focus store: {0}")]
    Open(redb::DatabaseError),
    #[error("failed to begin transaction: {0}")]
    Txn(#[from] redb::TransactionError),
    #[error("failed to open table: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("failed to commit transaction: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("failed to encode/decode a focus record: {0}")]
    Encode(#[from] serde_json::Error),
    /// The redb file-format generation on disk is older than this
    /// build's redb understands; run the documented one-time
    /// `redb::Database::upgrade()`.
    #[error(
        "the focus store's file format requires a one-time redb upgrade (see docs/crates/rp-targets.md)"
    )]
    RedbUpgradeRequired,
    /// The on-disk `schema_version` is newer than this build supports.
    #[error("on-disk schema version {found} is newer than this build supports (max {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    /// The blocking task running a redb operation panicked or was
    /// cancelled.
    #[error("focus store blocking task join error: {0}")]
    Join(String),
    /// A `meta` value was not shaped as this module writes it.
    #[error("the focus store's meta table is corrupt: {0}")]
    Corrupt(String),
}

/// The store: one redb file, opened once at startup and shared behind
/// an `Arc`. Every operation runs its transaction on the Tokio blocking
/// pool.
#[derive(Debug, Clone)]
pub struct FocusStore {
    db: Arc<Database>,
    /// Held across the read and the write of an [`update`], so two
    /// calls cannot both load a record and write back over each other.
    ///
    /// [`update`]: FocusStore::update
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl FocusStore {
    /// Open (creating if absent) the store at `path`, creating its
    /// parent directory, initializing a fresh file's `schema_version`
    /// or checking an existing one.
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
        Ok(Self {
            db: Arc::new(db),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// The record for `train_id`, if any.
    ///
    /// # Errors
    ///
    /// Returns the redb or decoding variant of [`StoreError`].
    pub async fn get(&self, train_id: &str) -> Result<Option<FocusRecord>, StoreError> {
        let db = Arc::clone(&self.db);
        let train_id = train_id.to_owned();
        tokio::task::spawn_blocking(move || get_sync(&db, &train_id))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))?
    }

    /// Write `record`, overwriting any record for the same train and
    /// stamping `updated_at`.
    ///
    /// # Errors
    ///
    /// Returns the redb or encoding variant of [`StoreError`].
    pub async fn put(&self, mut record: FocusRecord) -> Result<FocusRecord, StoreError> {
        record.updated_at = now_rfc3339();
        let db = Arc::clone(&self.db);
        let stored = record.clone();
        tokio::task::spawn_blocking(move || put_sync(&db, &stored))
            .await
            .map_err(|e| StoreError::Join(e.to_string()))??;
        Ok(record)
    }

    /// Read the train's record, apply `change` to it and write the
    /// result back, all under the store's write lock.
    ///
    /// A focus run loads the record before its sweep and writes minutes
    /// later; between the two another call may have written the same
    /// train. `change` therefore receives the record as it stands at
    /// write time, not the one the caller read, and returns the record
    /// to store alongside whatever it wants reported.
    ///
    /// # Errors
    ///
    /// Returns `change`'s own error, and the redb or encoding variant
    /// of [`StoreError`].
    pub async fn update<T, E, F>(&self, train_id: &str, change: F) -> Result<(FocusRecord, T), E>
    where
        E: From<StoreError>,
        F: FnOnce(Option<FocusRecord>) -> Result<(FocusRecord, T), E> + Send,
    {
        let _guard = self.write_lock.lock().await;
        let held = self.get(train_id).await?;
        let (record, reported) = change(held)?;
        let written = self.put(record).await?;
        Ok((written, reported))
    }
}

/// The stamp every write carries: RFC 3339, UTC, seconds.
#[must_use]
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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
        // Touch the records table so a fresh file always has both.
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

fn put_sync(db: &Database, record: &FocusRecord) -> Result<(), StoreError> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(RECORDS_TABLE)?;
        let bytes = serde_json::to_vec(record)?;
        table.insert(record.train_id.as_str(), bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

fn get_sync(db: &Database, train_id: &str) -> Result<Option<FocusRecord>, StoreError> {
    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(RECORDS_TABLE)?;
    match table.get(train_id)? {
        Some(value) => Ok(Some(serde_json::from_slice(value.value())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn facts() -> TrainFacts {
        TrainFacts {
            focuser_id: Some("main-focuser".to_owned()),
            camera_id: Some("main-cam".to_owned()),
            filters: Some(vec!["L".to_owned(), "Ha".to_owned()]),
        }
    }

    fn record() -> FocusRecord {
        FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(vec!["L".to_owned(), "Ha".to_owned()]),
        )
    }

    fn run(at: &str, filter: Option<&str>, outcome: RunOutcome, position: i32) -> FocusRun {
        let mut run = FocusRun::new(
            at.to_owned(),
            filter.map(str::to_owned),
            outcome,
            17,
            68,
            SweepSource::Derived,
        );
        run.position = Some(position);
        run.hfr = Some(1.2);
        run.temperature_c = Some(11.0);
        run
    }

    async fn open_temp() -> (FocusStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus-model.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn open_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state").join("focus.redb");
        FocusStore::open(&path).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_train_without_a_record_reads_as_none() {
        let (store, _dir) = open_temp().await;
        assert_eq!(store.get("main").await.unwrap(), None);
    }

    #[tokio::test]
    async fn put_then_get_round_trips_and_stamps_the_write() {
        let (store, _dir) = open_temp().await;
        let stored = store.put(record()).await.unwrap();
        assert!(!stored.updated_at.is_empty());
        assert_eq!(store.get("main").await.unwrap(), Some(stored));
    }

    #[tokio::test]
    async fn a_second_put_overwrites_the_train() {
        let (store, _dir) = open_temp().await;
        store.put(record()).await.unwrap();
        let mut newer = record();
        newer.set_offsets(Some("L"), [("Ha".to_owned(), 46)].into());
        store.put(newer).await.unwrap();
        let stored = store.get("main").await.unwrap().unwrap();
        assert_eq!(stored.offset_for(Some("Ha")), Some(46));
        assert_eq!(stored.reference_filter.as_deref(), Some("L"));
    }

    #[tokio::test]
    async fn records_of_other_trains_are_untouched() {
        let (store, _dir) = open_temp().await;
        store.put(record()).await.unwrap();
        let mut guide = FocusRecord::new("guide", Some("guide-focuser"), None, None);
        guide.push_run(
            run("2026-09-10T22:00:00Z", None, RunOutcome::Confirmed, 500),
            5,
        );
        store.put(guide).await.unwrap();
        assert_eq!(store.get("main").await.unwrap().unwrap().runs.len(), 0);
        assert_eq!(store.get("guide").await.unwrap().unwrap().runs.len(), 1);
    }

    #[tokio::test]
    async fn reopen_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.redb");
        {
            let store = FocusStore::open(&path).await.unwrap();
            store.put(record()).await.unwrap();
        }
        let store = FocusStore::open(&path).await.unwrap();
        assert!(store.get("main").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn open_rejects_a_newer_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.redb");
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

        let err = FocusStore::open(&path).await.unwrap_err();
        assert!(
            matches!(err, StoreError::UnsupportedSchemaVersion { found, .. } if found == CURRENT_SCHEMA_VERSION + 1),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_record_written_by_an_older_shape_still_reads() {
        let (store, _dir) = open_temp().await;
        let db = Arc::clone(&store.db);
        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(RECORDS_TABLE).unwrap();
                let legacy = serde_json::json!({
                    "train_id": "main",
                    "focuser_id": "main-focuser",
                    "future_field": true
                });
                table
                    .insert("main", serde_json::to_vec(&legacy).unwrap().as_slice())
                    .unwrap();
            }
            write_txn.commit().unwrap();
        })
        .await
        .unwrap();

        let stored = store.get("main").await.unwrap().unwrap();
        assert_eq!(stored.camera_id, None);
        assert!(stored.runs.is_empty());
        assert!(stored.offsets.is_empty());
    }

    #[test]
    fn a_matching_record_has_no_stale_fields() {
        assert!(record().stale_fields(&facts()).is_empty());
    }

    #[test]
    fn every_train_fact_is_judged_by_name() {
        let record = record();

        let mut changed = facts();
        changed.camera_id = Some("new-cam".to_owned());
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "camera_id changed from main-cam to new-cam"
        );

        let mut changed = facts();
        changed.focuser_id = None;
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "focuser_id changed from main-focuser to none"
        );

        let mut changed = facts();
        changed.filters = Some(vec!["L".to_owned(), "Ha".to_owned(), "OIII".to_owned()]);
        assert_eq!(
            record.stale_fields(&changed)[0].to_string(),
            "filters changed from L, Ha to L, Ha, OIII"
        );
    }

    /// The offsets and the last good focus are keyed by filter name,
    /// so the same filters in another wheel order describe the same
    /// train and must not throw the model away.
    #[test]
    fn reordering_the_same_filters_is_not_stale() {
        let record = record();
        let mut reordered = facts();
        reordered.filters = Some(vec!["Ha".to_owned(), "L".to_owned()]);
        assert!(record.stale_fields(&reordered).is_empty());

        let mut gone = facts();
        gone.filters = None;
        assert_eq!(
            record.stale_fields(&gone)[0].to_string(),
            "filters changed from L, Ha to none"
        );
    }

    /// The record `update` hands the closure is the one in the store
    /// at write time, not whatever the caller read minutes earlier.
    #[tokio::test]
    async fn update_applies_to_the_record_as_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        let mut first = record();
        first.push_run(
            run("2026-09-10T22:00:00Z", Some("L"), RunOutcome::Confirmed, 1),
            50,
        );
        store.put(first).await.unwrap();

        let (written, reported) = store
            .update("main", |held| {
                let mut record = held.expect("the stored record");
                record.push_run(
                    run("2026-09-10T23:00:00Z", Some("L"), RunOutcome::Confirmed, 2),
                    50,
                );
                Ok::<_, StoreError>((record, "appended"))
            })
            .await
            .unwrap();
        assert_eq!(reported, "appended");
        assert_eq!(written.runs.len(), 2);
        assert_eq!(store.get("main").await.unwrap().unwrap().runs.len(), 2);
    }

    #[tokio::test]
    async fn update_writes_nothing_when_the_change_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        let refused: Result<(FocusRecord, ()), StoreError> = store
            .update("main", |held| {
                assert!(held.is_none(), "no record for the train");
                Err(StoreError::Join("no record".to_owned()))
            })
            .await;
        assert!(refused.is_err());
        assert!(store.get("main").await.unwrap().is_none());
    }

    /// The run count alone does not bound the file: a coarse grid over
    /// several attempts is a hundred samples a run, so the oldest runs
    /// go when their samples add up, newest kept whole.
    #[test]
    fn the_history_is_capped_by_its_samples_too() {
        let mut record = record();
        let bulk = MAX_STORED_CURVE_POINTS / 2;
        for i in 0..3 {
            let mut run = run(
                &format!("2026-09-1{i}T22:00:00Z"),
                Some("L"),
                RunOutcome::Fallback,
                29_000 + i,
            );
            run.curve_points = vec![
                CurvePoint {
                    position: 1,
                    hfr: Some(1.0),
                    star_count: 1,
                    document_id: String::new(),
                    rejected: None,
                };
                bulk
            ];
            record.push_run(run, 500);
        }
        assert_eq!(record.runs.len(), 2, "the oldest run's samples went");
        assert_eq!(record.runs[1].position, Some(29_002), "the newest stays");
    }

    #[test]
    fn the_history_is_capped_oldest_first() {
        let mut record = record();
        for i in 0..5 {
            record.push_run(
                run(
                    &format!("2026-09-1{i}T22:00:00Z"),
                    Some("L"),
                    RunOutcome::Fallback,
                    29_000 + i,
                ),
                3,
            );
        }
        assert_eq!(record.runs.len(), 3);
        assert_eq!(record.runs[0].position, Some(29_002));
        assert_eq!(record.runs[2].position, Some(29_004));
    }

    #[test]
    fn only_a_confirmed_run_updates_the_filters_last_good() {
        let mut record = record();
        record.push_run(
            run(
                "2026-09-10T20:00:00Z",
                Some("L"),
                RunOutcome::Fallback,
                29_700,
            ),
            50,
        );
        assert!(record.last_good.is_empty(), "{:?}", record.last_good);

        record.push_run(
            run(
                "2026-09-10T21:00:00Z",
                Some("L"),
                RunOutcome::Confirmed,
                29_766,
            ),
            50,
        );
        assert_eq!(record.last_good_for(Some("L")).unwrap().position, 29_766);

        // The next confirmed run on the same filter replaces it.
        record.push_run(
            run(
                "2026-09-10T22:00:00Z",
                Some("L"),
                RunOutcome::Confirmed,
                29_770,
            ),
            50,
        );
        assert_eq!(record.last_good.len(), 1);
        assert_eq!(record.last_good_for(Some("L")).unwrap().position, 29_770);
    }

    #[test]
    fn each_filter_keeps_its_own_last_good_and_the_newest_anchors() {
        let mut record = record();
        record.push_run(
            run(
                "2026-09-09T22:00:00Z",
                Some("L"),
                RunOutcome::Confirmed,
                29_766,
            ),
            50,
        );
        record.push_run(
            run(
                "2026-09-10T22:00:00Z",
                Some("Ha"),
                RunOutcome::Confirmed,
                29_812,
            ),
            50,
        );
        assert_eq!(record.last_good.len(), 2);
        assert_eq!(record.most_recent_last_good().unwrap().position, 29_812);
        assert_eq!(record.last_good_for(Some("L")).unwrap().position, 29_766);
    }

    #[test]
    fn a_failed_run_is_kept_with_its_outcome_and_nothing_measured() {
        let mut record = record();
        let mut failed = FocusRun::new(
            "2026-09-10T22:00:00Z".to_owned(),
            Some("L".to_owned()),
            RunOutcome::NotEnoughStars,
            17,
            68,
            SweepSource::Derived,
        );
        failed.error = Some("not enough stars: only 0 of 5".to_owned());
        record.push_run(failed, 50);
        assert_eq!(record.runs[0].outcome, RunOutcome::NotEnoughStars);
        assert_eq!(record.runs[0].position, None);
        assert!(record.last_good.is_empty());
    }

    #[test]
    fn the_runs_read_back_newest_first_and_filter_by_name() {
        let mut record = record();
        record.push_run(
            run("2026-09-08T22:00:00Z", Some("L"), RunOutcome::Confirmed, 1),
            50,
        );
        record.push_run(
            run("2026-09-09T22:00:00Z", Some("Ha"), RunOutcome::Confirmed, 2),
            50,
        );
        record.push_run(
            run("2026-09-10T22:00:00Z", Some("L"), RunOutcome::Fallback, 3),
            50,
        );

        let newest: Vec<i32> = record
            .recent_runs(2, None)
            .iter()
            .filter_map(|run| run.position)
            .collect();
        assert_eq!(newest, [3, 2]);
        let luminance: Vec<i32> = record
            .recent_runs(10, Some("L"))
            .iter()
            .filter_map(|run| run.position)
            .collect();
        assert_eq!(luminance, [3, 1]);
        assert_eq!(record.run_count(None), 3);
        assert_eq!(record.run_count(Some("Ha")), 1);
    }

    #[test]
    fn a_reset_keeps_the_offsets_and_drops_the_measurements() {
        let mut record = record();
        record.set_offsets(Some("L"), [("Ha".to_owned(), 46)].into());
        record.set_temperature_coefficient(Some(-7.4), 6, 4.5);
        record.push_run(
            run(
                "2026-09-10T22:00:00Z",
                Some("L"),
                RunOutcome::Confirmed,
                29_766,
            ),
            50,
        );

        record.reset_measurements();

        assert!(record.runs.is_empty());
        assert!(record.last_good.is_empty());
        assert_eq!(record.temperature_coefficient, None);
        assert_eq!(record.coefficient_runs, None);
        assert_eq!(record.reference_filter.as_deref(), Some("L"));
        assert_eq!(record.offset_for(Some("Ha")), Some(46));
        assert_eq!(record.offset_for(Some("L")), Some(0), "the reference is 0");
    }

    #[test]
    fn a_filterless_slot_has_no_offset() {
        let mut record = record();
        record.set_offsets(Some("L"), [("Ha".to_owned(), 46)].into());
        assert_eq!(record.offset_for(None), None);
        assert_eq!(record.offset_for(Some("OIII")), None);
    }
}

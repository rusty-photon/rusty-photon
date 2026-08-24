//! The workflow blackboard: `session.*` state plus atomic persistence.
//!
//! The blackboard is the workflow's only mutable state (design:
//! `docs/services/session-runner.md` § Blackboard and Persistence). It is a
//! JSON object persisted to `<state_dir>/<session_id>.json` with the
//! workspace atomic-write pattern (sibling temp file, fsync, rename, fsync
//! parent directory — mirroring `rp`'s exposure-document sidecars), and it
//! is persisted after **every** mutation: each `set` instruction, each
//! `once` completion marker, each trigger bookkeeping update. That
//! write-on-mutation invariant — the file always reflects every completed
//! `set` — is what makes re-derive resume sound.
//!
//! Engine bookkeeping lives under reserved keys documents cannot set:
//! `session._once.*` (completed once-markers) and `session._triggers.<id>.*`
//! (`last_fired` / `fired_once`). [`Blackboard::set_path`] rejects
//! `_`-prefixed roots as defense in depth — the document validator already
//! refuses such `set` keys at load.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// Root key of the completed-`once`-marker map (`session._once.*`).
const ONCE_KEY: &str = "_once";

/// Root key of the trigger-bookkeeping map (`session._triggers.<id>.*`).
const TRIGGERS_KEY: &str = "_triggers";

/// A blackboard I/O failure. Per the design's error table these fail loud:
/// continuing with unpersistable state would silently break resume.
#[derive(Debug, thiserror::Error)]
pub enum BlackboardError {
    #[error("blackboard read failed for {}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    #[error("blackboard file {} is corrupt: {message}", path.display())]
    Corrupt { path: PathBuf, message: String },
    #[error("blackboard write failed for {}: {message}", path.display())]
    Write { path: PathBuf, message: String },
}

/// An invalid in-memory `set` write (no I/O involved).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetPathError {
    /// The first path segment is `_`-prefixed. The document validator
    /// already rejects these; this guards the engine-internal API surface.
    #[error(
        "`{key}` writes reserved engine state — `session._*` keys \
         (`session._once`, `session._triggers`) cannot be set by a document"
    )]
    Reserved { key: String },
    /// An intermediate path segment exists but is not an object (and not
    /// `null` — missing or `null` intermediates are created as objects).
    #[error("cannot set `{key}`: `{ancestor}` is not an object")]
    NotAnObject { key: String, ancestor: String },
    /// The segment list was empty — the `session` root itself cannot be
    /// replaced. The document model guarantees at least one segment.
    #[error("cannot set the `session` root itself")]
    EmptyPath,
}

/// The `session.*` state for one workflow session, bound to its
/// persistence path.
#[derive(Debug)]
pub struct Blackboard {
    /// Always `Value::Object` — both constructors build one and every
    /// write path preserves it.
    session: Value,
    path: PathBuf,
}

impl Blackboard {
    /// An empty blackboard bound to `path`, with no I/O. Prefer
    /// [`Blackboard::replace`] for a new session — it also clears any
    /// leftover file.
    #[must_use]
    pub fn new_empty(path: PathBuf) -> Self {
        Self {
            session: Value::Object(Map::new()),
            path,
        }
    }

    /// A fresh blackboard for a new (non-recovery) session: any leftover
    /// file at `path` (an earlier session that never completed) is
    /// deleted **eagerly**, per the design's invocation rules.
    ///
    /// Lazy replacement on first persist would not be enough — a safety
    /// termination before the first write must not leave the stale file
    /// (stale `_once` markers included) to be mistaken for this session's
    /// state on the recovery invocation.
    ///
    /// # Errors
    ///
    /// Returns [`BlackboardError::Write`] if the leftover file exists
    /// but cannot be deleted; a missing file is the normal case, not an
    /// error.
    pub async fn replace(path: PathBuf) -> Result<Self, BlackboardError> {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(BlackboardError::Write {
                    path,
                    message: format!("cannot delete leftover blackboard: {e}"),
                })
            }
        }
        Ok(Self::new_empty(path))
    }

    /// Load the blackboard for a recovery invocation.
    ///
    /// A missing file is not an error — the session starts with an
    /// empty `session.*` (first-run equivalent), because a crash can
    /// predate the first `set`. A present-but-unparsable file is an
    /// error: silently discarding state would break resume. Async so
    /// the `/invoke` request path never does blocking file I/O on a
    /// runtime worker.
    ///
    /// # Errors
    ///
    /// Returns [`BlackboardError::Corrupt`] if the file parses to
    /// something other than a JSON object (or not at all), and
    /// [`BlackboardError::Read`] if reading fails for any reason other
    /// than the file being absent.
    pub async fn load(path: PathBuf) -> Result<Self, BlackboardError> {
        let session = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|e| BlackboardError::Corrupt {
                        path: path.clone(),
                        message: e.to_string(),
                    })?;
                if !value.is_object() {
                    return Err(BlackboardError::Corrupt {
                        path,
                        message: "top level is not a JSON object".to_owned(),
                    });
                }
                value
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(e) => {
                return Err(BlackboardError::Read {
                    path,
                    message: e.to_string(),
                })
            }
        };
        Ok(Self { session, path })
    }

    /// The full `session` object, for the expression evaluation context.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.session
    }

    /// The persistence path this blackboard is bound to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// In-memory write of one `set` entry: `segments` are the path after
    /// the `session` root (`["a", "b"]` for `session.a.b`).
    ///
    /// Missing or `null` intermediate segments are created as objects;
    /// an existing non-object intermediate is an error (the write would
    /// silently discard a scalar the document previously stored). Does
    /// **not** persist — the engine persists once per `set` instruction,
    /// after all of its entries are written.
    ///
    /// # Errors
    ///
    /// Returns [`SetPathError::Reserved`] for a `_`-prefixed root,
    /// [`SetPathError::EmptyPath`] for an empty segment list, and
    /// [`SetPathError::NotAnObject`] for a non-object intermediate.
    pub fn set_path(&mut self, segments: &[String], value: Value) -> Result<(), SetPathError> {
        let key = document_key(segments);
        if segments.first().is_some_and(|s| s.starts_with('_')) {
            return Err(SetPathError::Reserved { key });
        }
        let (last, parents) = segments.split_last().ok_or(SetPathError::EmptyPath)?;

        let mut walked = String::from("session");
        let mut cur = &mut self.session;
        for seg in parents {
            if cur.is_null() {
                *cur = Value::Object(Map::new());
            }
            cur = match cur {
                Value::Object(map) => map
                    .entry(seg.clone())
                    .or_insert_with(|| Value::Object(Map::new())),
                _ => {
                    return Err(SetPathError::NotAnObject {
                        key,
                        ancestor: walked,
                    })
                }
            };
            walked.push('.');
            walked.push_str(seg);
        }
        if cur.is_null() {
            *cur = Value::Object(Map::new());
        }
        match cur {
            Value::Object(map) => {
                map.insert(last.clone(), value);
                Ok(())
            }
            _ => Err(SetPathError::NotAnObject {
                key,
                ancestor: walked,
            }),
        }
    }

    /// Whether the `once` marker `key` has been recorded (the instruction
    /// completed in this or an earlier run of the session).
    pub fn once_done(&self, key: &str) -> bool {
        self.session
            .get(ONCE_KEY)
            .and_then(|m| m.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Record the `once` marker `key` under `session._once` and persist.
    ///
    /// Engine-owned bookkeeping heals rather than errors: a corrupt
    /// non-object `_once` value is replaced.
    ///
    /// # Errors
    ///
    /// Returns [`Blackboard::persist`]'s write failure — the in-memory
    /// marker update itself cannot fail.
    pub async fn mark_once(&mut self, key: &str) -> Result<(), BlackboardError> {
        if let Value::Object(root) = &mut self.session {
            let once = root
                .entry(ONCE_KEY)
                .or_insert_with(|| Value::Object(Map::new()));
            if !once.is_object() {
                *once = Value::Object(Map::new());
            }
            if let Value::Object(markers) = once {
                markers.insert(key.to_owned(), Value::Bool(true));
            }
        }
        self.persist().await
    }

    /// Whether trigger `id` has completed a firing with `once` recorded
    /// (in this or an earlier run of the session).
    pub fn trigger_fired_once(&self, id: &str) -> bool {
        self.trigger_entry(id)
            .and_then(|t| t.get("fired_once"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The wall-clock time trigger `id` last completed its action, for
    /// cooldown gating. `None` when it never fired or the recorded value
    /// is unreadable — engine-owned bookkeeping heals rather than errors,
    /// and treating a corrupt timestamp as "never fired" only shortens a
    /// cooldown once.
    #[must_use]
    pub fn trigger_last_fired(&self, id: &str) -> Option<DateTime<Utc>> {
        let recorded = self.trigger_entry(id)?.get("last_fired")?.as_str()?;
        DateTime::parse_from_rfc3339(recorded)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    fn trigger_entry(&self, id: &str) -> Option<&Value> {
        self.session.get(TRIGGERS_KEY)?.get(id)
    }

    /// Record that trigger `id`'s action completed at `at` — and, for a
    /// `once` trigger, that it must not fire again this session — then
    /// persist.
    ///
    /// Corrupt non-object bookkeeping values are replaced.
    ///
    /// # Errors
    ///
    /// Returns [`Blackboard::persist`]'s write failure — the in-memory
    /// bookkeeping update itself cannot fail.
    pub async fn mark_trigger_fired(
        &mut self,
        id: &str,
        at: DateTime<Utc>,
        once: bool,
    ) -> Result<(), BlackboardError> {
        if let Value::Object(root) = &mut self.session {
            let triggers = root
                .entry(TRIGGERS_KEY)
                .or_insert_with(|| Value::Object(Map::new()));
            if !triggers.is_object() {
                *triggers = Value::Object(Map::new());
            }
            if let Value::Object(by_id) = triggers {
                let entry = by_id
                    .entry(id.to_owned())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !entry.is_object() {
                    *entry = Value::Object(Map::new());
                }
                if let Value::Object(bookkeeping) = entry {
                    bookkeeping.insert("last_fired".to_owned(), Value::String(at.to_rfc3339()));
                    if once {
                        bookkeeping.insert("fired_once".to_owned(), Value::Bool(true));
                    }
                }
            }
        }
        self.persist().await
    }

    /// Atomically persist the session object: stage into a sibling temp
    /// file, fsync, rename into place, fsync the parent directory
    /// (unix-only).
    ///
    /// Runs on the blocking pool, one task per write.
    ///
    /// # Errors
    ///
    /// Returns [`BlackboardError::Write`] if serialization, any step of
    /// the atomic write, or the blocking-task join fails.
    pub async fn persist(&self) -> Result<(), BlackboardError> {
        let body =
            serde_json::to_vec_pretty(&self.session).map_err(|e| BlackboardError::Write {
                path: self.path.clone(),
                message: format!("serialization failed: {e}"),
            })?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&path, &body))
            .await
            .map_err(|e| BlackboardError::Write {
                path: self.path.clone(),
                message: format!("write task join error: {e}"),
            })?
    }
}

/// The document-form key (`session.a.b`) for logs and errors.
fn document_key(segments: &[String]) -> String {
    let mut key = String::from("session");
    for seg in segments {
        key.push('.');
        key.push_str(seg);
    }
    key
}

/// The workspace atomic-write pattern, as in `rp`'s
/// `persistence::document::write_sidecar_sync`.
fn write_atomic(final_path: &Path, body: &[u8]) -> Result<(), BlackboardError> {
    let write_err = |message: String| BlackboardError::Write {
        path: final_path.to_path_buf(),
        message,
    };
    let parent = final_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| write_err("path has no parent directory".to_owned()))?;
    std::fs::create_dir_all(parent).map_err(|e| write_err(e.to_string()))?;

    // `NamedTempFile::new_in(parent)` gives an OS-generated unique name (so
    // concurrent writers cannot collide on the staging path) and a `Drop`
    // guard that removes the staging file on early return; `persist`
    // disarms the guard on success.
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| write_err(e.to_string()))?;
    tmp.write_all(body).map_err(|e| write_err(e.to_string()))?;
    // fsync the file data so a crash after rename cannot surface a
    // renamed-but-empty blackboard.
    tmp.as_file()
        .sync_all()
        .map_err(|e| write_err(e.to_string()))?;
    tmp.persist(final_path)
        .map_err(|e| write_err(e.error.to_string()))?;
    // fsync the parent directory so the rename itself is durable. Windows
    // cannot open a directory as a regular file handle, so unix-only.
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| write_err(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    fn segs(path: &[&str]) -> Vec<String> {
        path.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    async fn test_load_missing_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let bb = Blackboard::load(dir.path().join("nope.json"))
            .await
            .unwrap();
        assert_eq!(*bb.value(), json!({}));
    }

    #[tokio::test]
    async fn test_load_corrupt_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = Blackboard::load(path).await.unwrap_err();
        assert!(matches!(err, BlackboardError::Corrupt { .. }), "{err}");
    }

    #[tokio::test]
    async fn test_load_non_object_top_level_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, b"[1, 2]").unwrap();
        let err = Blackboard::load(path).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "blackboard file {} is corrupt: top level is not a JSON object",
                dir.path().join("s.json").display()
            )
        );
    }

    #[test]
    fn test_set_path_writes_scalars_and_nested_paths() {
        let mut bb = Blackboard::new_empty(PathBuf::from("unused/s.json"));
        bb.set_path(&segs(&["duration"]), json!(2.5)).unwrap();
        bb.set_path(&segs(&["report", "frames"]), json!(30))
            .unwrap();
        assert_eq!(
            *bb.value(),
            json!({"duration": 2.5, "report": {"frames": 30}})
        );
    }

    #[test]
    fn test_set_path_overwrites_and_creates_through_null() {
        let mut bb = Blackboard::new_empty(PathBuf::from("unused/s.json"));
        bb.set_path(&segs(&["a"]), Value::Null).unwrap();
        // A null intermediate is treated as absent (matching `has()`'s
        // view) and becomes an object.
        bb.set_path(&segs(&["a", "b"]), json!(1)).unwrap();
        assert_eq!(*bb.value(), json!({"a": {"b": 1}}));
        bb.set_path(&segs(&["a"]), json!("replaced")).unwrap();
        assert_eq!(*bb.value(), json!({"a": "replaced"}));
    }

    #[test]
    fn test_set_path_through_non_object_intermediate_is_an_error() {
        let mut bb = Blackboard::new_empty(PathBuf::from("unused/s.json"));
        bb.set_path(&segs(&["a"]), json!(5)).unwrap();
        let err = bb.set_path(&segs(&["a", "b", "c"]), json!(1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot set `session.a.b.c`: `session.a` is not an object"
        );
        // The final segment's parent gets the same check.
        let err = bb.set_path(&segs(&["a", "b"]), json!(1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot set `session.a.b`: `session.a` is not an object"
        );
    }

    #[test]
    fn test_set_path_rejects_reserved_roots() {
        let mut bb = Blackboard::new_empty(PathBuf::from("unused/s.json"));
        let err = bb
            .set_path(&segs(&["_once", "k"]), json!(true))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "`session._once.k` writes reserved engine state — `session._*` keys \
             (`session._once`, `session._triggers`) cannot be set by a document"
        );
    }

    #[test]
    fn test_set_path_rejects_the_empty_path() {
        let mut bb = Blackboard::new_empty(PathBuf::from("unused/s.json"));
        assert_eq!(bb.set_path(&[], json!(1)), Err(SetPathError::EmptyPath));
    }

    #[tokio::test]
    async fn test_persist_and_reload_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-1.json");
        let mut bb = Blackboard::new_empty(path.clone());
        bb.set_path(&segs(&["target_adu"]), json!(32767.5)).unwrap();
        bb.persist().await.unwrap();

        let reloaded = Blackboard::load(path).await.unwrap();
        assert_eq!(reloaded.value(), bb.value());
    }

    #[tokio::test]
    async fn test_persist_leaves_no_staging_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let bb = Blackboard::new_empty(path);
        bb.persist().await.unwrap();
        bb.persist().await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("s.json")]);
    }

    #[tokio::test]
    async fn test_persist_creates_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("s.json");
        let bb = Blackboard::new_empty(path.clone());
        bb.persist().await.unwrap();
        assert!(path.is_file());
    }

    #[tokio::test]
    async fn test_persist_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        // Make the rename fail: the destination is a directory.
        let path = dir.path().join("s.json");
        std::fs::create_dir(&path).unwrap();
        let bb = Blackboard::new_empty(path);
        let err = bb.persist().await.unwrap_err();
        assert!(matches!(err, BlackboardError::Write { .. }), "{err}");
    }

    #[tokio::test]
    async fn test_replace_deletes_a_leftover_file_eagerly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-1.json");
        // A stale file from an earlier incarnation of the same session id
        // — with a once-marker that would wrongly skip work if it ever
        // resurfaced on a recovery invocation.
        std::fs::write(&path, br#"{"_once": {"panel-on": true}, "stale": 1}"#).unwrap();

        let bb = Blackboard::replace(path.clone()).await.unwrap();
        assert_eq!(*bb.value(), json!({}));
        assert!(
            !path.exists(),
            "the leftover file must be gone even if this session never persists"
        );
        // Reloading (what a recovery invocation does) now sees a fresh
        // session, not the stale state.
        assert_eq!(*Blackboard::load(path).await.unwrap().value(), json!({}));
    }

    #[tokio::test]
    async fn test_replace_without_a_leftover_file_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let bb = Blackboard::replace(dir.path().join("nope.json"))
            .await
            .unwrap();
        assert_eq!(*bb.value(), json!({}));
    }

    #[tokio::test]
    async fn test_once_markers_record_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut bb = Blackboard::new_empty(path.clone());
        assert!(!bb.once_done("panel-on"));
        bb.mark_once("panel-on").await.unwrap();
        assert!(bb.once_done("panel-on"));

        let reloaded = Blackboard::load(path).await.unwrap();
        assert!(reloaded.once_done("panel-on"));
        assert_eq!(*reloaded.value(), json!({"_once": {"panel-on": true}}));
    }

    #[tokio::test]
    async fn test_once_done_requires_a_true_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, br#"{"_once": {"a": false, "b": 1}, "c": 2}"#).unwrap();
        let bb = Blackboard::load(path).await.unwrap();
        assert!(!bb.once_done("a"));
        assert!(!bb.once_done("b"));
        assert!(!bb.once_done("missing"));
    }

    #[tokio::test]
    async fn test_trigger_bookkeeping_records_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut bb = Blackboard::new_empty(path.clone());
        assert!(!bb.trigger_fired_once("refocus"));
        assert_eq!(bb.trigger_last_fired("refocus"), None);

        let at = chrono::Utc::now();
        bb.mark_trigger_fired("refocus", at, true).await.unwrap();
        assert!(bb.trigger_fired_once("refocus"));
        assert_eq!(bb.trigger_last_fired("refocus"), Some(at));

        let reloaded = Blackboard::load(path).await.unwrap();
        assert!(reloaded.trigger_fired_once("refocus"));
        assert_eq!(reloaded.trigger_last_fired("refocus"), Some(at));
    }

    #[tokio::test]
    async fn test_mark_trigger_fired_without_once_leaves_no_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut bb = Blackboard::new_empty(dir.path().join("s.json"));
        let at = chrono::Utc::now();
        bb.mark_trigger_fired("watch", at, false).await.unwrap();
        assert!(!bb.trigger_fired_once("watch"));
        assert_eq!(bb.trigger_last_fired("watch"), Some(at));
        assert_eq!(
            *bb.value(),
            json!({"_triggers": {"watch": {"last_fired": at.to_rfc3339()}}})
        );
    }

    #[tokio::test]
    async fn test_trigger_bookkeeping_heals_corrupt_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(
            &path,
            br#"{"_triggers": {"a": 5, "b": {"last_fired": "not a timestamp"}}}"#,
        )
        .unwrap();
        let mut bb = Blackboard::load(path).await.unwrap();
        // Unreadable bookkeeping reads as "never fired"…
        assert!(!bb.trigger_fired_once("a"));
        assert_eq!(bb.trigger_last_fired("a"), None);
        assert_eq!(bb.trigger_last_fired("b"), None);
        // …and is replaced on the next write.
        let at = chrono::Utc::now();
        bb.mark_trigger_fired("a", at, true).await.unwrap();
        assert!(bb.trigger_fired_once("a"));
        assert_eq!(bb.trigger_last_fired("a"), Some(at));
    }

    #[tokio::test]
    async fn test_mark_once_heals_a_corrupt_marker_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, br#"{"_once": 5}"#).unwrap();
        let mut bb = Blackboard::load(path).await.unwrap();
        assert!(!bb.once_done("k"));
        bb.mark_once("k").await.unwrap();
        assert!(bb.once_done("k"));
        assert_eq!(*bb.value(), json!({"_once": {"k": true}}));
    }
}

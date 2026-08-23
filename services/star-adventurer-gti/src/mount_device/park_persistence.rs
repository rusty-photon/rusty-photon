//! On-disk persistence for `SetPark` — read/write the park-encoder
//! pair to the operator's JSON config file, plus the startup helpers
//! `main.rs` calls to canonicalise the path and probe writability
//! before the first `SetPark` request lands.
//!
//! Read-as-`serde_json::Value` + atomic-rename pattern: only the two
//! `mount.park_*_ticks` keys are touched, every other JSON value is
//! preserved verbatim. See the design doc's
//! [§"Park persistence"](../../../../docs/services/star-adventurer-gti.md#park-persistence)
//! for the contract.

use std::path::{Path, PathBuf};

use crate::config::ApPark;
use crate::error::StarAdvError;

/// Probe whether the parent directory of `config_path` can host the
/// staging temp file that `SetPark`'s atomic-rename pattern requires.
///
/// Called once at startup from `main.rs` so the operator sees a `warn!`
/// at boot if `SetPark` will fail at runtime due to filesystem
/// permissions, rather than only discovering it on the first `SetPark`
/// call. Does **not** change `CanSetPark` — the capability still
/// advertises support; the probe is purely an early-warning signal.
///
/// The probe creates a `NamedTempFile` in the same directory the real
/// staging file would live in (`config_path.parent()`) and immediately
/// drops it. Writability of the **parent directory** is what matters
/// for the atomic-rename pattern: even if the target config file is
/// itself read-only, `rename(2)` only needs write access to the
/// containing directory to swap in a new file. The probe covers only
/// that first step of [`write_park_to_config`] — the real write can
/// still fail later while writing, syncing, or renaming, so a clean
/// probe is an early warning, not a guarantee.
///
/// # Errors
///
/// Returns the I/O error if a temporary file cannot be created in the
/// config file's directory — the first failure the real staging write
/// would hit.
pub fn probe_park_file_writability(config_path: &Path) -> std::io::Result<()> {
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Drop closes and deletes the temp file; the probe leaves no trace.
    let _tmp = tempfile::NamedTempFile::new_in(parent)?;
    Ok(())
}

/// Canonicalise the operator-supplied config path so `SetPark` writes
/// to a stable absolute location even if the process later `chdir`s
/// away.
///
/// Also resolves symlinks, which the atomic-rename pattern needs — the
/// temp file goes in the *physical* parent directory. On canonicalisation
/// failure (path doesn't yet exist, symlink loop, permission denied on a
/// path component) the original path is returned and a `warn!` is logged
/// — `SetPark` will still attempt the write against the path as given,
/// surfacing the real error there.
///
/// Extracted from `main.rs` so the warn-on-failure branch is unit
/// testable; the binary calls this from `main()`.
#[must_use]
pub fn canonicalise_config_path(config_path: &Path) -> PathBuf {
    std::fs::canonicalize(config_path).unwrap_or_else(|e| {
        tracing::warn!(
            "could not canonicalise config path {:?}: {e}; SetPark will write to the path as given",
            config_path
        );
        config_path.to_path_buf()
    })
}

/// Early-warning probe wrapper: run [`probe_park_file_writability`] on
/// the supplied path and log a `warn!` on failure.
///
/// Used by `main.rs` at startup — operators get a heads-up at boot if
/// `SetPark` will fail at runtime due to filesystem permissions, rather
/// than only discovering it on the first `SetPark` call. `CanSetPark` is
/// not affected; the capability still advertises support and the actual
/// `SetPark` will surface a structured error if the probe was correct.
///
/// Extracted from `main.rs` so the warn-on-failure branch is unit
/// testable.
pub fn warn_if_park_path_unwritable(config_path: &Path) {
    if let Err(e) = probe_park_file_writability(config_path) {
        tracing::warn!(
            "SetPark writes to {:?} will fail at runtime: {e}. \
             Check permissions on the containing directory if SetPark support is required.",
            config_path
        );
    }
}

/// Decode an optional park-tick JSON value:
///
/// - Absent (`None`) or explicit `Value::Null` → `Ok(None)` (caller
///   falls back to the handshake-captured value).
/// - A JSON integer in the signed-24-bit encoder range
///   `[POSITION_MIN, POSITION_MAX]` → `Ok(Some(n))`.
/// - Anything else (string, float, boolean, array/object, integer
///   outside the i24 encoder range) → `Err(StarAdvError::Config)`.
///   Loud failure on operator typo is the whole reason this helper
///   exists — silently falling back to handshake would mask the
///   misconfiguration, and silently accepting an out-of-range value
///   would only defer the failure to `MountManager::send`'s
///   pre-encode validation at the first park attempt.
fn extract_park_tick(
    value: Option<&serde_json::Value>,
    key: &'static str,
) -> crate::error::Result<Option<i32>> {
    use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_i64().ok_or_else(|| {
                StarAdvError::Config(format!(
                    "`{key}` must be an integer (encoder ticks), got {v}"
                ))
            })?;
            let ticks = i32::try_from(n).map_err(|_| {
                StarAdvError::Config(format!(
                    "`{key}` value {n} is outside the i32 encoder-tick range"
                ))
            })?;
            if !(POSITION_MIN..=POSITION_MAX).contains(&ticks) {
                return Err(StarAdvError::Config(format!(
                    "`{key}` value {ticks} is outside the signed-24-bit encoder \
                     range [{POSITION_MIN}, {POSITION_MAX}]"
                )));
            }
            Ok(Some(ticks))
        }
    }
}

/// The connect-time `mount.*` fields the driver re-reads from disk on
/// every connect, read in a **single** parse. A `None` means the key was
/// absent or JSON `null` on disk — the caller applies the in-memory
/// startup fallback for that field.
#[derive(Debug)]
pub(super) struct MountConnectFields {
    pub park_ra_ticks: Option<i32>,
    pub park_dec_ticks: Option<i32>,
    pub unpark_from_ap_position: Option<ApPark>,
    pub preferred_ap_park: Option<ApPark>,
}

/// Read the four connect-time `mount.*` fields (`park_ra_ticks`,
/// `park_dec_ticks`, `unpark_from_ap_position`, `preferred_ap_park`)
/// from the on-disk config in one read + parse, so `set_connected`'s
/// seed + park-target hooks don't each re-read the file. A
/// `SetUnparkFromApPosition` / `SetPreferredApPark` / `SetPark` write —
/// or an operator hand-edit between connects — therefore takes effect on
/// the next connect without a driver restart.
///
/// Per-field decoding matches the single-field helpers: tick keys go
/// through [`extract_park_tick`] (loud on a non-integer, or an integer
/// outside the `i32` / signed-24-bit encoder range), AP-park keys
/// through [`extract_ap_park`] (loud on an unrecognised string). A
/// missing/malformed file or a missing `mount` object is a
/// `StarAdvError::Config`. The `ap_park_0` rejection that
/// `preferred_ap_park` carries at full-config deserialize is **not**
/// re-applied here — the caller treats an `ap_park_0` value as "no AP
/// target" and falls back accordingly.
///
/// Blocking I/O; callers wrap in `tokio::task::spawn_blocking`.
pub(super) fn read_connect_fields(config_path: &Path) -> crate::error::Result<MountConnectFields> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| StarAdvError::Config(format!("read config {}: {e}", config_path.display())))?;
    let root: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        StarAdvError::Config(format!("parse config {}: {e}", config_path.display()))
    })?;
    let mount = root
        .as_object()
        .and_then(|o| o.get("mount"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            StarAdvError::Config(format!(
                "config {} has no `mount` object",
                config_path.display()
            ))
        })?;
    Ok(MountConnectFields {
        park_ra_ticks: extract_park_tick(mount.get("park_ra_ticks"), "mount.park_ra_ticks")?,
        park_dec_ticks: extract_park_tick(mount.get("park_dec_ticks"), "mount.park_dec_ticks")?,
        unpark_from_ap_position: extract_ap_park(
            mount.get("unpark_from_ap_position"),
            "unpark_from_ap_position",
        )?,
        preferred_ap_park: extract_ap_park(mount.get("preferred_ap_park"), "preferred_ap_park")?,
    })
}

/// Decode an optional `mount.<key>` AP-park value: absent / `null` →
/// `Ok(None)`; a recognised `ap_park_N` string → `Ok(Some(park))`;
/// anything else → `Err(StarAdvError::Config)`. Loud failure on an
/// operator typo mirrors [`extract_park_tick`].
fn extract_ap_park(
    value: Option<&serde_json::Value>,
    key: &str,
) -> crate::error::Result<Option<ApPark>> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            let park: ApPark = serde_json::from_value(v.clone()).map_err(|e| {
                StarAdvError::Config(format!("`mount.{key}` is not a valid AP park: {e}"))
            })?;
            Ok(Some(park))
        }
    }
}

/// Patch the on-disk JSON config with the supplied park encoder pair.
///
/// Read-as-`Value` + atomic-rename pattern: load the file as
/// `serde_json::Value`, mutate **only** the `mount.park_ra_ticks` and
/// `mount.park_dec_ticks` keys, serialise pretty-printed, write via a
/// `tempfile::NamedTempFile` in the same directory, `persist` to swap
/// it in atomically. Every other field of the JSON file — known and
/// unknown — is preserved as a JSON value. Operator-level formatting
/// (insertion-order of unrelated keys, custom indentation, comments
/// disguised as fields) is not preserved byte-for-byte because the
/// round-trip pretty-prints the whole document; the *semantic* content
/// outside the two park keys is unchanged.
///
/// Durability: fsync the staged file before rename (`tempfile::persist`
/// uses POSIX `rename(2)`), then fsync the parent directory after
/// rename so the directory entry update is itself durable. Mirrors
/// `services/rp/src/persistence/document.rs::write_sidecar_sync`.
///
/// The driver never re-serialises its in-memory typed `Config` here:
/// doing so would round-trip CLI overrides (`--port`, `--baud`, etc.)
/// back to disk and is structurally avoided. See the design doc's
/// [§"Park persistence"](../../../../docs/services/star-adventurer-gti.md#park-persistence)
/// for the contract this helper implements.
///
/// Blocking I/O; callers wrap in `tokio::task::spawn_blocking`.
pub(super) fn write_park_to_config(
    config_path: &Path,
    park_ra_ticks: i32,
    park_dec_ticks: i32,
) -> crate::error::Result<()> {
    write_mount_fields_to_config(
        config_path,
        &[
            ("park_ra_ticks", serde_json::Value::from(park_ra_ticks)),
            ("park_dec_ticks", serde_json::Value::from(park_dec_ticks)),
        ],
    )
}

/// Patch the on-disk JSON config, setting each `mount.<key>` in
/// `updates` to its paired value. The read-modify-write engine behind
/// [`write_park_to_config`] and the `SetUnparkFromApPosition` /
/// `SetPreferredApPark` Actions.
///
/// Read-as-`Value` + atomic-rename pattern: load the file as
/// `serde_json::Value`, mutate **only** the listed `mount` keys,
/// serialise pretty-printed, write via a `tempfile::NamedTempFile` in
/// the same directory, `persist` to swap it in atomically. Every other
/// field — known and unknown — is preserved as a JSON value. Operator
/// formatting (key order, indentation) is not preserved byte-for-byte
/// because the round-trip pretty-prints the whole document; the
/// *semantic* content outside the listed keys is unchanged.
///
/// Durability: fsync the staged file before rename (`tempfile::persist`
/// uses POSIX `rename(2)`), then fsync the parent directory after
/// rename so the directory entry update is itself durable. Mirrors
/// `services/rp/src/persistence/document.rs::write_sidecar_sync`.
///
/// The driver never re-serialises its in-memory typed `Config` here:
/// doing so would round-trip CLI overrides (`--port`, `--baud`, etc.)
/// back to disk and is structurally avoided. See the design doc's
/// [§"Park persistence"](../../../../docs/services/star-adventurer-gti.md#park-persistence)
/// for the contract this helper implements.
///
/// Blocking I/O; callers wrap in `tokio::task::spawn_blocking`.
pub(super) fn write_mount_fields_to_config(
    config_path: &Path,
    updates: &[(&str, serde_json::Value)],
) -> crate::error::Result<()> {
    use std::io::Write;

    let content = std::fs::read_to_string(config_path)
        .map_err(|e| StarAdvError::Config(format!("read config {}: {e}", config_path.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        StarAdvError::Config(format!("parse config {}: {e}", config_path.display()))
    })?;
    let mount = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mount"))
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            StarAdvError::Config(format!(
                "config {} has no `mount` object",
                config_path.display()
            ))
        })?;
    for (key, value) in updates {
        mount.insert((*key).to_string(), value.clone());
    }
    let mut pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| StarAdvError::Config(format!("serialise config: {e}")))?;
    // serde_json's pretty-printer omits a trailing newline; add one so
    // operators editing the file later don't trip POSIX "no newline at
    // end of file" warnings in diffs.
    pretty.push('\n');

    // Temp file must live in the **same directory** as the destination
    // so `persist` can use POSIX `rename` (atomic on the same
    // filesystem) rather than copy-and-delete. Fall back to the
    // current dir if the path has no parent (e.g. a bare filename),
    // which is what Path::parent returns Some("") for — coerce to ".".
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        StarAdvError::Config(format!("create temp file in {}: {e}", parent.display()))
    })?;
    tmp.write_all(pretty.as_bytes())
        .map_err(|e| StarAdvError::Config(format!("write temp file: {e}")))?;
    // fsync the file data so a crash after rename cannot surface a
    // renamed-but-zero-length sidecar.
    tmp.as_file()
        .sync_all()
        .map_err(|e| StarAdvError::Config(format!("fsync temp file: {e}")))?;
    tmp.persist(config_path).map_err(|e| {
        StarAdvError::Config(format!("atomic rename to {}: {e}", config_path.display()))
    })?;
    // fsync the parent directory so the rename itself is durable.
    // Windows can't open a directory as a regular file handle, so this
    // is unix-only. Mirrors `services/rp/src/persistence/document.rs`.
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| {
                StarAdvError::Config(format!("fsync parent dir {}: {e}", parent.display()))
            })?;
    }
    Ok(())
}

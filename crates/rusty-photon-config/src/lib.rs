//! Shared config helpers for rusty-photon drivers.
//!
//! ASCOM Alpaca requires every device's `UniqueID` to be **globally unique** and to **never
//! change**, but the protocol enforces neither — uniqueness has to come from how the id is
//! generated. This crate gives each driver a spec-compliant identity: it resolves a platform
//! config path (per-user on Unix, machine-wide `%PROGRAMDATA%` on Windows), and
//! [`materialize_identity`] mints a `UUIDv4` for each device on first run, persists it atomically,
//! and never overwrites an id that already exists.
//!
//! The helpers operate on `serde_json::Value` + JSON pointers so they apply uniformly across the
//! heterogeneous driver config shapes (one device or several, at different pointers).

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod actions;

use std::path::{Path, PathBuf};

use serde_json::Value;
use uuid::Uuid;

/// Errors from config-path resolution, reading, or persistence.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No platform config directory could be determined for the default path.
    #[error("could not determine a platform config directory")]
    NoConfigDir,
    /// The config file exists but is not valid JSON.
    #[error("config file {path} is not valid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The config file could not be read.
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The config file could not be persisted.
    #[error("could not persist config file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Resolve the config-file path.
///
/// The explicit `--config` path if given, else the platform default — the per-user config
/// directory on Unix (e.g. `~/.config/rusty-photon/<service>.json` on Linux), or the
/// machine-wide `%PROGRAMDATA%\rusty-photon\<service>.json` on Windows. A path is always
/// resolvable, so config persistence is never disabled for lack of one.
///
/// Windows deliberately does **not** use the per-user profile (ADR-015): the services run under
/// service accounts whose profile is buried in `...\systemprofile\AppData\Roaming`, so the
/// default must live in the one obvious, operator-editable machine-wide folder.
///
/// # Errors
///
/// Returns [`ConfigError::NoConfigDir`] if no explicit path is given and
/// the platform config directory cannot be determined.
pub fn resolve_config_path(
    service: &str,
    explicit: Option<PathBuf>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    Ok(default_config_dir()?.join(format!("{service}.json")))
}

/// The per-user platform config directory on non-Windows platforms
/// (`directories::ProjectDirs`, e.g. `~/.config/rusty-photon` on Linux).
///
/// Public because doctor resolves the same directory the services do.
///
/// # Errors
///
/// Returns [`ConfigError::NoConfigDir`] if the platform yields no config
/// directory (no resolvable home).
#[cfg(not(windows))]
pub fn default_config_dir() -> Result<PathBuf, ConfigError> {
    let dirs =
        directories::ProjectDirs::from("", "", "rusty-photon").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().to_path_buf())
}

/// The machine-wide config directory on Windows: `%PROGRAMDATA%\rusty-photon`.
/// Public because doctor resolves the same directory the services do.
///
/// # Errors
///
/// Never fails on Windows — the fixed `C:\ProgramData` fallback always
/// resolves; the `Result` keeps the signature identical across platforms.
#[cfg(windows)]
pub fn default_config_dir() -> Result<PathBuf, ConfigError> {
    Ok(program_data_root(std::env::var_os("ProgramData")).join("rusty-photon"))
}

/// Pure resolution of the Windows `ProgramData` root from the value of the `ProgramData`
/// environment variable: the value verbatim when present and non-empty, else the fixed
/// `C:\ProgramData` fallback. Parameterized over the env value, and compiled on Windows and in
/// test builds on every platform, so the logic is unit-testable on non-Windows hosts.
#[cfg(any(windows, test))]
fn program_data_root(program_data: Option<std::ffi::OsString>) -> PathBuf {
    match program_data {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(r"C:\ProgramData"),
    }
}

/// Read the file at `path` as a JSON `Value`; a missing file yields a clone of `default`, while a
/// present-but-corrupt file is an error (so a typo never silently resets config).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidJson`] if the file exists but does not
/// parse, and [`ConfigError::Read`] for any read failure other than the
/// file being absent.
pub fn read_file_value(path: &Path, default: &Value) -> Result<Value, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|source| ConfigError::InvalidJson {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default.clone()),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Stage `value` as pretty JSON in a synced temp file next to `path` (same
/// directory, so the final rename/link stays on one filesystem).
fn stage_pretty_json<'p>(
    path: &'p Path,
    value: &Value,
) -> std::io::Result<(tempfile::NamedTempFile, &'p Path)> {
    use std::io::Write as _;

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)?;

    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;
    Ok((tmp, parent))
}

#[cfg(unix)]
fn sync_dir(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}
#[cfg(not(unix))]
fn sync_dir(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Carry the replaced file's mode and owner over to the staged temp file (Unix). The rename in
/// [`save`] replaces the inode, so without this a privileged caller — doctor under sudo, the only
/// practical way to run it against a packaged install's config root — would strand the config
/// root-owned and unreadable by the service user. The invariant is that a save never changes who
/// owns the file: the chown runs whenever the staged file's owner differs from the original's,
/// and a chown that fails is a save error. For an unprivileged caller that only happens in an
/// anomalous state (a config hand-chowned to another user), where the save now fails with
/// `PermissionDenied` instead of silently re-owning the file to the writer — surfacing the
/// anomaly beats papering over it.
#[cfg(unix)]
fn preserve_owner_and_mode(path: &Path, tmp: &tempfile::NamedTempFile) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // Deliberately follows a symlinked config to its target: the target's
    // attributes are what the reading service effectively sees, while the
    // link inode's are a fixed 0o777 and the link creator's uid — exactly
    // the wrong thing to stamp onto the regular file the rename leaves in
    // the link's place. The rename itself never follows the link, so the
    // target is only ever read, never written.
    let original = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Ownership before mode: chown clears setuid (and setgid on
    // group-executable files) even for root, so the mode must be applied
    // to the final owner.
    let staged = tmp.as_file().metadata()?;
    if (staged.uid(), staged.gid()) != (original.uid(), original.gid()) {
        std::os::unix::fs::fchown(tmp.as_file(), Some(original.uid()), Some(original.gid()))
            .map_err(|e| ownership_error(original.uid(), original.gid(), &e))?;
    }
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(original.mode() & 0o7777))?;
    tmp.as_file().sync_all()
}

/// Context for a failed ownership transfer in [`preserve_owner_and_mode`]:
/// names the step and the owner being kept, so a packaged-install failure is
/// diagnosable from the save error alone instead of a bare EPERM.
#[cfg(unix)]
fn ownership_error(uid: u32, gid: u32, e: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        e.kind(),
        format!("keeping the replaced file's owner {uid}:{gid}: {e}"),
    )
}

#[cfg(not(unix))]
fn preserve_owner_and_mode(_path: &Path, _tmp: &tempfile::NamedTempFile) -> std::io::Result<()> {
    Ok(())
}

/// Atomically persist `value` as pretty JSON.
///
/// Creates parent dirs, stages to a uniquely-named temp file in the same directory, fsyncs,
/// renames into place, then fsyncs the directory (Unix) so the rename itself is durable. When a
/// file is being replaced, its mode and owner survive onto the new inode (Unix), so a
/// privileged caller never leaves a config the owning service can no longer read. A save that
/// cannot keep the original owner — an unprivileged caller replacing a file owned by another
/// user — fails with `PermissionDenied` rather than changing who owns it.
///
/// # Errors
///
/// Returns the I/O error from any staging, ownership, rename, or sync
/// step.
pub fn save(path: &Path, value: &Value) -> std::io::Result<()> {
    let (tmp, parent) = stage_pretty_json(path, value)?;
    preserve_owner_and_mode(path, &tmp)?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync_dir(parent)
}

/// Persist `default` at `path` if no config file exists there yet, so a
/// fresh install materializes an editable file on the service's first
/// start.
///
/// Returns whether a file was written; an existing file is never touched —
/// the final step is an atomic no-clobber link, so even a file created
/// concurrently between the existence check and the write survives intact.
///
/// # Errors
///
/// Returns [`ConfigError::Write`] if staging or persisting the default
/// file fails.
pub fn init_file_if_absent(path: &Path, default: &Value) -> Result<bool, ConfigError> {
    if path.exists() {
        return Ok(false);
    }
    let wrap = |source: std::io::Error| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };
    let (tmp, parent) = stage_pretty_json(path, default).map_err(wrap)?;
    match tmp.persist_noclobber(path) {
        Ok(_) => {
            sync_dir(parent).map_err(wrap)?;
            Ok(true)
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(wrap(e.error)),
    }
}

/// The canonical startup bootstrap — the one call every service binary makes
/// before its first config read. In order:
///
/// 1. Resolve the config path: the explicit `--config` value if given, else
///    the platform default.
/// 2. Mint device identity: every `identity_pointers` entry that is absent or
///    empty receives a fresh `UUIDv4` ([`materialize_identity`]), persisted to
///    the resolved path — explicit **or** default, because a minted ASCOM
///    `UniqueID` is only an identity if the service re-reads the same value on
///    every future start. Minting is the one step that will create a missing
///    explicit file — and only when it actually fills an id: a `default`
///    whose pointers already hold non-empty ids has nothing to persist, so a
///    missing explicit file stays absent. Minting runs before step 3 so a
///    pointered first start writes the scaffold once, ids already filled.
/// 3. When the path is the platform default and no file exists yet, persist
///    `default` there, so a packaged install materializes an editable file on
///    first start — and so same-host consumers that derive facts from the
///    file (sentinel's health probes, doctor) can read it. An explicit path
///    is never self-created by this step: what a missing explicit file means
///    is the caller's policy — strict-config services treat it as a hard load
///    error (a typo'd `--config` must not silently run on defaults), while
///    the `config.apply` drivers deliberately fall back to in-memory
///    defaults.
///
/// Services whose device identities come from elsewhere (camera SDK serials)
/// or that expose no devices pass `&[]` — declining minting is a visible
/// choice here, and never silently drops the first-start materialization.
///
/// # Errors
///
/// Returns a [`ConfigError`] if the path cannot be resolved, the
/// existing file is unreadable or corrupt, or writing the minted /
/// default file fails.
pub fn resolve_and_init(
    service: &str,
    explicit: Option<PathBuf>,
    default: &Value,
    identity_pointers: &[&str],
) -> Result<PathBuf, ConfigError> {
    let is_explicit = explicit.is_some();
    let path = resolve_config_path(service, explicit)?;
    // Minting runs first: a first start with identity pointers then writes the
    // scaffold once, with the ids already filled, and the init step below
    // finds the file present.
    if !identity_pointers.is_empty() {
        let outcome = materialize_identity(&path, default, identity_pointers)?;
        if outcome.wrote {
            tracing::debug!(
                "Minted device UniqueID(s) {:?} into {}",
                outcome.filled,
                path.display()
            );
        } else {
            tracing::debug!("Device UniqueID(s) already present; not minting");
        }
    }
    if !is_explicit && init_file_if_absent(&path, default)? {
        tracing::info!("Created default config at {}", path.display());
    }
    Ok(path)
}

/// The result of [`materialize_identity`].
pub struct MaterializeOutcome {
    /// Whether the file was (re)written (i.e. at least one id was minted).
    pub wrote: bool,
    /// The JSON pointers that received a freshly-minted id.
    pub filled: Vec<String>,
    /// The post-materialization file `Value`.
    pub value: Value,
}

/// Ensure every pointer in `identity_pointers` holds a non-empty string
/// `UniqueID` in the **file layer**, minting a fresh `UUIDv4` for any
/// that are absent, non-string, or empty.
///
/// Idempotent (only fills empties; never overwrites an existing id) and
/// persists only when it actually filled something. Operates solely on
/// the on-disk file (never a CLI-override-applied effective config), so a
/// transient `--port` is never baked in.
///
/// # Errors
///
/// Returns a [`ConfigError`] if the existing file is unreadable or
/// corrupt, or if persisting the minted ids fails.
pub fn materialize_identity(
    path: &Path,
    default_value: &Value,
    identity_pointers: &[&str],
) -> Result<MaterializeOutcome, ConfigError> {
    let mut value = read_file_value(path, default_value)?;
    let mut filled = Vec::new();

    for ptr in identity_pointers {
        let needs = match value.pointer(ptr) {
            Some(Value::String(s)) => s.trim().is_empty(),
            _ => true, // absent, null, or non-string
        };
        if needs {
            insert_at_pointer(&mut value, ptr, Value::String(Uuid::new_v4().to_string()));
            filled.push((*ptr).to_string());
        }
    }

    let wrote = if filled.is_empty() {
        false
    } else {
        save(path, &value).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        true
    };

    Ok(MaterializeOutcome {
        wrote,
        filled,
        value,
    })
}

/// Set `new` at the RFC-6901 JSON `pointer`, creating intermediate objects as needed (unlike
/// `Value::pointer_mut`, which returns `None` for a missing key).
fn insert_at_pointer(root: &mut Value, pointer: &str, new: Value) {
    let tokens: Vec<String> = pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect();
    let Some((last, parents)) = tokens.split_last() else {
        return;
    };

    let mut cur = root;
    for tok in parents {
        if !cur.is_object() {
            *cur = Value::Object(serde_json::Map::new());
        }
        let Some(map) = cur.as_object_mut() else {
            return;
        };
        cur = map
            .entry(tok.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }

    if !cur.is_object() {
        *cur = Value::Object(serde_json::Map::new());
    }
    if let Some(map) = cur.as_object_mut() {
        map.insert(last.clone(), new);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_uses_explicit_path() {
        let p = resolve_config_path("dsd-fp2", Some(PathBuf::from("/tmp/x.json"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/x.json"));
    }

    #[test]
    fn resolve_defaults_to_platform_dir() {
        let p = resolve_config_path("dsd-fp2", None).unwrap();
        assert!(p.ends_with("dsd-fp2.json"), "{p:?}");
        assert!(p.to_string_lossy().contains("rusty-photon"), "{p:?}");
    }

    #[test]
    fn program_data_root_uses_env_value_verbatim() {
        let p = program_data_root(Some(std::ffi::OsString::from(r"D:\CustomData")));
        assert_eq!(p, PathBuf::from(r"D:\CustomData"));
    }

    #[test]
    fn program_data_root_falls_back_when_env_absent() {
        assert_eq!(program_data_root(None), PathBuf::from(r"C:\ProgramData"));
    }

    #[test]
    fn program_data_root_falls_back_when_env_empty() {
        assert_eq!(
            program_data_root(Some(std::ffi::OsString::new())),
            PathBuf::from(r"C:\ProgramData")
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_defaults_to_machine_wide_program_data_on_windows() {
        let p = resolve_config_path("dsd-fp2", None).unwrap();
        assert!(p.ends_with(r"rusty-photon\dsd-fp2.json"), "{p:?}");
        assert!(p.is_absolute(), "{p:?}");
        // The per-user profile must never be the service default (ADR-015):
        // under a service account it lands in the hidden systemprofile dir.
        assert!(!p.to_string_lossy().contains("AppData"), "{p:?}");
    }

    #[test]
    fn init_file_if_absent_writes_default_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let default = json!({ "server": { "port": 11111 } });

        assert!(init_file_if_absent(&path, &default).unwrap());
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk, default);
    }

    #[test]
    fn init_file_if_absent_never_touches_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{\"server\":{\"port\":9}}").unwrap();

        assert!(!init_file_if_absent(&path, &json!({ "server": { "port": 11111 } })).unwrap());
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk, json!({ "server": { "port": 9 } }));
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_the_replaced_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        save(&path, &json!({ "server": { "port": 1 } })).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640, "the temp file's 0600 must not replace 0640");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_the_replaced_files_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();
        // Only root may hand a file to another owner, so the cross-owner
        // case (a sudo'd doctor rewriting the service user's config) is
        // exercised on privileged runs (root CI containers); unprivileged
        // runs still pin the owner across the inode swap.
        let cross_owner = std::os::unix::fs::chown(&path, Some(12345), Some(12345)).is_ok();
        // The setuid bit doubles as an ordering probe: chown always clears
        // it (setgid survives on non-group-executable files), so it only
        // survives a cross-owner save if the mode is applied after the
        // ownership transfer. Set after the chown above for the same reason.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4640)).unwrap();
        let before = std::fs::metadata(&path).unwrap();

        save(&path, &json!({ "server": { "port": 1 } })).unwrap();

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (after.uid(), after.gid()),
            (before.uid(), before.gid()),
            "owner must survive the rename (cross-owner run: {cross_owner})"
        );
        if cross_owner {
            assert_eq!((after.uid(), after.gid()), (12345, 12345));
        }
        assert_eq!(
            after.permissions().mode() & 0o7777,
            0o4640,
            "setuid must survive the ownership transfer (cross-owner run: {cross_owner})"
        );
    }

    /// A gid from `id -G` different from `primary`, if the environment has
    /// one. An owner may hand a file to any group they belong to, so this
    /// lets the ownership-transfer path run without privileges.
    #[cfg(unix)]
    fn supplementary_gid(primary: u32) -> Option<u32> {
        let out = std::process::Command::new("id").arg("-G").output().ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .find(|g| *g != primary)
    }

    #[cfg(unix)]
    #[test]
    fn save_transfers_group_ownership_back_to_the_original() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();
        let primary = std::fs::metadata(&path).unwrap().gid();
        let Some(other) = supplementary_gid(primary) else {
            eprintln!("single-group environment; the cross-gid path needs the privileged tests");
            return;
        };
        // Sandboxes with a single-mapping user namespace (bazel's
        // linux-sandbox) cannot express the transfer at all (EINVAL);
        // plain cargo runs and real machines can.
        if std::os::unix::fs::chown(&path, None, Some(other)).is_err() {
            eprintln!("environment cannot chgrp to a supplementary group; skipping");
            return;
        }

        save(&path, &json!({ "server": { "port": 1 } })).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().gid(),
            other,
            "the staged file's primary gid must not replace the original's group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_surfaces_a_stat_error_on_the_replaced_path() {
        #[cfg(target_os = "linux")]
        const ELOOP: i32 = 40;
        #[cfg(not(target_os = "linux"))]
        const ELOOP: i32 = 62;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // A self-looping symlink: the only stat outcome that is neither
        // success nor NotFound and needs no privileges to set up.
        std::os::unix::fs::symlink("c.json", &path).unwrap();

        let err = save(&path, &json!({})).unwrap_err();

        assert_eq!(err.raw_os_error(), Some(ELOOP), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn ownership_error_keeps_the_kind_and_names_the_owner() {
        let e = ownership_error(
            985,
            985,
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        let msg = e.to_string();
        assert!(
            msg.contains("keeping the replaced file's owner 985:985"),
            "{msg}"
        );
    }

    #[test]
    fn resolve_and_init_leaves_missing_explicit_path_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("typo.json");

        let p = resolve_and_init("dsd-fp2", Some(missing.clone()), &json!({}), &[]).unwrap();

        assert_eq!(p, missing);
        assert!(
            !missing.exists(),
            "explicit path must never be self-created"
        );
    }

    #[test]
    fn resolve_and_init_mints_identity_at_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let default = json!({ "device": { "unique_id": "" } });

        let p = resolve_and_init(
            "dsd-fp2",
            Some(path.clone()),
            &default,
            &["/device/unique_id"],
        )
        .unwrap();

        assert_eq!(p, path);
        // Minting is the one case where a missing explicit file is created:
        // the id must be on disk where every future start re-reads it.
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let id = on_disk
            .pointer("/device/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn resolve_and_init_keeps_existing_identity_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"device":{"unique_id":"keep-me"},"port":9}"#).unwrap();

        resolve_and_init(
            "dsd-fp2",
            Some(path.clone()),
            &json!({ "device": { "unique_id": "" }, "port": 1 }),
            &["/device/unique_id"],
        )
        .unwrap();

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk,
            json!({ "device": { "unique_id": "keep-me" }, "port": 9 })
        );
    }

    /// Serializes the tests that mutate `XDG_CONFIG_HOME` — env vars are
    /// process-global, and plain `cargo test` runs tests as threads.
    /// Poison-tolerant: these tests panic on failure, which would otherwise
    /// cascade into spurious later-test failures.
    #[cfg(target_os = "linux")]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores an env var's prior state on drop (including on panic).
    #[cfg(target_os = "linux")]
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    #[cfg(target_os = "linux")]
    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }
    #[cfg(target_os = "linux")]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_and_init_creates_default_at_xdg_path() {
        // XDG_CONFIG_HOME is honored on Linux only; other platforms would hit
        // the real per-user dir, so this test is Linux-scoped.
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let default = json!({ "server": { "port": 11111 } });

        let p = resolve_and_init("xdg-init-test", None, &default, &[]).unwrap();

        assert!(p.starts_with(dir.path()), "{p:?}");
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(on_disk, default);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_and_init_materializes_and_mints_at_xdg_path() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("XDG_CONFIG_HOME", dir.path());
        let default = json!({ "server": { "port": 1 }, "device": { "unique_id": "" } });

        let p = resolve_and_init("xdg-mint-test", None, &default, &["/device/unique_id"]).unwrap();

        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(on_disk.pointer("/server/port"), Some(&json!(1)));
        let id = on_disk
            .pointer("/device/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_fills_empty_and_persists_valid_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let default = json!({ "cover_calibrator": { "unique_id": "" } });

        let out = materialize_identity(&path, &default, &["/cover_calibrator/unique_id"]).unwrap();

        assert!(out.wrote);
        assert_eq!(out.filled, vec!["/cover_calibrator/unique_id".to_string()]);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let id = on_disk
            .pointer("/cover_calibrator/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_is_idempotent_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let default = json!({ "d": { "unique_id": "" } });

        let first = materialize_identity(&path, &default, &["/d/unique_id"]).unwrap();
        assert!(first.wrote);
        let id1 = first
            .value
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let second = materialize_identity(&path, &default, &["/d/unique_id"]).unwrap();
        assert!(!second.wrote);
        assert!(second.filled.is_empty());
        let id2 = second
            .value
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_eq!(id1, id2);
    }

    #[test]
    fn materialize_never_overwrites_existing_and_fills_only_empties() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(
            &path,
            r#"{"a":{"unique_id":"keep-me"},"b":{"unique_id":""}}"#,
        )
        .unwrap();

        let out =
            materialize_identity(&path, &json!({}), &["/a/unique_id", "/b/unique_id"]).unwrap();

        assert!(out.wrote);
        assert_eq!(out.filled, vec!["/b/unique_id".to_string()]);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk.pointer("/a/unique_id").and_then(Value::as_str),
            Some("keep-me")
        );
        let b = on_disk
            .pointer("/b/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(b).unwrap();
    }

    #[test]
    fn materialize_absent_file_writes_default_scaffold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let default = json!({ "serial": { "port": "/dev/ttyACM0" }, "d": { "unique_id": "" } });

        let out = materialize_identity(&path, &default, &["/d/unique_id"]).unwrap();

        assert!(out.wrote);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Non-identity defaults are written through; identity is minted.
        assert_eq!(
            on_disk.pointer("/serial/port").and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        assert!(on_disk
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .is_some());
    }

    #[test]
    fn materialize_inserts_absent_pointer_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // Present file whose device object lacks `unique_id` entirely.
        std::fs::write(&path, r#"{"device":{"name":"cam"}}"#).unwrap();

        let out = materialize_identity(&path, &json!({}), &["/device/unique_id"]).unwrap();

        assert!(out.wrote);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(on_disk
            .pointer("/device/unique_id")
            .and_then(Value::as_str)
            .is_some());
        assert_eq!(
            on_disk.pointer("/device/name").and_then(Value::as_str),
            Some("cam")
        );
    }

    #[test]
    fn read_file_value_missing_returns_default() {
        let v = read_file_value(
            Path::new("/tmp/rusty-photon-config-definitely-missing-zzz.json"),
            &json!({ "x": 1 }),
        )
        .unwrap();
        assert_eq!(v, json!({ "x": 1 }));
    }

    #[test]
    fn read_file_value_corrupt_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = read_file_value(&path, &json!({})).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidJson { .. }), "{err:?}");
    }

    #[test]
    fn save_round_trips_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        save(&path, &json!({ "k": "v" })).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, json!({ "k": "v" }));

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "s.json")
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files present");
    }

    #[test]
    fn read_file_value_read_error_when_path_is_a_directory() {
        // Reading a *directory* (rather than a file) fails with a non-NotFound
        // error on every platform — `IsADirectory` on unix, access-denied on
        // Windows — so it must surface as `Read`, not the default. (A file in
        // the middle of the path is ENOTDIR on unix but NotFound on Windows, so
        // a directory is the portable way to force a non-NotFound read error.)
        let dir = tempfile::tempdir().unwrap();

        let err = read_file_value(dir.path(), &json!({})).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn materialize_write_error_surfaces_when_dir_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        // Readable+executable (so the absent file reads as NotFound → default),
        // but not writable (so the persist step fails).
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let path = ro.join("c.json");

        let result = materialize_identity(
            &path,
            &json!({ "d": { "unique_id": "" } }),
            &["/d/unique_id"],
        );

        // Restore write perms first so the tempdir cleanup always succeeds.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();

        match result {
            Err(ConfigError::Write { .. }) => {}
            // Running as root bypasses the directory mode, so the write succeeds
            // and there is nothing to assert; CI runs as a normal user.
            Ok(_) => {}
            Err(other) => panic!("expected ConfigError::Write, got {other:?}"),
        }
    }

    #[test]
    fn materialize_treats_non_string_id_as_empty() {
        // A hand-edited config with a non-string id is treated as missing and reminted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"d":{"unique_id":123}}"#).unwrap();

        let out = materialize_identity(&path, &json!({}), &["/d/unique_id"]).unwrap();

        assert!(out.wrote);
        assert_eq!(out.filled, vec!["/d/unique_id".to_string()]);
        let id = out
            .value
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_treats_whitespace_id_as_empty() {
        // Whitespace-only ids are blank after trimming and so are reminted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{\"d\":{\"unique_id\":\"   \"}}").unwrap();

        let out = materialize_identity(&path, &json!({}), &["/d/unique_id"]).unwrap();

        assert!(out.wrote);
        assert_eq!(out.filled, vec!["/d/unique_id".to_string()]);
        let id = out
            .value
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_replaces_non_object_root() {
        // A corrupt file whose root is an array (not an object) is rebuilt into
        // the object scaffold the pointer needs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "[1,2,3]").unwrap();

        let out = materialize_identity(&path, &json!({}), &["/d/unique_id"]).unwrap();

        assert!(out.wrote);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let id = on_disk
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_replaces_non_object_at_pointer_parent() {
        // The parent of the pointer exists but is a scalar; it is replaced with
        // an object so the id can be inserted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"device":"oops"}"#).unwrap();

        let out = materialize_identity(&path, &json!({}), &["/device/unique_id"]).unwrap();

        assert!(out.wrote);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let id = on_disk
            .pointer("/device/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn materialize_honors_rfc6901_escaped_tokens() {
        // `~1` decodes to `/`, so the pointer addresses a key literally named "a/b".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");

        let out = materialize_identity(&path, &json!({}), &["/a~1b/unique_id"]).unwrap();

        assert!(out.wrote);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let id = on_disk
            .pointer("/a~1b/unique_id")
            .and_then(Value::as_str)
            .unwrap();
        Uuid::parse_str(id).unwrap();
    }

    #[test]
    fn insert_at_pointer_empty_pointer_is_noop() {
        // An empty pointer has no tokens, so the value is left untouched.
        let mut v = json!({ "a": 1 });
        insert_at_pointer(&mut v, "", json!("x"));
        assert_eq!(v, json!({ "a": 1 }));
    }

    #[test]
    fn save_falls_back_to_cwd_for_bare_filename() {
        // A bare filename has an empty parent; `save` must fall back to the CWD.
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = save(Path::new("bare.json"), &json!({ "k": "v" }));

        // Restore the CWD before asserting so a failure cannot strand sibling tests.
        std::env::set_current_dir(&prev).unwrap();
        result.unwrap();

        let written = dir.path().join("bare.json");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(&written).unwrap()).unwrap();
        assert_eq!(back, json!({ "k": "v" }));
    }
}

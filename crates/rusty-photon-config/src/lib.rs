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
//! heterogeneous driver config shapes (one device or several, at different pointers). Each service
//! names its own configuration type through [`ConfigFile`]: the bootstrap writes to a file only
//! when the result loads as that type, and [`load_file`] reads a file as one. See
//! `docs/crates/rusty-photon-config.md`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
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

use serde::de::DeserializeOwned;
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
    /// The config file is valid JSON but does not deserialize as the
    /// service's configuration: a field is missing, unknown, of the wrong
    /// type or out of range.
    #[error("config file {path} is valid JSON but not a valid configuration: {source}")]
    InvalidConfig {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The config file deserializes, but the service's
    /// [`ConfigFile::check`] refuses it.
    #[error("config file {path} is valid JSON but not a valid configuration: {reason}")]
    Rejected { path: PathBuf, reason: String },
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

/// A service's configuration type: what its config file must hold.
///
/// [`load_file`] reads a file as one, and the bootstrap ([`resolve_and_init`],
/// [`materialize_identity`]) writes to a file only when the result is one — so
/// the bootstrap never leaves behind a file the service would refuse. A file
/// "loads" when it deserializes as the type and passes [`check`](Self::check).
pub trait ConfigFile: DeserializeOwned {
    /// The rules a parse cannot express, such as one spanning several
    /// blocks. A service whose loader enforces such a rule must express it
    /// here, or the bootstrap could write a file the loader then refuses. The
    /// default accepts every configuration that deserializes.
    ///
    /// # Errors
    ///
    /// Returns why the configuration is refused.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Resolve the config-file path.
///
/// The explicit `--config` path if given, else the platform default — the per-user config
/// directory on Unix (e.g. `~/.config/rusty-photon/<service>.json` on Linux), or the
/// machine-wide `%PROGRAMDATA%\rusty-photon\<service>.json` on Windows. Windows always
/// resolves (the fixed fallback needs no environment); Unix needs a resolvable home —
/// the one failure named below — so in practice config persistence is never disabled
/// for lack of a path.
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
    read_text(path)?.map_or_else(|| Ok(default.clone()), |text| parse_json(path, &text))
}

/// Load the config file at `path` as the service's configuration `C`, or
/// `None` when there is no file — whether that means "run on defaults" or
/// "refuse to start" is the caller's policy.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidJson`] for a syntax error,
/// [`ConfigError::InvalidConfig`] for valid JSON that does not deserialize
/// as `C`, [`ConfigError::Rejected`] when [`ConfigFile::check`] refuses it,
/// and [`ConfigError::Read`] for any read failure other than the file being
/// absent.
pub fn load_file<C: ConfigFile>(path: &Path) -> Result<Option<C>, ConfigError> {
    read_text(path)?
        .map(|text| parse_config(path, &text))
        .transpose()
}

/// The file's contents, or `None` when it does not exist.
fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parse `text`, the contents of the file at `path`, as a JSON `Value`.
fn parse_json(path: &Path, text: &str) -> Result<Value, ConfigError> {
    serde_json::from_str(text).map_err(|source| ConfigError::InvalidJson {
        path: path.to_path_buf(),
        source,
    })
}

/// Parse `text`, the contents of the file at `path`, as `C`. A syntax error
/// and valid JSON of the wrong shape are told apart, so a missing field is
/// never reported as invalid JSON.
fn parse_config<C: ConfigFile>(path: &Path, text: &str) -> Result<C, ConfigError> {
    let config = serde_json::from_str(text).map_err(|source| {
        let path = path.to_path_buf();
        if source.is_data() {
            ConfigError::InvalidConfig { path, source }
        } else {
            ConfigError::InvalidJson { path, source }
        }
    })?;
    checked(path, config)
}

/// Deserialize `value`, meant for the file at `path`, as `C`.
fn parse_value<C: ConfigFile>(path: &Path, value: &Value) -> Result<C, ConfigError> {
    let config = C::deserialize(value).map_err(|source| ConfigError::InvalidConfig {
        path: path.to_path_buf(),
        source,
    })?;
    checked(path, config)
}

/// Apply `C`'s [`ConfigFile::check`] to a deserialized `config`.
fn checked<C: ConfigFile>(path: &Path, config: C) -> Result<C, ConfigError> {
    config.check().map_err(|reason| ConfigError::Rejected {
        path: path.to_path_buf(),
        reason,
    })?;
    Ok(config)
}

/// Refuse `text`, the contents of the file at `path`, when an object in it
/// holds the same key twice — the one thing a parse into a `Value` loses.
fn reject_duplicate_keys(path: &Path, text: &str) -> Result<(), ConfigError> {
    serde_json::from_str::<UniqueKeys>(text)
        .map(|UniqueKeys| ())
        .map_err(|source| ConfigError::InvalidConfig {
            path: path.to_path_buf(),
            source,
        })
}

/// Any JSON value whose objects each hold every key at most once. Parsing
/// into it checks that and keeps nothing.
struct UniqueKeys;

impl<'de> serde::Deserialize<'de> for UniqueKeys {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> serde::de::Visitor<'de> for UniqueKeys {
    type Value = Self;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_unit<E>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<Self>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if seen.contains(&key) {
                return Err(serde::de::Error::custom(format_args!(
                    "duplicate key `{key}`"
                )));
            }
            map.next_value::<Self>()?;
            seen.insert(key);
        }
        Ok(self)
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "mirrors the fallible unix variant's signature"
)]
const fn sync_dir(_parent: &Path) -> std::io::Result<()> {
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "mirrors the fallible unix variant's signature"
)]
const fn preserve_owner_and_mode(
    _path: &Path,
    _tmp: &tempfile::NamedTempFile,
) -> std::io::Result<()> {
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
///    every future start. A section the file leaves out is filled in from
///    `default` first, and the file is written only when the result loads as
///    `C` — a file the service would refuse is left exactly as it was.
///    Minting is the one step that will create a missing
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
/// existing file is unreadable or corrupt, minting would leave a file that
/// does not load as `C` (the error is the one the file as written produces),
/// or writing the minted / default file fails.
pub fn resolve_and_init<C: ConfigFile>(
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
        let outcome = materialize_identity::<C>(&path, default, identity_pointers)?;
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
/// A section the file leaves out is copied from the same place in
/// `default_value` before the id goes in, so it arrives with the fields the
/// service needs instead of holding the id alone. Where something other than
/// an object stands in the way, nothing is minted there: that is the
/// operator's to fix, and the load reports it.
///
/// Idempotent (only fills empties; never overwrites an existing id) and
/// persists only when it actually filled something **and** the result loads
/// as `C` — so a file the service would refuse is never written. Operates
/// solely on the on-disk file (never a CLI-override-applied effective
/// config), so a transient `--port` is never baked in.
///
/// # Errors
///
/// Returns a [`ConfigError`] if the existing file is unreadable or not
/// valid JSON, if the minted result would not load as `C` — the error is
/// then the one the file as written produces, which names the operator's
/// line and column — or if persisting the minted ids fails.
pub fn materialize_identity<C: ConfigFile>(
    path: &Path,
    default_value: &Value,
    identity_pointers: &[&str],
) -> Result<MaterializeOutcome, ConfigError> {
    let text = read_text(path)?;
    let mut value = match &text {
        Some(text) => parse_json(path, text)?,
        None => default_value.clone(),
    };
    let mut filled = Vec::new();

    for ptr in identity_pointers {
        let needs = match value.pointer(ptr) {
            Some(Value::String(s)) => s.trim().is_empty(),
            _ => true, // absent, null, or non-string
        };
        if needs && insert_identity(&mut value, default_value, ptr, fresh_id()) {
            filled.push((*ptr).to_string());
        }
    }

    let wrote = if filled.is_empty() {
        false
    } else {
        // Write only a file the service will load. A refusal is reported
        // against the file as written: that is the text the operator opens
        // to fix, and its error carries their line and column. The minted
        // result's own error stands in only where there is no such text.
        if let Err(refused) = parse_value::<C>(path, &value) {
            return Err(match text.map(|text| parse_config::<C>(path, &text)) {
                Some(Err(as_written)) => as_written,
                _ => refused,
            });
        }
        // A `Value` keeps only the last of two equal keys, so the rewrite
        // would silently drop the operator's other one.
        if let Some(text) = &text {
            reject_duplicate_keys(path, text)?;
        }
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

/// Set `id` at the RFC-6901 JSON `pointer`, creating the objects missing on
/// the way (unlike `Value::pointer_mut`, which returns `None` for a missing
/// key). A missing object is copied from the same place in `default` when the
/// default holds an object there, and starts empty otherwise.
///
/// Returns `false` when something other than an object stands in the way —
/// the root, or a section on the path — and replaces nothing.
fn insert_identity(root: &mut Value, default: &Value, pointer: &str, id: Value) -> bool {
    let tokens: Vec<&str> = pointer.split('/').skip(1).collect();
    let Some((last, parents)) = tokens.split_last() else {
        return false;
    };

    let mut cur = root;
    let mut at = String::new();
    for token in parents {
        at.push('/');
        at.push_str(token);
        let Some(map) = cur.as_object_mut() else {
            return false;
        };
        cur = map.entry(unescape(token)).or_insert_with(|| {
            default
                .pointer(&at)
                .filter(|section| section.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
        });
    }

    let Some(map) = cur.as_object_mut() else {
        return false;
    };
    map.insert(unescape(last), id);
    true
}

/// A freshly minted `UniqueID`.
fn fresh_id() -> Value {
    Value::String(Uuid::new_v4().to_string())
}

/// Decode one RFC-6901 reference token: `~1` is `/`, then `~0` is `~`.
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    /// Any JSON loads: the minting mechanics under test, with no shape to
    /// check.
    impl ConfigFile for Value {}

    /// A device section the way the drivers declare one: `name` required,
    /// the id defaulted so a file without one still deserializes.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Device {
        name: String,
        #[serde(default)]
        unique_id: String,
    }

    /// A service configuration with a required device section and one rule
    /// the parse cannot express.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Typed {
        port: u16,
        device: Device,
    }

    impl ConfigFile for Typed {
        fn check(&self) -> Result<(), String> {
            if self.port == 0 {
                Err("port must not be 0".to_string())
            } else {
                Ok(())
            }
        }
    }

    /// `Typed`'s default: a complete device section with an empty id.
    fn typed_default() -> Value {
        json!({ "port": 1, "device": { "name": "Default Cam", "unique_id": "" } })
    }

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

        let p =
            resolve_and_init::<Value>("dsd-fp2", Some(missing.clone()), &json!({}), &[]).unwrap();

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

        let p = resolve_and_init::<Value>(
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

        resolve_and_init::<Value>(
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

        let p = resolve_and_init::<Value>("xdg-init-test", None, &default, &[]).unwrap();

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

        let p = resolve_and_init::<Value>("xdg-mint-test", None, &default, &["/device/unique_id"])
            .unwrap();

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

        let out = materialize_identity::<Value>(&path, &default, &["/cover_calibrator/unique_id"])
            .unwrap();

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

        let first = materialize_identity::<Value>(&path, &default, &["/d/unique_id"]).unwrap();
        assert!(first.wrote);
        let id1 = first
            .value
            .pointer("/d/unique_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let second = materialize_identity::<Value>(&path, &default, &["/d/unique_id"]).unwrap();
        assert!(!second.wrote);
        assert_eq!(second.filled, Vec::<String>::new());
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
            materialize_identity::<Value>(&path, &json!({}), &["/a/unique_id", "/b/unique_id"])
                .unwrap();

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

        let out = materialize_identity::<Value>(&path, &default, &["/d/unique_id"]).unwrap();

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

        let out = materialize_identity::<Value>(&path, &json!({}), &["/device/unique_id"]).unwrap();

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

        let result = materialize_identity::<Value>(
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

        let out = materialize_identity::<Value>(&path, &json!({}), &["/d/unique_id"]).unwrap();

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

        let out = materialize_identity::<Value>(&path, &json!({}), &["/d/unique_id"]).unwrap();

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
    fn materialize_mints_nothing_through_a_non_object_root() {
        // A root that is an array is the operator's to fix; replacing it would
        // throw their content away. It stays, for the load to report.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "[1,2,3]").unwrap();

        let out = materialize_identity::<Value>(&path, &json!({}), &["/d/unique_id"]).unwrap();

        assert!(!out.wrote);
        assert_eq!(out.filled, Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2,3]");
    }

    #[test]
    fn materialize_mints_nothing_through_a_non_object_section() {
        // A section the operator wrote as a scalar is not replaced by an object.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"device":"oops"}"#).unwrap();

        let out = materialize_identity::<Value>(&path, &json!({}), &["/device/unique_id"]).unwrap();

        assert!(!out.wrote);
        assert_eq!(out.filled, Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"device":"oops"}"#
        );
    }

    #[test]
    fn materialize_fills_a_left_out_section_from_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"port": 9}"#).unwrap();

        let out =
            materialize_identity::<Typed>(&path, &typed_default(), &["/device/unique_id"]).unwrap();

        assert!(out.wrote);
        let loaded = load_file::<Typed>(&path).unwrap().unwrap();
        assert_eq!(loaded.port, 9);
        assert_eq!(loaded.device.name, "Default Cam");
        Uuid::parse_str(&loaded.device.unique_id).unwrap();
    }

    #[test]
    fn materialize_starts_a_section_empty_where_the_default_has_no_object() {
        // A default holding a scalar where the section would be is no section
        // to copy; the id gets an empty object of its own.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{}").unwrap();

        let out =
            materialize_identity::<Value>(&path, &json!({ "d": 5 }), &["/d/unique_id"]).unwrap();

        assert!(out.wrote);
        let d = out.value.pointer("/d").and_then(Value::as_object).unwrap();
        assert_eq!(d.keys().collect::<Vec<_>>(), ["unique_id"]);
    }

    #[test]
    fn materialize_leaves_a_file_that_would_not_load_untouched() {
        // A device section without its required `name` is the operator's to
        // fix: no id is written into it, and the error is the one their own
        // file produces, line and column included.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let written = "{\"port\": 9,\n\"device\": {}}";
        std::fs::write(&path, written).unwrap();

        let err = materialize_identity::<Typed>(&path, &typed_default(), &["/device/unique_id"])
            .err()
            .unwrap();

        assert!(matches!(err, ConfigError::InvalidConfig { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("is valid JSON but not a valid configuration: missing field `name`"),
            "{msg}"
        );
        assert!(msg.contains("line 2"), "{msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn materialize_leaves_a_file_its_check_refuses_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let written = r#"{"port": 0, "device": {"name": "cam"}}"#;
        std::fs::write(&path, written).unwrap();

        let err = materialize_identity::<Typed>(&path, &typed_default(), &["/device/unique_id"])
            .err()
            .unwrap();

        assert!(matches!(err, ConfigError::Rejected { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("is valid JSON but not a valid configuration: port must not be 0"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn materialize_refuses_a_default_that_would_not_load_and_writes_nothing() {
        // No file to report against: the error is the minted result's own.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        let err = materialize_identity::<Typed>(
            &path,
            &json!({ "port": 1, "device": { "unique_id": "" } }),
            &["/device/unique_id"],
        )
        .err()
        .unwrap();

        assert!(matches!(err, ConfigError::InvalidConfig { .. }), "{err:?}");
        assert!(!path.exists(), "a refused default must not be written");
    }

    #[test]
    fn materialize_refuses_a_file_that_is_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{ not json").unwrap();

        let err = materialize_identity::<Value>(&path, &json!({}), &["/d/unique_id"])
            .err()
            .unwrap();

        assert!(matches!(err, ConfigError::InvalidJson { .. }), "{err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn materialize_leaves_a_file_with_a_duplicate_key_untouched() {
        // A rewrite from a `Value` would keep only the second `x`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let written = "{\"d\": {},\n\"x\": 1, \"x\": 2}";
        std::fs::write(&path, written).unwrap();

        let err = materialize_identity::<Value>(&path, &json!({}), &["/d/unique_id"])
            .err()
            .unwrap();

        assert!(matches!(err, ConfigError::InvalidConfig { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("duplicate key `x`"), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn materialize_leaves_a_file_with_a_duplicate_field_untouched() {
        // Collapsed to its last `name`, the device section would load, so the
        // typed check alone would let the rewrite through.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let written = r#"{"port": 9, "device": {"name": "a", "name": "b"}}"#;
        std::fs::write(&path, written).unwrap();

        let err = materialize_identity::<Typed>(&path, &typed_default(), &["/device/unique_id"])
            .err()
            .unwrap();

        assert!(err.to_string().contains("`name`"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn unique_keys_accepts_every_kind_of_json_value() {
        // Equal keys in *different* objects are not duplicates.
        let text = r#"{"a": -1, "b": 2, "c": 1.5, "d": true, "e": null,
                       "f": [1, "x", {"a": "\"quoted\""}], "g": {"a": 1}}"#;

        reject_duplicate_keys(Path::new("c.json"), text).unwrap();
    }

    #[test]
    fn unique_keys_finds_a_duplicate_inside_an_array() {
        let err = reject_duplicate_keys(Path::new("c.json"), r#"[{"a": 1, "a": 2}]"#)
            .err()
            .unwrap();

        assert!(err.to_string().contains("duplicate key `a`"), "{err}");
    }

    #[test]
    fn unique_keys_names_what_it_expects() {
        // JSON never reaches `expecting`; bytes do.
        use serde::Deserialize as _;
        let bytes = serde::de::value::BytesDeserializer::<serde::de::value::Error>::new(b"x");

        let err = UniqueKeys::deserialize(bytes).err().unwrap();

        assert!(err.to_string().contains("a JSON value"), "{err}");
    }

    #[test]
    fn load_file_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();

        let loaded = load_file::<Typed>(&dir.path().join("missing.json")).unwrap();

        assert!(loaded.is_none());
    }

    #[test]
    fn load_file_reads_a_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(
            &path,
            r#"{"port": 9, "device": {"name": "cam", "unique_id": "id"}}"#,
        )
        .unwrap();

        let loaded = load_file::<Typed>(&path).unwrap().unwrap();

        assert_eq!(loaded.port, 9);
        assert_eq!(loaded.device.name, "cam");
        assert_eq!(loaded.device.unique_id, "id");
    }

    #[test]
    fn load_file_reports_a_syntax_error_as_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"port": 9,"#).unwrap();

        let err = load_file::<Typed>(&path).err().unwrap();

        assert!(matches!(err, ConfigError::InvalidJson { .. }), "{err:?}");
        assert!(err.to_string().contains("is not valid JSON"), "{err}");
    }

    #[test]
    fn load_file_reports_a_missing_field_as_an_invalid_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"port": 9, "device": {}}"#).unwrap();

        let err = load_file::<Typed>(&path).err().unwrap();

        assert!(matches!(err, ConfigError::InvalidConfig { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("is valid JSON but not a valid configuration: missing field `name`"),
            "{msg}"
        );
        assert!(!msg.contains("not valid JSON"), "{msg}");
    }

    #[test]
    fn load_file_reports_what_check_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, r#"{"port": 0, "device": {"name": "cam"}}"#).unwrap();

        let err = load_file::<Typed>(&path).err().unwrap();

        assert!(matches!(err, ConfigError::Rejected { .. }), "{err:?}");
        assert!(err.to_string().contains("port must not be 0"), "{err}");
    }

    #[test]
    fn load_file_read_error_when_path_is_a_directory() {
        // As `read_file_value_read_error_when_path_is_a_directory`: a directory
        // is the portable way to force a read error other than NotFound.
        let dir = tempfile::tempdir().unwrap();

        let err = load_file::<Typed>(dir.path()).err().unwrap();

        assert!(matches!(err, ConfigError::Read { .. }), "{err:?}");
    }

    #[test]
    fn materialize_honors_rfc6901_escaped_tokens() {
        // `~1` decodes to `/`, so the pointer addresses a key literally named "a/b".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");

        let out = materialize_identity::<Value>(&path, &json!({}), &["/a~1b/unique_id"]).unwrap();

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
    fn insert_identity_refuses_an_empty_pointer() {
        // An empty pointer has no tokens, so the value is left untouched.
        let mut v = json!({ "a": 1 });
        assert!(!insert_identity(&mut v, &json!({}), "", json!("x")));
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

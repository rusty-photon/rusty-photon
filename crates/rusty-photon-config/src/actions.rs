//! The `config.get` / `config.apply` / `config.schema` action protocol.
//!
//! ASCOM Alpaca drivers expose their configuration over HTTP as three vendor
//! [`Action`]s. This module holds the **driver-agnostic** machinery — the wire
//! envelopes, the layer-aware persist, secret redaction, the effective-config
//! diff, and the JSON-Schema generation — behind a single [`ConfigurableDriver`]
//! trait that each driver implements for its own `Config` type. The cross-driver
//! protocol and editability tiers are documented in
//! [`docs/services/config-actions.md`] and the plan
//! [`docs/plans/archive/config-actions.md`].
//!
//! The generic functions return plain values / [`ConfigError`]-flavoured errors;
//! the per-driver `device.rs` wraps those into `ascom_alpaca::ASCOMResult`, so
//! this crate stays free of an `ascom-alpaca` dependency.
//!
//! [`Action`]: https://ascom-standards.org/api/
//! [`docs/services/config-actions.md`]: ../../../docs/services/config-actions.md
//! [`docs/plans/archive/config-actions.md`]: ../../../docs/plans/archive/config-actions.md

use std::collections::BTreeSet;
use std::path::Path;

use schemars::{schema_for, JsonSchema};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{read_file_value, save, ConfigError};

/// Sentinel emitted by `config.get` in place of a redacted secret, and
/// recognised by `config.apply` as "leave this secret unchanged".
pub const REDACTED: &str = "********";

/// The three config actions. Drivers advertise these via `supported_actions()`
/// and dispatch on them in `action()`.
///
/// The per-variant strings are the ASCOM Alpaca vendor Action names on the
/// wire, so each is pinned explicitly rather than derived from its variant
/// name; matching is exact — case-sensitive and untrimmed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
pub enum ConfigAction {
    #[strum(serialize = "config.get")]
    Get,
    #[strum(serialize = "config.apply")]
    Apply,
    #[strum(serialize = "config.schema")]
    Schema,
}

impl ConfigAction {
    /// The action's wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.into()
    }

    /// The action a wire name denotes, or `None` if it denotes none.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        name.parse().ok()
    }
}

/// How a driver's persisted config changes take effect: via in-process reload
/// (the default) or only on the next process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDisposition {
    /// Changed fields apply via in-process reload: [`config_apply`] classifies
    /// them into `reload[]` and returns `status:"applying"`; the driver fires
    /// its `ReloadSignal` after the response flushes.
    Reload,
    /// The process has no in-process reload (e.g. rp, which runs under
    /// `ServiceRunner::run`): changed fields are classified into
    /// `restart_required[]` and `status` stays `"ok"` — the persisted file
    /// takes effect on the next process start.
    Restart,
}

/// Per-driver configuration metadata.
///
/// All invariant protocol logic lives in the free functions below
/// ([`config_get`], [`config_apply`], [`config_schema`]), generic over
/// this trait; an implementor supplies only what *varies* per driver —
/// its `Config` type, validation, secrets, and editability tiers.
pub trait ConfigurableDriver {
    /// The driver's config type. (No `Default` bound: `config_apply` seeds its
    /// file-read fallback from the running config, so drivers whose config has
    /// mandatory fields — e.g. sky-survey-camera's optics — need not invent one.)
    type Config: Serialize + DeserializeOwned + JsonSchema + Send + Sync;
    /// The driver's CLI-override carrier (`()` if the driver has no
    /// overrides). Bound `Sync` so the generic action futures, which borrow
    /// the shared config context, stay `Send`.
    type Overrides: Sync;

    /// Trim / canonicalize a submitted config in place before validation and
    /// persist (e.g. trim a whitespace-padded `serial.port`).
    fn normalize(config: &mut Self::Config);

    /// Domain validation. An empty result means valid. Paths are dotted
    /// (`"serial.port"`) so the UI can render each error next to its field.
    fn validate(config: &Self::Config) -> Vec<FieldError>;

    /// RFC-6901 JSON pointers to secret leaves, redacted by `config.get` and
    /// carried forward (rather than overwritten with the sentinel) by
    /// `config.apply`. Empty slice if the driver stores no secrets.
    ///
    /// A pointer segment may be the wildcard `*`, matching every element of
    /// the array (or every value of the object) at that position — e.g.
    /// `/equipment/cameras/*/auth/password` names that leaf in every camera
    /// entry. See [`expand_secret_pointer`] for the expansion semantics;
    /// on persist, array elements pair with their stored prior by `id`,
    /// not by index.
    fn secret_pointers() -> &'static [&'static str];

    /// Dotted paths currently pinned by a CLI override; surfaced in
    /// `config.get`'s `overrides[]` so the UI renders them disabled, and skipped
    /// by `config.apply` so a transient override is never baked into the file.
    fn override_paths(overrides: &Self::Overrides) -> Vec<String>;

    /// Apply CLI overrides onto a config in place, producing the *effective*
    /// config a reloaded server would run.
    fn apply_overrides(config: &mut Self::Config, overrides: &Self::Overrides);

    /// Identity fields (e.g. a device `unique_id`) — read-only by default in the
    /// UI behind an explicit "unlock to edit" escape hatch.
    #[must_use]
    fn locked_paths() -> &'static [&'static str] {
        &[]
    }

    /// Hard read-only fields the UI must never let the user edit (e.g. a
    /// `server.port` the BFF could not follow across a rebind).
    #[must_use]
    fn read_only_paths() -> &'static [&'static str] {
        &[]
    }

    /// How this driver's persisted changes take effect. The default —
    /// [`ApplyDisposition::Reload`] — matches the Alpaca drivers, which run
    /// under `ServiceRunner::run_with_reload`; services with no in-process
    /// reload (rp) override to [`ApplyDisposition::Restart`].
    #[must_use]
    fn apply_disposition() -> ApplyDisposition {
        ApplyDisposition::Reload
    }
}

/// A single field-level validation error (`config.apply` `status:"invalid"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub path: String,
    pub msg: String,
}

/// `config.get` response body: the effective config (secrets redacted) plus the
/// dotted paths pinned by a CLI override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigGetResponse {
    pub config: Value,
    pub overrides: Vec<String>,
}

/// `config.schema` response body: a JSON Schema for the driver's config plus the
/// editability tiers (which the schema alone cannot express) the UI renders from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSchemaResponse {
    /// JSON Schema (schemars) describing the config's shape and field types.
    pub schema: Value,
    /// Dotted paths that are identity/locked (read-only behind an unlock hatch).
    pub locked_fields: Vec<String>,
    /// Dotted paths that are hard read-only (never editable from the UI).
    pub read_only_fields: Vec<String>,
}

/// Outcome of a `config.apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplyStatus {
    /// Persisted and an in-process reload is in flight.
    Applying,
    /// Persisted; no reload is in flight — nothing changed, or every change
    /// is `restart_required` ([`ApplyDisposition::Restart`] drivers).
    Ok,
    /// Validation failed; the file is unchanged.
    Invalid,
}

/// `config.apply` response body. Classification arrays are always present for a
/// stable shape; `persisted_to` / `errors` appear only when relevant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigApplyResponse {
    pub status: ApplyStatus,
    /// Took effect live, no reload. Unused while every changed field reloads.
    pub applied: Vec<String>,
    /// Applied via in-process reload ([`ApplyDisposition::Reload`] drivers).
    pub reload: Vec<String>,
    /// Persisted, but takes effect only on the next process start
    /// ([`ApplyDisposition::Restart`] services, e.g. rp — no in-process
    /// reload). Until that restart, re-applying the same value keeps listing
    /// the path here: it still differs from the running process.
    pub restart_required: Vec<String>,
    /// Submitted but not persisted because the field is CLI-override-pinned.
    pub skipped_override: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<FieldError>,
}

impl ConfigApplyResponse {
    /// Build the validation-failure response (`status:"invalid"`, file unchanged).
    #[must_use]
    pub const fn invalid(errors: Vec<FieldError>) -> Self {
        Self {
            status: ApplyStatus::Invalid,
            applied: Vec::new(),
            reload: Vec::new(),
            restart_required: Vec::new(),
            skipped_override: Vec::new(),
            persisted_to: None,
            errors,
        }
    }
}

/// Error from [`config_apply`] that the caller maps to an ASCOM error.
///
/// Validation failures are *not* errors here — they come back as
/// `Ok(ConfigApplyResponse)` with `status:"invalid"` (an HTTP-200 domain
/// error, file untouched).
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The submitted `Parameters` did not parse into the driver's `Config`.
    #[error("invalid config JSON: {0}")]
    Parse(serde_json::Error),
    /// The on-disk config file is present but unreadable / corrupt.
    #[error("{0}")]
    ReadFile(ConfigError),
    /// Persisting the new config failed.
    #[error("failed to persist config: {0}")]
    Persist(std::io::Error),
    /// An internal (de)serialization step failed — effectively a bug.
    #[error("internal serialization error: {0}")]
    Serialize(serde_json::Error),
}

/// `config.get`: serialize the effective config, redact secrets, and attach the
/// CLI-override-pinned paths.
///
/// # Errors
///
/// Returns the `serde_json` error if the effective config fails to
/// serialize — an internal inconsistency, not an input failure.
pub fn config_get<D: ConfigurableDriver>(
    effective: &D::Config,
    overrides: &D::Overrides,
) -> Result<ConfigGetResponse, serde_json::Error> {
    let mut config = serde_json::to_value(effective)?;
    redact_value(&mut config, D::secret_pointers());
    Ok(ConfigGetResponse {
        config,
        overrides: D::override_paths(overrides),
    })
}

/// `config.schema`: a JSON Schema for the driver's config plus its
/// editability tiers.
///
/// The schema shapes the form; the tier lists gate which fields the UI
/// lets the user edit (JSON Schema cannot express "identity" /
/// "read-only").
#[must_use]
pub fn config_schema<D: ConfigurableDriver>() -> ConfigSchemaResponse {
    let schema = schema_for!(D::Config);
    ConfigSchemaResponse {
        schema: serde_json::to_value(&schema).unwrap_or(Value::Null),
        locked_fields: D::locked_paths().iter().map(|s| (*s).to_string()).collect(),
        read_only_fields: D::read_only_paths()
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    }
}

/// `config.apply`: parse → normalize → validate → layer-aware persist → classify.
///
/// Returns `Ok(ConfigApplyResponse)` for both success (`applying`/`ok`) and
/// validation failure (`invalid`, file untouched). The changed effective-config
/// paths are classified per [`ConfigurableDriver::apply_disposition`]:
/// [`ApplyDisposition::Reload`] lists them in `reload` — when non-empty
/// (`status:"applying"`) the caller fires the in-process reload **after**
/// flushing the response — while [`ApplyDisposition::Restart`] lists them in
/// `restart_required` with `status:"ok"` (persisted; takes effect on the next
/// process start). Hard failures (bad JSON, corrupt file, persist error) come
/// back as [`ApplyError`].
///
/// # Errors
///
/// Returns an [`ApplyError`]: the submission did not parse, the on-disk
/// file is unreadable or corrupt, persisting failed, or an internal
/// (de)serialization step failed.
pub fn config_apply<D: ConfigurableDriver>(
    path: &Path,
    overrides: &D::Overrides,
    effective_before: &D::Config,
    submitted_json: &str,
) -> Result<ConfigApplyResponse, ApplyError> {
    let mut submitted: D::Config =
        serde_json::from_str(submitted_json).map_err(ApplyError::Parse)?;
    D::normalize(&mut submitted);

    let mut errors = D::validate(&submitted);

    // A present-but-corrupt file is surfaced (not silently treated as default,
    // which would overwrite and lose its contents on the layer-aware persist).
    // The seed is the running config: a `config.apply` on a live driver always
    // has a config file on disk (loaded at startup), so the seed only matters in
    // the shouldn't-happen case where the file vanished mid-run.
    let running = serde_json::to_value(effective_before).map_err(ApplyError::Serialize)?;
    let file_current = read_file_value(path, &running).map_err(ApplyError::ReadFile)?;

    let submitted_value = serde_json::to_value(&submitted).map_err(ApplyError::Serialize)?;

    // The redaction sentinel means "keep the stored secret unchanged"; with
    // nothing stored there is nothing to keep, so honouring it would bake the
    // sentinel in as the real secret. Reject as a domain error. Wildcard
    // patterns expand against the *submitted* value, and each concrete
    // pointer pairs with its stored prior by identity — see
    // [`prior_secret_pointer`].
    for pattern in D::secret_pointers() {
        for ptr in expand_secret_pointer(pattern, &submitted_value) {
            if sentinel_at(&submitted_value, &ptr)
                && prior_secret(pattern, &ptr, &submitted_value, &file_current).is_none()
            {
                errors.push(FieldError {
                    path: pointer_to_dotted(&ptr),
                    msg:
                        "cannot keep an unchanged secret when none is stored; provide the secret value"
                            .to_string(),
                });
            }
        }
    }

    if !errors.is_empty() {
        return Ok(ConfigApplyResponse::invalid(errors));
    }

    let pinned = D::override_paths(overrides);
    let (to_persist, skipped) = build_persist_value(
        &submitted_value,
        &file_current,
        &pinned,
        D::secret_pointers(),
    );

    // Classify what would change in the running (effective) config: the persisted
    // value with CLI overrides re-applied on top.
    let mut effective_after: D::Config =
        serde_json::from_value(to_persist.clone()).map_err(ApplyError::Serialize)?;
    D::apply_overrides(&mut effective_after, overrides);
    let new_effective = serde_json::to_value(&effective_after).map_err(ApplyError::Serialize)?;
    let changed = diff_paths(&running, &new_effective);

    save(path, &to_persist).map_err(ApplyError::Persist)?;

    // Classify the changed effective paths per the driver's disposition:
    // in-process reload (`reload[]`, `applying` when non-empty) or process
    // restart (`restart_required[]`, status stays `ok` — persisted, takes
    // effect on the next start).
    let (status, reload, restart_required) = match D::apply_disposition() {
        ApplyDisposition::Reload => {
            let status = if changed.is_empty() {
                ApplyStatus::Ok
            } else {
                ApplyStatus::Applying
            };
            (status, changed, Vec::new())
        }
        ApplyDisposition::Restart => (ApplyStatus::Ok, Vec::new(), changed),
    };

    Ok(ConfigApplyResponse {
        status,
        applied: Vec::new(),
        reload,
        restart_required,
        skipped_override: skipped,
        persisted_to: Some(path.display().to_string()),
        errors: Vec::new(),
    })
}

// --- pure helpers (driver-agnostic) --------------------------------------------

/// Expand a secret-pointer *pattern* against a concrete `Value` into the
/// RFC-6901 pointers that resolve in that value.
///
/// A pattern is an RFC-6901 pointer whose segments may be the wildcard `*`,
/// meaning "every element of the array (or every value of the object) at this
/// position" — e.g. `/equipment/cameras/*/auth/password` names that leaf in
/// every camera entry that has one. Non-`*` segments resolve with exact
/// [`Value::pointer`] semantics (RFC-6901 `~0`/`~1` escapes, strict array
/// indices), so a pattern without `*` expands to itself when the pointer
/// resolves and to nothing otherwise.
///
/// Expansion names leaves in **one** value; pairing a round-tripped sentinel
/// with its stored prior across the submitted and on-disk values is
/// `prior_secret_pointer`'s job (array elements pair by `id`, not index).
#[must_use]
pub fn expand_secret_pointer(pattern: &str, value: &Value) -> Vec<String> {
    if pattern.is_empty() {
        // `Value::pointer("")` resolves to the root.
        return vec![String::new()];
    }
    if !pattern.starts_with('/') {
        // Matches `Value::pointer`: a non-empty pointer must start with `/`.
        return Vec::new();
    }
    let segments: Vec<&str> = pattern.split('/').skip(1).collect();
    let mut matches = Vec::new();
    expand_into(value, &segments, String::new(), &mut matches);
    matches
}

fn expand_into(current: &Value, segments: &[&str], prefix: String, out: &mut Vec<String>) {
    let Some((segment, rest)) = segments.split_first() else {
        out.push(prefix);
        return;
    };
    if *segment == "*" {
        match current {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    expand_into(item, rest, format!("{prefix}/{index}"), out);
                }
            }
            Value::Object(map) => {
                for (key, child) in map {
                    let token = escape_pointer_token(key);
                    expand_into(child, rest, format!("{prefix}/{token}"), out);
                }
            }
            // A wildcard over a scalar / null matches nothing.
            _ => {}
        }
    } else if let Some(child) = current.pointer(&format!("/{segment}")) {
        // Delegate single-segment resolution to `Value::pointer` so escapes
        // and array-index strictness match serde_json exactly; keep the
        // original (escaped) segment in the emitted pointer string.
        expand_into(child, rest, format!("{prefix}/{segment}"), out);
    }
}

/// Escape a map key for embedding in an RFC-6901 pointer (`~` → `~0`, then
/// `/` → `~1`).
fn escape_pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Redact each secret leaf in a serialized config `Value`, in place. Entries
/// in `secret_pointers` may be wildcard patterns (see
/// [`expand_secret_pointer`]).
fn redact_value(value: &mut Value, secret_pointers: &[&str]) {
    for pattern in secret_pointers {
        for ptr in expand_secret_pointer(pattern, value) {
            if let Some(secret) = value.pointer_mut(&ptr) {
                if secret.is_string() {
                    *secret = Value::String(REDACTED.to_string());
                }
            }
        }
    }
}

/// Whether `value` carries the redaction sentinel at `pointer`.
fn sentinel_at(value: &Value, pointer: &str) -> bool {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .is_some_and(|s| s == REDACTED)
}

/// The stored secret paired with a round-tripped sentinel: resolve the
/// file-side pointer for `concrete` (see [`prior_secret_pointer`]) and return
/// the string stored there. `None` means "nothing stored to keep" — the
/// caller rejects the sentinel as a domain error rather than persisting it.
fn prior_secret(
    pattern: &str,
    concrete: &str,
    submitted: &Value,
    file_current: &Value,
) -> Option<Value> {
    let file_pointer = prior_secret_pointer(pattern, concrete, submitted, file_current)?;
    file_current
        .pointer(&file_pointer)
        .filter(|stored| stored.is_string())
        .cloned()
}

/// Map a concrete submitted-side secret pointer onto the file-side pointer
/// naming the same **logical** leaf. Literal segments (and `*` matches over
/// object keys) resolve to the same token on both sides; a `*` match over an
/// **array** re-locates the file element by identity ([`paired_file_index`])
/// instead of by index, so a submission that removes or reorders sibling
/// entries between `config.get` and `config.apply` never pairs a sentinel
/// with another entry's stored secret. `None` when the file has no
/// counterpart for the submitted element.
fn prior_secret_pointer(
    pattern: &str,
    concrete: &str,
    submitted: &Value,
    file_current: &Value,
) -> Option<String> {
    let pattern_segments: Vec<&str> = pattern.split('/').skip(1).collect();
    let concrete_segments: Vec<&str> = concrete.split('/').skip(1).collect();
    // `expand_secret_pointer` emits one concrete segment per pattern segment.
    if pattern_segments.len() != concrete_segments.len() {
        return None;
    }
    let mut submitted_node = submitted;
    let mut file_node = file_current;
    let mut file_pointer = String::new();
    for (pattern_segment, concrete_segment) in pattern_segments.iter().zip(&concrete_segments) {
        let file_segment = match (*pattern_segment, submitted_node, file_node) {
            ("*", Value::Array(submitted_items), Value::Array(file_items)) => {
                let index: usize = concrete_segment.parse().ok()?;
                paired_file_index(submitted_items, file_items, index)?.to_string()
            }
            _ => (*concrete_segment).to_string(),
        };
        submitted_node = submitted_node.pointer(&format!("/{concrete_segment}"))?;
        file_node = file_node.pointer(&format!("/{file_segment}"))?;
        file_pointer.push('/');
        file_pointer.push_str(&file_segment);
    }
    Some(file_pointer)
}

/// An array element's identity for secret pairing: its `"id"` member.
fn element_id(element: &Value) -> Option<&str> {
    element.get("id").and_then(Value::as_str)
}

/// The file-side index paired with `submitted[index]` for a secret-pointer
/// `*` over an array. Elements pair by id: the file element with the same id,
/// wherever it moved. A submitted id absent from the file pairs positionally
/// only when it is the array's lone id change (same length, every other
/// position's id equal on both sides) — a renamed entry keeps its stored
/// secret, while a removal or reorder alongside the unknown id yields `None`
/// (no prior, a loud apply error) rather than a silent cross-pairing.
/// Elements without a string id pair positionally.
fn paired_file_index(submitted: &[Value], file: &[Value], index: usize) -> Option<usize> {
    let Some(id) = submitted.get(index).and_then(element_id) else {
        return (index < file.len()).then_some(index);
    };
    if let Some(found) = file
        .iter()
        .position(|element| element_id(element) == Some(id))
    {
        return Some(found);
    }
    let lone_id_change = submitted.len() == file.len()
        && submitted.iter().zip(file).enumerate().all(
            |(position, (submitted_element, file_element))| {
                position == index || element_id(submitted_element) == element_id(file_element)
            },
        );
    lone_id_change.then_some(index)
}

/// Build the value to persist from a validated `submitted` config value:
/// CLI-override-pinned fields are written through from `file_current` (so a
/// transient `--port` is never baked into the file), and a round-tripped redacted
/// secret keeps the file's existing value. Returns the value to write plus the
/// dotted paths that were skipped because they are override-pinned.
fn build_persist_value(
    submitted: &Value,
    file_current: &Value,
    pinned_paths: &[String],
    secret_pointers: &[&str],
) -> (Value, Vec<String>) {
    let mut to_write = submitted.clone();

    for path in pinned_paths {
        let pointer = dotted_to_pointer(path);
        match file_current.pointer(&pointer).cloned() {
            // Present on disk: write the file's prior value through, discarding the
            // submitted (override-shadowed) value.
            Some(file_val) => {
                if let Some(slot) = to_write.pointer_mut(&pointer) {
                    *slot = file_val;
                }
            }
            // Absent on disk (e.g. a `#[serde(default)]` field left at its default,
            // like `switch.enabled`): the file's prior state is "unset", so drop the
            // field from the persist payload — otherwise a transient CLI override
            // (or a disabled-field edit) to a pinned field would still be baked into
            // the file.
            None => remove_at_pointer(&mut to_write, &pointer),
        }
    }

    // Wildcard patterns expand against the to-write value; each concrete
    // sentinel is restored from the file entry it pairs with by identity
    // (see `prior_secret_pointer` — array elements pair by id, not index).
    for pattern in secret_pointers {
        for secret_pointer in expand_secret_pointer(pattern, &to_write) {
            if !sentinel_at(&to_write, &secret_pointer) {
                continue;
            }
            if let Some(file_secret) =
                prior_secret(pattern, &secret_pointer, &to_write, file_current)
            {
                if let Some(slot) = to_write.pointer_mut(&secret_pointer) {
                    *slot = file_secret;
                }
            }
        }
    }

    (to_write, pinned_paths.to_vec())
}

/// Remove the leaf at a JSON pointer from `value`, if its parent is an object.
/// Used to drop an override-pinned field that is absent from the on-disk config so
/// `config.apply` persists the file's prior "unset" state rather than the submitted
/// (override-shadowed) value.
fn remove_at_pointer(value: &mut Value, pointer: &str) {
    if let Some((parent, key)) = pointer.rsplit_once('/') {
        if let Some(Value::Object(map)) = value.pointer_mut(parent) {
            map.remove(key);
        }
    }
}

/// Dotted JSON paths whose leaf values differ between `before` and `after`.
/// Objects recurse; everything else compares by equality.
fn diff_paths(before: &Value, after: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    diff_into(before, after, "", &mut paths);
    paths
}

fn diff_into(before: &Value, after: &Value, prefix: &str, out: &mut Vec<String>) {
    match (before, after) {
        (Value::Object(b), Value::Object(a)) => {
            let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
            for key in keys {
                let child = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match (b.get(key), a.get(key)) {
                    (Some(bv), Some(av)) => diff_into(bv, av, &child, out),
                    _ => out.push(child),
                }
            }
        }
        _ => {
            if before != after {
                out.push(prefix.to_string());
            }
        }
    }
}

/// Convert a dotted path (`serial.port`) to an RFC-6901 JSON pointer (`/serial/port`).
fn dotted_to_pointer(dotted: &str) -> String {
    let mut pointer = String::with_capacity(dotted.len().saturating_add(1));
    pointer.push('/');
    pointer.push_str(&dotted.replace('.', "/"));
    pointer
}

/// Convert an RFC-6901 JSON pointer (`/server/auth/password_hash`) to a dotted
/// path (`server.auth.password_hash`).
fn pointer_to_dotted(pointer: &str) -> String {
    pointer.trim_start_matches('/').replace('/', ".")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use strum::VariantArray;

    // --- a minimal fake driver covering every protocol feature ---------------

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct Serial {
        port: String,
        baud_rate: u32,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct Auth {
        username: String,
        password_hash: String,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct Server {
        port: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        auth: Option<Auth>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct Device {
        unique_id: String,
        name: String,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct TestConfig {
        serial: Serial,
        server: Server,
        device: Device,
    }

    #[derive(Clone, Default)]
    struct TestOverrides {
        serial_port: Option<String>,
    }

    struct TestDriver;

    impl ConfigurableDriver for TestDriver {
        type Config = TestConfig;
        type Overrides = TestOverrides;

        fn normalize(config: &mut Self::Config) {
            let trimmed = config.serial.port.trim();
            if trimmed.len() != config.serial.port.len() {
                config.serial.port = trimmed.to_string();
            }
        }

        fn validate(config: &Self::Config) -> Vec<FieldError> {
            let mut errors = Vec::new();
            if config.serial.port.trim().is_empty() {
                errors.push(FieldError {
                    path: "serial.port".to_string(),
                    msg: "must not be empty".to_string(),
                });
            }
            if config.serial.baud_rate == 0 {
                errors.push(FieldError {
                    path: "serial.baud_rate".to_string(),
                    msg: "must be greater than 0".to_string(),
                });
            }
            errors
        }

        fn secret_pointers() -> &'static [&'static str] {
            &["/server/auth/password_hash"]
        }

        fn override_paths(overrides: &Self::Overrides) -> Vec<String> {
            let mut paths = Vec::new();
            if overrides.serial_port.is_some() {
                paths.push("serial.port".to_string());
            }
            paths
        }

        fn apply_overrides(config: &mut Self::Config, overrides: &Self::Overrides) {
            if let Some(port) = &overrides.serial_port {
                config.serial.port = port.clone();
            }
        }

        fn locked_paths() -> &'static [&'static str] {
            &["device.unique_id"]
        }

        fn read_only_paths() -> &'static [&'static str] {
            &["server.port"]
        }
    }

    fn valid_config() -> TestConfig {
        TestConfig {
            serial: Serial {
                port: "/dev/ttyACM0".to_string(),
                baud_rate: 115_200,
            },
            server: Server {
                port: 11119,
                auth: None,
            },
            device: Device {
                unique_id: "id-1".to_string(),
                name: "Test".to_string(),
            },
        }
    }

    #[test]
    fn config_action_names_round_trip() {
        for &action in ConfigAction::VARIANTS {
            assert_eq!(ConfigAction::from_name(action.name()), Some(action));
        }
        assert_eq!(ConfigAction::from_name("config.nope"), None);
        // The three names are the Alpaca vendor Action strings on the wire.
        assert_eq!(ConfigAction::Get.name(), "config.get");
        assert_eq!(ConfigAction::Apply.name(), "config.apply");
        assert_eq!(ConfigAction::Schema.name(), "config.schema");
    }

    #[test]
    fn config_action_displays_its_wire_name() {
        // `Display` renders the same wire string `name()` returns, so an action
        // interpolated into a log line or an error message stays recognisable.
        assert_eq!(ConfigAction::Get.to_string(), "config.get");
        assert_eq!(ConfigAction::Apply.to_string(), "config.apply");
        assert_eq!(ConfigAction::Schema.to_string(), "config.schema");
    }

    #[test]
    fn config_action_all_lists_the_three_variants_in_declaration_order() {
        assert_eq!(
            ConfigAction::VARIANTS,
            [ConfigAction::Get, ConfigAction::Apply, ConfigAction::Schema]
        );
    }

    #[test]
    fn config_action_from_name_rejects_near_misses() {
        // Matching is exact: case-sensitive, no trimming, no bare variant names.
        for near_miss in [
            "config.list",
            "Get",
            "get",
            "Config.Get",
            "CONFIG.GET",
            "config.Get",
            "config.get ",
            " config.get",
            "config_get",
            "configget",
            "",
        ] {
            assert_eq!(
                ConfigAction::from_name(near_miss),
                None,
                "{near_miss:?} must not parse"
            );
        }
    }

    #[test]
    fn config_get_redacts_secret_and_lists_overrides() {
        let mut cfg = valid_config();
        cfg.server.auth = Some(Auth {
            username: "obs".to_string(),
            password_hash: "$argon2id$real".to_string(),
        });
        let overrides = TestOverrides {
            serial_port: Some("/dev/ttyACM9".to_string()),
        };
        let resp = config_get::<TestDriver>(&cfg, &overrides).unwrap();
        assert_eq!(
            resp.config
                .pointer("/server/auth/password_hash")
                .and_then(Value::as_str),
            Some(REDACTED)
        );
        // Non-secret fields are untouched.
        assert_eq!(
            resp.config.pointer("/serial/port").and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        assert_eq!(resp.overrides, vec!["serial.port".to_string()]);
    }

    #[test]
    fn config_get_without_secret_is_a_noop() {
        let resp = config_get::<TestDriver>(&valid_config(), &TestOverrides::default()).unwrap();
        assert!(resp
            .config
            .pointer("/server/auth")
            .is_none_or(Value::is_null));
        assert_eq!(resp.overrides, Vec::<String>::new());
    }

    #[test]
    fn config_schema_carries_tiers_and_describes_fields() {
        let resp = config_schema::<TestDriver>();
        assert_eq!(resp.locked_fields, vec!["device.unique_id".to_string()]);
        assert_eq!(resp.read_only_fields, vec!["server.port".to_string()]);
        // The schema names the top-level config sections.
        let props = resp
            .schema
            .pointer("/properties")
            .and_then(Value::as_object)
            .expect("schema has properties");
        assert!(props.contains_key("serial"));
        assert!(props.contains_key("server"));
        assert!(props.contains_key("device"));
    }

    #[test]
    fn config_apply_persists_and_reports_changed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        save(&path, &serde_json::to_value(&before).unwrap()).unwrap();

        let mut changed = valid_config();
        changed.serial.baud_rate = 9600;
        let submitted = serde_json::to_string(&changed).unwrap();

        let resp =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        assert_eq!(resp.status, ApplyStatus::Applying);
        assert_eq!(resp.reload, vec!["serial.baud_rate".to_string()]);
        assert_eq!(resp.persisted_to, Some(path.display().to_string()));

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk.pointer("/serial/baud_rate").and_then(Value::as_u64),
            Some(9600)
        );
    }

    #[test]
    fn config_apply_no_change_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        save(&path, &serde_json::to_value(&before).unwrap()).unwrap();

        let submitted = serde_json::to_string(&valid_config()).unwrap();
        let resp =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        assert_eq!(resp.status, ApplyStatus::Ok);
        assert_eq!(resp.reload, Vec::<String>::new());
    }

    #[test]
    fn config_apply_invalid_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        let original = serde_json::to_value(&before).unwrap();
        save(&path, &original).unwrap();
        let on_disk_before = std::fs::read_to_string(&path).unwrap();

        let mut bad = valid_config();
        bad.serial.baud_rate = 0;
        let submitted = serde_json::to_string(&bad).unwrap();

        let resp =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        assert_eq!(resp.status, ApplyStatus::Invalid);
        assert_eq!(
            resp.errors,
            vec![FieldError {
                path: "serial.baud_rate".to_string(),
                msg: "must be greater than 0".to_string(),
            }]
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), on_disk_before);
    }

    #[test]
    fn config_apply_rejects_non_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let err =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &valid_config(), "nope")
                .unwrap_err();
        assert!(matches!(err, ApplyError::Parse(_)), "{err:?}");
    }

    #[test]
    fn config_apply_normalizes_before_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        save(&path, &serde_json::to_value(&before).unwrap()).unwrap();

        let mut padded = valid_config();
        padded.serial.port = "  /dev/ttyACM0  ".to_string();
        let submitted = serde_json::to_string(&padded).unwrap();

        let resp =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        // Trimmed value equals the running one, so nothing changed.
        assert_eq!(resp.status, ApplyStatus::Ok);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk.pointer("/serial/port").and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
    }

    #[test]
    fn config_apply_skips_override_pinned_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // File has its own serial.port.
        let mut file_cfg = valid_config();
        file_cfg.serial.port = "/dev/ttyACM0".to_string();
        save(&path, &serde_json::to_value(&file_cfg).unwrap()).unwrap();

        let overrides = TestOverrides {
            serial_port: Some("/dev/ttyACM9".to_string()),
        };
        // Effective (running) config reflects the override.
        let mut running = file_cfg;
        TestDriver::apply_overrides(&mut running, &overrides);

        // Submission carries the override value (as config.get would have shown)
        // plus a real edit elsewhere.
        let mut submitted_cfg = running.clone();
        submitted_cfg.device.name = "Renamed".to_string();
        let submitted = serde_json::to_string(&submitted_cfg).unwrap();

        let resp = config_apply::<TestDriver>(&path, &overrides, &running, &submitted).unwrap();

        assert_eq!(resp.skipped_override, vec!["serial.port".to_string()]);
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The pinned field keeps the FILE's value, not the override value.
        assert_eq!(
            on_disk.pointer("/serial/port").and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        // The non-pinned edit is persisted.
        assert_eq!(
            on_disk.pointer("/device/name").and_then(Value::as_str),
            Some("Renamed")
        );
    }

    #[test]
    fn build_persist_value_drops_pinned_field_absent_from_file() {
        // A pinned field omitted from the on-disk file (e.g. a `#[serde(default)]`
        // bool at its default) must NOT be persisted from the submitted value — the
        // file keeps its prior "unset" state. Regression for the override-pinned-
        // but-absent persist leak.
        let submitted = serde_json::json!({ "switch": { "enabled": false, "name": "sw" } });
        let file_current = serde_json::json!({ "switch": { "name": "sw" } });
        let (to_write, skipped) = build_persist_value(
            &submitted,
            &file_current,
            &["switch.enabled".to_string()],
            &[],
        );
        assert!(
            to_write.pointer("/switch/enabled").is_none(),
            "pinned field absent on disk must not be persisted: {to_write}"
        );
        assert_eq!(
            to_write.pointer("/switch/name").and_then(Value::as_str),
            Some("sw")
        );
        assert_eq!(skipped, vec!["switch.enabled".to_string()]);
    }

    #[test]
    fn build_persist_value_writes_through_pinned_field_present_on_file() {
        // Present on disk: the file's prior value wins over the submitted value.
        let submitted = serde_json::json!({ "server": { "port": 9999 } });
        let file_current = serde_json::json!({ "server": { "port": 11111 } });
        let (to_write, _) =
            build_persist_value(&submitted, &file_current, &["server.port".to_string()], &[]);
        assert_eq!(
            to_write.pointer("/server/port").and_then(Value::as_u64),
            Some(11111)
        );
    }

    #[test]
    fn config_apply_keeps_secret_when_sentinel_submitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let mut file_cfg = valid_config();
        file_cfg.server.auth = Some(Auth {
            username: "obs".to_string(),
            password_hash: "$argon2id$real".to_string(),
        });
        save(&path, &serde_json::to_value(&file_cfg).unwrap()).unwrap();

        // Submission round-trips the redaction sentinel.
        let mut submitted_cfg = file_cfg.clone();
        submitted_cfg.server.auth.as_mut().unwrap().password_hash = REDACTED.to_string();
        submitted_cfg.device.name = "Renamed".to_string();
        let submitted = serde_json::to_string(&submitted_cfg).unwrap();

        config_apply::<TestDriver>(&path, &TestOverrides::default(), &file_cfg, &submitted)
            .unwrap();

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The real hash is preserved, not overwritten with the sentinel.
        assert_eq!(
            on_disk
                .pointer("/server/auth/password_hash")
                .and_then(Value::as_str),
            Some("$argon2id$real")
        );
    }

    #[test]
    fn config_apply_rejects_sentinel_without_prior_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let file_cfg = valid_config(); // no stored secret
        save(&path, &serde_json::to_value(&file_cfg).unwrap()).unwrap();

        let mut submitted_cfg = valid_config();
        submitted_cfg.server.auth = Some(Auth {
            username: "obs".to_string(),
            password_hash: REDACTED.to_string(),
        });
        let submitted = serde_json::to_string(&submitted_cfg).unwrap();

        let resp =
            config_apply::<TestDriver>(&path, &TestOverrides::default(), &file_cfg, &submitted)
                .unwrap();

        assert_eq!(resp.status, ApplyStatus::Invalid);
        assert_eq!(
            resp.errors,
            vec![FieldError {
                path: "server.auth.password_hash".to_string(),
                msg:
                    "cannot keep an unchanged secret when none is stored; provide the secret value"
                        .to_string(),
            }]
        );
    }

    #[test]
    fn config_apply_surfaces_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        std::fs::write(&path, "{ not json").unwrap();

        let submitted = serde_json::to_string(&valid_config()).unwrap();
        let err = config_apply::<TestDriver>(
            &path,
            &TestOverrides::default(),
            &valid_config(),
            &submitted,
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::ReadFile(_)), "{err:?}");
    }

    #[test]
    fn diff_paths_reports_changed_leaves_only() {
        let before = json!({ "a": { "x": 1, "y": 2 }, "b": 3 });
        let after = json!({ "a": { "x": 1, "y": 9 }, "b": 3 });
        assert_eq!(diff_paths(&before, &after), vec!["a.y".to_string()]);
    }

    // --- wildcard secret pointers ---------------------------------------------

    #[test]
    fn expand_secret_pointer_over_array_elements() {
        let value = json!({
            "cameras": [
                { "id": "a", "auth": { "password": "p0" } },
                { "id": "b", "auth": { "password": "p1" } }
            ]
        });
        assert_eq!(
            expand_secret_pointer("/cameras/*/auth/password", &value),
            vec![
                "/cameras/0/auth/password".to_string(),
                "/cameras/1/auth/password".to_string(),
            ]
        );
    }

    #[test]
    fn expand_secret_pointer_skips_array_elements_missing_the_leaf() {
        // Element 1 has no auth block; only element 0 resolves.
        let value = json!({
            "cameras": [
                { "id": "a", "auth": { "password": "p0" } },
                { "id": "b", "auth": null },
                { "id": "c" }
            ]
        });
        assert_eq!(
            expand_secret_pointer("/cameras/*/auth/password", &value),
            vec!["/cameras/0/auth/password".to_string()]
        );
    }

    #[test]
    fn expand_secret_pointer_over_object_values() {
        let value = json!({
            "clients": {
                "alpha": { "password": "pa" },
                "beta": { "password": "pb" }
            }
        });
        // serde_json::Map preserves insertion order from the json! literal.
        assert_eq!(
            expand_secret_pointer("/clients/*/password", &value),
            vec![
                "/clients/alpha/password".to_string(),
                "/clients/beta/password".to_string(),
            ]
        );
    }

    #[test]
    fn expand_secret_pointer_escapes_object_keys() {
        // A key containing `/` must come back RFC-6901-escaped so the emitted
        // pointer resolves via `Value::pointer`.
        let value = json!({ "clients": { "a/b": { "password": "p" } } });
        let expanded = expand_secret_pointer("/clients/*/password", &value);
        assert_eq!(expanded, vec!["/clients/a~1b/password".to_string()]);
        assert_eq!(
            value.pointer(&expanded[0]).and_then(Value::as_str),
            Some("p")
        );
    }

    #[test]
    fn expand_secret_pointer_missing_path_matches_nothing() {
        let value = json!({ "server": { "port": 1 } });
        assert_eq!(
            expand_secret_pointer("/cameras/*/auth/password", &value),
            Vec::<String>::new()
        );
    }

    #[test]
    fn expand_secret_pointer_wildcard_over_scalar_matches_nothing() {
        let value = json!({ "cameras": "oops" });
        assert_eq!(
            expand_secret_pointer("/cameras/*/password", &value),
            Vec::<String>::new()
        );
        let value = json!({ "cameras": null });
        assert_eq!(
            expand_secret_pointer("/cameras/*/password", &value),
            Vec::<String>::new()
        );
    }

    #[test]
    fn expand_secret_pointer_without_wildcard_is_exact() {
        // Present → expands to itself; absent → nothing (mirrors today's
        // exact-pointer no-op behaviour byte for byte).
        let value = json!({ "server": { "auth": { "password_hash": "h" } } });
        assert_eq!(
            expand_secret_pointer("/server/auth/password_hash", &value),
            vec!["/server/auth/password_hash".to_string()]
        );
        assert_eq!(
            expand_secret_pointer("/server/auth/missing", &value),
            Vec::<String>::new()
        );
    }

    #[test]
    fn expand_secret_pointer_invalid_or_root_pointer() {
        let value = json!({ "a": 1 });
        // Empty pattern resolves to the root, like `Value::pointer("")`.
        assert_eq!(expand_secret_pointer("", &value), vec![String::new()]);
        // A non-empty pointer must start with `/`, like `Value::pointer`.
        assert_eq!(expand_secret_pointer("a", &value), Vec::<String>::new());
    }

    // A driver whose secrets live inside an array, exercising wildcard
    // redaction, per-element carry-forward, and per-element sentinel rejection.

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
    struct FleetAuth {
        username: String,
        password: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
    struct FleetCamera {
        id: String,
        #[serde(default)]
        auth: Option<FleetAuth>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
    struct FleetConfig {
        cameras: Vec<FleetCamera>,
        name: String,
    }

    struct FleetDriver;

    impl ConfigurableDriver for FleetDriver {
        type Config = FleetConfig;
        type Overrides = ();

        fn normalize(_config: &mut Self::Config) {}

        fn validate(_config: &Self::Config) -> Vec<FieldError> {
            Vec::new()
        }

        fn secret_pointers() -> &'static [&'static str] {
            &["/cameras/*/auth/password"]
        }

        fn override_paths(_overrides: &Self::Overrides) -> Vec<String> {
            Vec::new()
        }

        fn apply_overrides(_config: &mut Self::Config, _overrides: &Self::Overrides) {}
    }

    fn fleet_config(passwords: &[Option<&str>]) -> FleetConfig {
        FleetConfig {
            cameras: passwords
                .iter()
                .enumerate()
                .map(|(i, pw)| FleetCamera {
                    id: format!("cam-{i}"),
                    auth: pw.map(|p| FleetAuth {
                        username: "obs".to_string(),
                        password: p.to_string(),
                    }),
                })
                .collect(),
            name: "fleet".to_string(),
        }
    }

    #[test]
    fn config_get_redacts_every_array_element_secret() {
        let cfg = fleet_config(&[Some("p0"), None, Some("p2")]);
        let resp = config_get::<FleetDriver>(&cfg, &()).unwrap();
        assert_eq!(
            resp.config
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some(REDACTED)
        );
        assert!(resp
            .config
            .pointer("/cameras/1/auth")
            .is_some_and(Value::is_null));
        assert_eq!(
            resp.config
                .pointer("/cameras/2/auth/password")
                .and_then(Value::as_str),
            Some(REDACTED)
        );
        // Non-secret leaves are untouched.
        assert_eq!(
            resp.config
                .pointer("/cameras/0/auth/username")
                .and_then(Value::as_str),
            Some("obs")
        );
    }

    #[test]
    fn config_apply_carries_forward_sentinel_per_array_element() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let file_cfg = fleet_config(&[Some("stored-0"), Some("stored-1")]);
        save(&path, &serde_json::to_value(&file_cfg).unwrap()).unwrap();

        // Element 0 round-trips the sentinel (keep); element 1 submits a new
        // real password (overwrite).
        let mut submitted_cfg = file_cfg.clone();
        submitted_cfg.cameras[0].auth.as_mut().unwrap().password = REDACTED.to_string();
        submitted_cfg.cameras[1].auth.as_mut().unwrap().password = "new-1".to_string();
        let submitted = serde_json::to_string(&submitted_cfg).unwrap();

        let resp = config_apply::<FleetDriver>(&path, &(), &file_cfg, &submitted).unwrap();
        assert_eq!(resp.errors, vec![]);

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("stored-0"),
            "sentinel at element 0 must keep the stored secret"
        );
        assert_eq!(
            on_disk
                .pointer("/cameras/1/auth/password")
                .and_then(Value::as_str),
            Some("new-1"),
            "a real submission at element 1 must overwrite"
        );
    }

    #[test]
    fn config_apply_rejects_sentinel_per_array_element_without_prior() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        // The file stores a secret for element 0 only.
        let file_cfg = fleet_config(&[Some("stored-0"), None]);
        save(&path, &serde_json::to_value(&file_cfg).unwrap()).unwrap();

        // Element 1 newly gains an auth block carrying the sentinel — there is
        // no stored secret to keep, so this is a per-element domain error.
        let mut submitted_cfg = file_cfg.clone();
        submitted_cfg.cameras[0].auth.as_mut().unwrap().password = REDACTED.to_string();
        submitted_cfg.cameras[1].auth = Some(FleetAuth {
            username: "obs".to_string(),
            password: REDACTED.to_string(),
        });
        let submitted = serde_json::to_string(&submitted_cfg).unwrap();

        let resp = config_apply::<FleetDriver>(&path, &(), &file_cfg, &submitted).unwrap();

        assert_eq!(resp.status, ApplyStatus::Invalid);
        assert_eq!(
            resp.errors,
            vec![FieldError {
                path: "cameras.1.auth.password".to_string(),
                msg:
                    "cannot keep an unchanged secret when none is stored; provide the secret value"
                        .to_string(),
            }],
            "only element 1 (no prior) errors; element 0's sentinel is fine"
        );
    }

    // --- identity pairing of array-element sentinels ---------------------------

    /// Run a fleet `config_apply` against a file seeded with `file_cfg` and
    /// return the response plus the file's JSON afterwards (unchanged when the
    /// apply is rejected).
    fn fleet_apply(file_cfg: &FleetConfig, submitted: &Value) -> (ConfigApplyResponse, Value) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        save(&path, &serde_json::to_value(file_cfg).unwrap()).unwrap();
        let resp =
            config_apply::<FleetDriver>(&path, &(), file_cfg, &submitted.to_string()).unwrap();
        let on_disk = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        (resp, on_disk)
    }

    /// The submitted shape a UI produces: the redacted `config.get` round-trip
    /// of `file_cfg` (every stored password replaced by the sentinel).
    fn redacted_round_trip(file_cfg: &FleetConfig) -> Value {
        let mut cfg = file_cfg.clone();
        for camera in &mut cfg.cameras {
            if let Some(auth) = &mut camera.auth {
                auth.password = REDACTED.to_string();
            }
        }
        serde_json::to_value(&cfg).unwrap()
    }

    #[test]
    fn config_apply_removing_the_first_camera_keeps_the_survivors_own_secret() {
        // The equipment-page delete flow: round-trip the redacted config with
        // entry 0 removed, shifting cam-1 down to index 0. Positional pairing
        // would persist cam-0's stored password into cam-1.
        let file_cfg = fleet_config(&[Some("stored-0"), Some("stored-1")]);
        let mut submitted = redacted_round_trip(&file_cfg);
        submitted["cameras"].as_array_mut().unwrap().remove(0);

        let (resp, on_disk) = fleet_apply(&file_cfg, &submitted);

        assert_eq!(resp.errors, vec![]);
        assert_eq!(
            on_disk.pointer("/cameras/0/id").and_then(Value::as_str),
            Some("cam-1")
        );
        assert_eq!(
            on_disk
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("stored-1"),
            "the survivor must keep its own secret, not the deleted entry's"
        );
    }

    #[test]
    fn config_apply_reordering_cameras_keeps_each_secret_with_its_entry() {
        let file_cfg = fleet_config(&[Some("stored-0"), Some("stored-1")]);
        let mut submitted = redacted_round_trip(&file_cfg);
        submitted["cameras"].as_array_mut().unwrap().reverse();

        let (resp, on_disk) = fleet_apply(&file_cfg, &submitted);

        assert_eq!(resp.errors, vec![]);
        assert_eq!(
            on_disk
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("stored-1"),
            "cam-1 moved to index 0 and must carry its own secret along"
        );
        assert_eq!(
            on_disk
                .pointer("/cameras/1/auth/password")
                .and_then(Value::as_str),
            Some("stored-0")
        );
    }

    #[test]
    fn config_apply_lone_rename_keeps_the_entrys_stored_secret() {
        // Renaming one entry in place is the array's lone id change, so the
        // entry pairs positionally and keeps its credential.
        let file_cfg = fleet_config(&[Some("stored-0"), Some("stored-1")]);
        let mut submitted = redacted_round_trip(&file_cfg);
        submitted["cameras"][0]["id"] = json!("cam-renamed");

        let (resp, on_disk) = fleet_apply(&file_cfg, &submitted);

        assert_eq!(resp.errors, vec![]);
        assert_eq!(
            on_disk.pointer("/cameras/0/id").and_then(Value::as_str),
            Some("cam-renamed")
        );
        assert_eq!(
            on_disk
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("stored-0")
        );
    }

    #[test]
    fn config_apply_rename_alongside_removal_is_rejected_not_cross_paired() {
        // An unknown id in an array whose shape changed in other ways too has
        // no unambiguous prior — reject loudly, never guess positionally.
        let file_cfg = fleet_config(&[Some("stored-0"), Some("stored-1")]);
        let submitted = json!({
            "cameras": [
                { "id": "cam-renamed", "auth": { "username": "obs", "password": REDACTED } }
            ],
            "name": "fleet"
        });

        let (resp, on_disk) = fleet_apply(&file_cfg, &submitted);

        assert_eq!(resp.status, ApplyStatus::Invalid);
        assert_eq!(resp.errors.len(), 1, "{:?}", resp.errors);
        assert_eq!(resp.errors[0].path, "cameras.0.auth.password");
        // The file is untouched — both stored secrets survive.
        assert_eq!(
            on_disk
                .pointer("/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("stored-0")
        );
        assert_eq!(
            on_disk
                .pointer("/cameras/1/auth/password")
                .and_then(Value::as_str),
            Some("stored-1")
        );
    }

    #[test]
    fn build_persist_value_pairs_id_less_array_elements_positionally() {
        // Elements without an "id" member keep the positional pairing (no
        // driver ships such a shape; the behaviour is pinned regardless).
        let submitted = json!({ "clients": [ { "password": REDACTED } ] });
        let file = json!({ "clients": [ { "password": "stored" } ] });
        let (to_write, _) = build_persist_value(&submitted, &file, &[], &["/clients/*/password"]);
        assert_eq!(
            to_write
                .pointer("/clients/0/password")
                .and_then(Value::as_str),
            Some("stored")
        );
    }

    #[test]
    fn build_persist_value_pairs_object_wildcard_secrets_by_key() {
        // A `*` over an object pairs by key, so dropping a sibling key cannot
        // rewire the survivor's secret.
        let submitted = json!({ "clients": { "beta": { "password": REDACTED } } });
        let file = json!({
            "clients": {
                "alpha": { "password": "pa" },
                "beta": { "password": "pb" }
            }
        });
        let (to_write, _) = build_persist_value(&submitted, &file, &[], &["/clients/*/password"]);
        assert_eq!(
            to_write
                .pointer("/clients/beta/password")
                .and_then(Value::as_str),
            Some("pb")
        );
    }

    // --- apply disposition ------------------------------------------------------

    /// Same config shape as [`TestDriver`], but with no in-process reload:
    /// every changed field is classified `restart_required`.
    struct RestartDriver;

    impl ConfigurableDriver for RestartDriver {
        type Config = TestConfig;
        type Overrides = TestOverrides;

        fn normalize(config: &mut Self::Config) {
            TestDriver::normalize(config);
        }

        fn validate(config: &Self::Config) -> Vec<FieldError> {
            TestDriver::validate(config)
        }

        fn secret_pointers() -> &'static [&'static str] {
            TestDriver::secret_pointers()
        }

        fn override_paths(overrides: &Self::Overrides) -> Vec<String> {
            TestDriver::override_paths(overrides)
        }

        fn apply_overrides(config: &mut Self::Config, overrides: &Self::Overrides) {
            TestDriver::apply_overrides(config, overrides);
        }

        fn apply_disposition() -> ApplyDisposition {
            ApplyDisposition::Restart
        }
    }

    #[test]
    fn default_apply_disposition_is_reload() {
        assert_eq!(TestDriver::apply_disposition(), ApplyDisposition::Reload);
    }

    #[test]
    fn config_apply_restart_disposition_classifies_changes_as_restart_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        save(&path, &serde_json::to_value(&before).unwrap()).unwrap();

        let mut changed = valid_config();
        changed.serial.baud_rate = 9600;
        let submitted = serde_json::to_string(&changed).unwrap();

        let resp =
            config_apply::<RestartDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        // Persisted; takes effect on the next process start — status stays ok.
        assert_eq!(resp.status, ApplyStatus::Ok);
        assert_eq!(resp.restart_required, vec!["serial.baud_rate".to_string()]);
        assert_eq!(resp.reload, Vec::<String>::new());
        assert_eq!(resp.persisted_to, Some(path.display().to_string()));
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk.pointer("/serial/baud_rate").and_then(Value::as_u64),
            Some(9600)
        );
    }

    #[test]
    fn config_apply_restart_disposition_no_change_is_ok_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let before = valid_config();
        save(&path, &serde_json::to_value(&before).unwrap()).unwrap();

        let submitted = serde_json::to_string(&valid_config()).unwrap();
        let resp =
            config_apply::<RestartDriver>(&path, &TestOverrides::default(), &before, &submitted)
                .unwrap();

        assert_eq!(resp.status, ApplyStatus::Ok);
        assert_eq!(resp.restart_required, Vec::<String>::new());
        assert_eq!(resp.reload, Vec::<String>::new());
    }

    #[test]
    fn dotted_pointer_conversions_round_trip() {
        assert_eq!(dotted_to_pointer("serial.port"), "/serial/port");
        assert_eq!(
            pointer_to_dotted("/server/auth/password_hash"),
            "server.auth.password_hash"
        );
    }
}

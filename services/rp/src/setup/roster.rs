//! rp's own part of its setup page: the equipment block with its roster,
//! and the registered plugins (`docs/services/rp.md` § "Setup Page").
//!
//! The roster is an array of entries per kind, which the generated form
//! names as edited in the file; here each entry is a row of its own fields,
//! named by its place in the file (`equipment.cameras.0.gain`) as rp's
//! validation names its errors. A row carries where it came from — its
//! place in the file and the id it had there — so a save can check that the
//! file has not moved on since the page was drawn, and a page drawn again
//! after a rejected save, whose rows follow the submission, still knows
//! each row's entry in the file.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use maud::{html, Markup};
use rusty_photon_config::actions::FieldError;
use rusty_photon_setup::form::overlay_field;
use rusty_photon_setup::origin::link_for;
use rusty_photon_setup::render::{self, FieldState};
use rusty_photon_setup::schema::{resolve, unwrap_optional, FieldKind, FieldModel, FieldSpec};
use rusty_photon_setup::{
    BoxFuture, CustomSection, DriverPages, FormValues, PageScope, SectionView,
};
use serde_json::{json, Map, Value};
use tokio::task::JoinSet;
use tracing::debug;

use crate::config::Config;
use crate::equipment::EquipmentRegistry;
use crate::train_lease::facade;

/// How long the page waits for a connected camera to say whether it is
/// `phd2-guider`'s facade: past it, the camera is drawn as any camera.
const FACADE_PROBE: Duration = Duration::from_secs(2);

/// The prefix a row's controls are rewritten to while its fields are
/// overlaid onto its entry, whatever place the row was submitted under.
const SCRATCH: &str = "entry";

/// One kind of roster entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Kind {
    /// Its key under `equipment`.
    key: &'static str,
    /// Its device type in Alpaca's path spelling.
    alpaca: &'static str,
    /// One entry, in the page's words.
    noun: &'static str,
    /// The kind's heading.
    heading: &'static str,
}

/// The kinds the roster lists, each an array under `equipment`.
const KINDS: [Kind; 9] = [
    Kind {
        key: "cameras",
        alpaca: "camera",
        noun: "camera",
        heading: "Cameras",
    },
    Kind {
        key: "filter_wheels",
        alpaca: "filterwheel",
        noun: "filter wheel",
        heading: "Filter wheels",
    },
    Kind {
        key: "focusers",
        alpaca: "focuser",
        noun: "focuser",
        heading: "Focusers",
    },
    Kind {
        key: "cover_calibrators",
        alpaca: "covercalibrator",
        noun: "cover calibrator",
        heading: "Cover calibrators",
    },
    Kind {
        key: "rotators",
        alpaca: "rotator",
        noun: "rotator",
        heading: "Rotators",
    },
    Kind {
        key: "switches",
        alpaca: "switch",
        noun: "switch",
        heading: "Switches",
    },
    Kind {
        key: "safety_monitors",
        alpaca: "safetymonitor",
        noun: "safety monitor",
        heading: "Safety monitors",
    },
    Kind {
        key: "observing_conditions",
        alpaca: "observingconditions",
        noun: "observing conditions device",
        heading: "Observing conditions",
    },
    Kind {
        key: "domes",
        alpaca: "dome",
        noun: "dome",
        heading: "Domes",
    },
];

/// The singular mount, an optional object under `equipment`.
const MOUNT: Kind = Kind {
    key: "mount",
    alpaca: "telescope",
    noun: "mount",
    heading: "Mount",
};

/// Where a row's entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// The entry at `index` in the file, which carried `id` there (the
    /// mount: its `alpaca_url`).
    File { index: usize, id: String },
    /// An entry the save adds.
    New,
}

impl Origin {
    fn encode(&self) -> String {
        match self {
            Self::File { index, id } => format!("file:{index}:{id}"),
            Self::New => "new".to_string(),
        }
    }

    fn decode(text: &str) -> Option<Self> {
        if text == "new" {
            return Some(Self::New);
        }
        let (index, id) = text.strip_prefix("file:")?.split_once(':')?;
        Some(Self::File {
            index: index.parse().ok()?,
            id: id.to_string(),
        })
    }
}

/// One row as the submission carries it: the prefix its controls were
/// named by, where its entry came from, and whether its Remove box was
/// checked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Submitted {
    prefix: String,
    origin: Origin,
    removed: bool,
}

/// A row of the entry list a save builds, in order: where its entry came
/// from, and the prefix its controls were submitted under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    origin: Origin,
    from: String,
}

fn origin_control(prefix: &str) -> String {
    format!("origin:{prefix}")
}

fn remove_control(prefix: &str) -> String {
    format!("remove:{prefix}")
}

/// The control a page drawn again after a rejected save carries the
/// entries of `kind` that save removes under, one origin per value.
fn removed_control(kind: Kind) -> String {
    format!("removed:equipment.{}", kind.key)
}

fn add_prefix(kind: Kind) -> String {
    format!("add.{}", kind.key)
}

fn row_prefix(kind: Kind, index: Option<usize>) -> String {
    index.map_or_else(
        || format!("equipment.{}", kind.key),
        |i| format!("equipment.{}.{i}", kind.key),
    )
}

/// The rows of `kind` the submission carries, in the order the page drew
/// them.
fn submitted_rows(form: &FormValues, kind: Kind) -> Vec<Submitted> {
    let mut rows: Vec<(usize, Submitted)> = Vec::new();
    for (name, value) in form.pairs() {
        let Some(prefix) = name.strip_prefix("origin:") else {
            continue;
        };
        let place = if kind == MOUNT {
            (prefix == row_prefix(MOUNT, None)).then_some(0)
        } else {
            prefix
                .strip_prefix(&format!("equipment.{}.", kind.key))
                .and_then(|n| n.parse::<usize>().ok())
        };
        let (Some(place), Some(origin)) = (place, Origin::decode(value)) else {
            continue;
        };
        rows.push((
            place,
            Submitted {
                prefix: prefix.to_string(),
                origin,
                removed: form.contains_key(&remove_control(prefix)),
            },
        ));
    }
    rows.sort_by_key(|(place, _)| *place);
    rows.into_iter().map(|(_, row)| row).collect()
}

/// The file's entries of `kind` the submission removes: the rows whose
/// Remove box is checked, and those a page drawn again after a rejected
/// save carries as removed.
fn removed_origins(form: &FormValues, kind: Kind) -> Vec<Origin> {
    submitted_rows(form, kind)
        .into_iter()
        .filter(|row| row.removed)
        .map(|row| row.origin)
        .chain(
            form.get_all(&removed_control(kind))
                .filter_map(Origin::decode),
        )
        .filter(|origin| matches!(origin, Origin::File { .. }))
        .collect()
}

/// Whether the submission adds an entry of `kind`: its `id` is filled (the
/// mount's `alpaca_url`).
fn adds(form: &FormValues, kind: Kind) -> bool {
    let key = if kind == MOUNT { "alpaca_url" } else { "id" };
    form.get(&format!("{}.{key}", add_prefix(kind)))
        .is_some_and(|v| !v.trim().is_empty())
}

/// The entries of `kind` a save of `form` builds, in order: the rows not
/// removed, then the added one.
fn plan(form: &FormValues, kind: Kind) -> Vec<Planned> {
    let mut planned: Vec<Planned> = submitted_rows(form, kind)
        .into_iter()
        .filter(|row| !row.removed)
        .map(|row| Planned {
            origin: row.origin,
            from: row.prefix,
        })
        .collect();
    let room = kind != MOUNT || planned.is_empty();
    if room && adds(form, kind) {
        planned.push(Planned {
            origin: Origin::New,
            from: add_prefix(kind),
        });
    }
    planned
}

/// The pairs of `form` under `prefix`, renamed under [`SCRATCH`].
fn scoped(form: &FormValues, prefix: &str) -> FormValues {
    let lead = format!("{prefix}.");
    FormValues::from(
        form.pairs()
            .iter()
            .filter_map(|(name, value)| {
                name.strip_prefix(&lead)
                    .map(|rest| (format!("{SCRATCH}.{rest}"), value.clone()))
            })
            .collect::<Vec<_>>(),
    )
}

/// `error`, named under `to` when it names a control under [`SCRATCH`].
fn renamed(error: FieldError, to: &str) -> FieldError {
    let path = error
        .path
        .strip_prefix(SCRATCH)
        .filter(|rest| rest.is_empty() || rest.starts_with('.'))
        .map_or_else(|| error.path.clone(), |rest| format!("{to}{rest}"));
    FieldError {
        path,
        msg: error.msg,
    }
}

/// The text of `value` at `key` in `entry`.
fn text<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// rp's own part of its setup page.
pub struct RosterPages {
    equipment: Arc<EquipmentRegistry>,
    schema: Arc<Value>,
}

impl RosterPages {
    /// The part drawn from the registry rp is running.
    #[must_use]
    pub fn new(equipment: Arc<EquipmentRegistry>) -> Self {
        let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap_or_default();
        Self {
            equipment,
            schema: Arc::new(schema),
        }
    }

    /// The ids of the connected cameras that are `phd2-guider`'s facade:
    /// their `SupportedActions` lists the sweep lease's acquire. A camera
    /// that does not answer within [`FACADE_PROBE`] is taken for any
    /// camera.
    async fn facades(&self) -> HashSet<String> {
        let mut probes = JoinSet::new();
        for camera in &self.equipment.cameras {
            let Some(device) = camera.session.device() else {
                continue;
            };
            let id = camera.id.clone();
            _ = probes.spawn(async move {
                let offers =
                    tokio::time::timeout(FACADE_PROBE, facade::offers_lease(device.as_ref())).await;
                (id, matches!(offers, Ok(Ok(true))))
            });
        }
        let mut facades = HashSet::new();
        while let Some(joined) = probes.join_next().await {
            if let Ok((id, true)) = joined {
                _ = facades.insert(id);
            }
        }
        facades
    }
}

impl DriverPages for RosterPages {
    fn owned_keys(&self) -> Vec<String> {
        vec!["equipment".to_string(), "plugins".to_string()]
    }

    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        _saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        Box::pin(async move {
            if !matches!(scope, PageScope::Server) {
                return Vec::new();
            }
            let live = serde_json::to_value(self.equipment.status()).unwrap_or_default();
            let facades = self.facades().await;
            debug!(
                ?facades,
                "the setup page's roster found these cameras to be the facade"
            );
            let sections: Vec<Box<dyn CustomSection>> = vec![
                Box::new(RosterSection {
                    schema: Arc::clone(&self.schema),
                    live,
                    facades,
                }),
                Box::new(PluginsSection),
            ];
            sections
        })
    }
}

/// The equipment block: its own settings, the optical trains named as
/// edited in the file, and the roster.
struct RosterSection {
    schema: Arc<Value>,
    /// The running registry's state, as `GET /api/equipment` serves it.
    live: Value,
    /// The running cameras that are `phd2-guider`'s facade.
    facades: HashSet<String>,
}

impl RosterSection {
    fn equipment_node(&self) -> &Value {
        self.schema
            .pointer("/properties/equipment")
            .map_or(&Value::Null, |node| resolve(node, &self.schema))
    }

    /// The schema of one entry of `kind`.
    fn entry_node(&self, kind: Kind) -> &Value {
        let equipment = self.equipment_node();
        if kind == MOUNT {
            return equipment
                .pointer("/properties/mount")
                .and_then(|node| unwrap_optional(node, &self.schema))
                .unwrap_or(&Value::Null);
        }
        equipment
            .pointer(&format!("/properties/{}/items", kind.key))
            .unwrap_or(&Value::Null)
    }

    /// The equipment block's own fields: its settings, and the optical
    /// trains as edited in the file. The roster's kinds are drawn as rows.
    fn equipment_fields(&self) -> FieldModel {
        FieldModel::from_node_under(&self.schema, self.equipment_node(), "equipment")
    }

    fn own_field(spec: &FieldSpec) -> bool {
        let Some(key) = spec.name().strip_prefix("equipment.") else {
            return false;
        };
        key != MOUNT.key && !KINDS.iter().any(|kind| kind.key == key)
    }

    /// The fields of an entry of `kind` under `prefix`, its credential left
    /// to [`credential_fields`].
    /// The fields that say what an entry is come first, in this order; the
    /// rest follow in the schema's.
    fn entry_fields(&self, kind: Kind, prefix: &str) -> Vec<FieldSpec> {
        const LEADING: [&str; 4] = ["id", "name", "alpaca_url", "device_number"];
        let auth = format!("{prefix}.auth");
        let rank = |spec: &FieldSpec| {
            spec.name()
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|key| LEADING.iter().position(|lead| *lead == key))
                .unwrap_or(LEADING.len())
        };
        let mut fields: Vec<FieldSpec> =
            FieldModel::from_node_under(&self.schema, self.entry_node(kind), prefix)
                .fields()
                .iter()
                .filter(|spec| spec.name() != auth)
                .cloned()
                .collect();
        fields.sort_by_key(rank);
        fields
    }

    /// The entries of `kind` in `config`.
    fn entries(config: &Value, kind: Kind) -> Vec<Value> {
        let at = config.pointer(&format!("/equipment/{}", kind.key));
        if kind == MOUNT {
            return at
                .filter(|mount| mount.is_object())
                .cloned()
                .into_iter()
                .collect();
        }
        at.and_then(Value::as_array).cloned().unwrap_or_default()
    }

    /// The id a row of `kind` is checked by: its `id`, the mount's address.
    fn identity(kind: Kind, entry: &Value) -> String {
        let key = if kind == MOUNT { "alpaca_url" } else { "id" };
        text(entry, key).to_string()
    }

    /// The running registry's state of the entry `id` of `kind`.
    fn live_of(&self, kind: Kind, id: &str) -> Option<&Value> {
        if kind == MOUNT {
            return self.live.get(MOUNT.key).filter(|m| m.is_object());
        }
        self.live
            .get(kind.key)
            .and_then(Value::as_array)?
            .iter()
            .find(|device| device.get("id").and_then(Value::as_str) == Some(id))
    }
}

impl CustomSection for RosterSection {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let state = FieldState {
            pinned: &[],
            errors: view.errors,
        };
        let model = self.equipment_fields();
        html! {
            fieldset.roster {
                legend { "Equipment" }
                @for spec in model.fields().iter().filter(|s| Self::own_field(s)) {
                    (render::field(spec, view.config, state))
                }
                p.hint {
                    "The roster: what rp connects to. Each entry opens to its settings; "
                    "the links lead to its driver's own pages."
                }
                @for kind in KINDS { (self.kind_block(kind, view)) }
                (self.kind_block(MOUNT, view))
            }
        }
    }

    fn has_controls(&self) -> bool {
        true
    }

    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let model = self.equipment_fields();
        for spec in model.fields().iter().filter(|s| Self::own_field(s)) {
            overlay_field(config, spec, form, &mut errors);
        }
        for kind in KINDS {
            self.apply_kind(kind, form, config, &mut errors);
        }
        self.apply_kind(MOUNT, form, config, &mut errors);
        errors
    }

    fn shows_error(&self, path: &str) -> bool {
        if self
            .equipment_fields()
            .fields()
            .iter()
            .any(|s| Self::own_field(s) && s.name() == path && s.is_editable_scalar())
        {
            return true;
        }
        KINDS.iter().chain([&MOUNT]).any(|kind| {
            let rest = if *kind == MOUNT {
                path.strip_prefix("equipment.mount.")
            } else {
                path.strip_prefix(&format!("equipment.{}.", kind.key))
                    .and_then(|rest| rest.split_once('.'))
                    .filter(|(n, _)| n.parse::<usize>().is_ok())
                    .map(|(_, field)| field)
            };
            rest.is_some_and(|field| {
                field == "auth.username"
                    || field == "auth.password"
                    || self
                        .entry_fields(*kind, SCRATCH)
                        .iter()
                        .any(|s| s.is_editable_scalar() && s.name() == format!("{SCRATCH}.{field}"))
            })
        })
    }
}

impl RosterSection {
    /// One kind's rows and its Add block.
    fn kind_block(&self, kind: Kind, view: SectionView<'_>) -> Markup {
        let entries = Self::entries(view.config, kind);
        // Drawn again after a rejected save, the rows follow the
        // submission; drawn from the file, each row is its entry there.
        let planned = view.submitted.map(|form| plan(form, kind));
        let removed = view
            .submitted
            .map(|form| removed_origins(form, kind))
            .unwrap_or_default();
        html! {
            h3.kind { (kind.heading) }
            @if !removed.is_empty() {
                p.hint.removed {
                    "Removed by this save: "
                    @for (n, origin) in removed.iter().enumerate() {
                        @if n > 0 { ", " }
                        @if let Origin::File { id, .. } = origin { code { (id) } }
                    }
                    ". Reload the page to keep them."
                }
                @for origin in &removed {
                    input type="hidden" name=(removed_control(kind)) value=(origin.encode());
                }
            }
            @if entries.is_empty() {
                p.hint { "None." }
            }
            @for (place, entry) in entries.iter().enumerate() {
                @let planned = planned.as_ref().and_then(|p| p.get(place));
                @let origin = planned.map_or_else(
                    || Origin::File { index: place, id: Self::identity(kind, entry) },
                    |p| p.origin.clone(),
                );
                @let prefix = row_prefix(kind, (kind != MOUNT).then_some(place));
                (self.row(kind, &prefix, entry, &origin, planned.map(|p| p.from.as_str()), view))
            }
            @if kind != MOUNT || entries.is_empty() {
                (self.add_block(kind, view))
            }
        }
    }

    /// One entry's row: its summary, then its fields.
    fn row(
        &self,
        kind: Kind,
        prefix: &str,
        entry: &Value,
        origin: &Origin,
        from: Option<&str>,
        view: SectionView<'_>,
    ) -> Markup {
        let id = if kind == MOUNT {
            "mount"
        } else {
            text(entry, "id")
        };
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = text(entry, "alpaca_url");
        let number = entry
            .get("device_number")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let username = entry.pointer("/auth/username").and_then(Value::as_str);
        let live = match origin {
            Origin::File { .. } => self.live_of(kind, id),
            Origin::New => None,
        };
        let open = view
            .errors
            .iter()
            .any(|e| e.path.starts_with(&format!("{prefix}.")));
        let state = FieldState {
            pinned: &[],
            errors: view.errors,
        };
        let (identity, plain): (Vec<FieldSpec>, Vec<FieldSpec>) = self
            .entry_fields(kind, prefix)
            .into_iter()
            .partition(|s| s.name().ends_with(".unique_id"));
        html! {
            details.entry data-entry=(id) open[open] {
                summary {
                    (led(live))
                    strong.entry-id { (id) }
                    @if !name.is_empty() { span.entry-name { (name) } }
                    span.entry-address { (url) " · device " (number) }
                    span.entry-credential {
                        @match username {
                            Some(user) => { "as " (user) }
                            None => "no credential",
                        }
                    }
                }
                (Self::facts(live, origin))
                (self.links(kind, id, url, number, view))
                input type="hidden" name=(origin_control(prefix)) value=(origin.encode());
                @for spec in &plain { (render::field(spec, view.config, state)) }
                (credential_fields(prefix, entry, from, view))
                @if !identity.is_empty() {
                    details.identity {
                        summary { "Identity" }
                        p.hint {
                            "The UniqueID the device at this number must report, or rp refuses the "
                            "connect. Blank leaves the entry unpinned."
                        }
                        @for spec in &identity { (render::field(spec, view.config, state)) }
                    }
                }
                div.checkbox.remove {
                    input type="checkbox" name=(remove_control(prefix)) id=(remove_control(prefix));
                    label for=(remove_control(prefix)) { "Remove this " (kind.noun) " from the roster" }
                }
            }
        }
    }

    /// What the running rp knows of the entry.
    fn facts(live: Option<&Value>, origin: &Origin) -> Markup {
        let field = |key: &str| {
            live.and_then(|l| l.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        html! {
            dl.facts {
                dt { "State" }
                dd.entry-state {
                    @match (origin, live) {
                        (Origin::New, _) => "added by this save: connects after rp restarts",
                        (_, None) => "not in the roster rp is running: connects after rp restarts",
                        (_, Some(l)) if l.get("connected").and_then(Value::as_bool) == Some(true) => "connected",
                        (_, Some(_)) => "not connected",
                    }
                }
                @if let Some(name) = field("device_name") {
                    dt { "Bound to" }
                    dd { (name) @if let Some(uid) = field("unique_id") { " · " (uid) } }
                }
            }
        }
    }

    /// The links to the entry's driver: its server page and the device's
    /// own, or, for `phd2-guider`'s facade, the guider's page.
    fn links(&self, kind: Kind, id: &str, url: &str, number: u64, view: SectionView<'_>) -> Markup {
        let base = url.trim_end_matches('/');
        if base.is_empty() {
            return html! {};
        }
        if kind.key == "cameras" && self.facades.contains(id) {
            let guider = view
                .config
                .pointer("/equipment/mount/guiding/url")
                .and_then(Value::as_str)
                .map(|g| link_for(&format!("{}/setup", g.trim_end_matches('/')), view.host));
            return html! {
                p.links {
                    @match guider {
                        Some(link) => {
                            "This camera is phd2-guider's facade, configured on "
                            a.guider-setup href=(link) { "the guider's setup page" } "."
                        }
                        None => {
                            "This camera is phd2-guider's facade, configured on the guider's "
                            "setup page; rp has no guiding address (equipment.mount.guiding.url) to link it by."
                        }
                    }
                }
            };
        }
        let server = link_for(&format!("{base}/setup"), view.host);
        let device = link_for(
            &format!("{base}/setup/v1/{}/{number}/setup", kind.alpaca),
            view.host,
        );
        let ip_note = server != format!("{base}/setup")
            && url.starts_with("https://")
            && view.host.is_some_and(|h| {
                h.trim_matches(|c| c == '[' || c == ']')
                    .parse::<std::net::IpAddr>()
                    .is_ok()
            });
        html! {
            p.links {
                a.server-setup href=(server) { "Driver's setup page" }
                " · "
                a.device-setup href=(device) { "This device's page" }
            }
            @if ip_note {
                p.hint {
                    "This page was reached by an IP address, which a doctor-issued certificate names "
                    "only when doctor was given --extra-san: the browser may refuse the driver's page."
                }
            }
        }
    }

    /// The Add block of `kind`: a blank entry, its fields at their
    /// defaults.
    fn add_block(&self, kind: Kind, view: SectionView<'_>) -> Markup {
        let prefix = add_prefix(kind);
        let blank = json!({});
        let state = FieldState {
            pinned: &[],
            errors: &[],
        };
        let (identity, plain): (Vec<FieldSpec>, Vec<FieldSpec>) = self
            .entry_fields(kind, &prefix)
            .into_iter()
            .partition(|s| s.name().ends_with(".unique_id"));
        let required = if kind == MOUNT { "address" } else { "id" };
        html! {
            details.entry.add {
                summary { "Add " (article(kind.noun)) " " (kind.noun) }
                p.hint { "Saved with the page when its " (required) " is filled." }
                @for spec in plain.iter().filter(|s| !matches!(s.kind(), FieldKind::FileOnly)) {
                    (render::field(spec, &blank, state))
                }
                (credential_fields(&prefix, &blank, None, SectionView { errors: &[], ..view }))
                @for spec in &identity { (render::field(spec, &blank, state)) }
            }
        }
    }

    /// Build `kind`'s entries from the submission onto `config`.
    fn apply_kind(
        &self,
        kind: Kind,
        form: &FormValues,
        config: &mut Value,
        errors: &mut Vec<FieldError>,
    ) {
        let file = Self::entries(config, kind);
        // The rows, and the entries a page drawn again carries as removed,
        // must be the file's entries as the page first drew them: each
        // where it was, with the id it had, and none the page did not draw.
        let carried = removed_control(kind);
        let origins = submitted_rows(form, kind)
            .into_iter()
            .map(|row| row.origin)
            .chain(form.get_all(&carried).filter_map(Origin::decode));
        let mut drawn: Vec<usize> = Vec::new();
        for origin in origins {
            if let Origin::File { index, id } = &origin {
                if file.get(*index).map(|e| Self::identity(kind, e)).as_deref() != Some(id) {
                    errors.push(moved_on(kind));
                    return;
                }
                drawn.push(*index);
            }
        }
        drawn.sort_unstable();
        drawn.dedup();
        if drawn.len() != file.len() {
            errors.push(moved_on(kind));
            return;
        }
        let mut built = Vec::new();
        for planned in plan(form, kind) {
            let base = match &planned.origin {
                Origin::File { index, .. } => {
                    file.get(*index).cloned().unwrap_or_else(|| json!({}))
                }
                Origin::New => json!({}),
            };
            let to = row_prefix(kind, (kind != MOUNT).then_some(built.len()));
            let sub = scoped(form, &planned.from);
            let mut scratch = json!({ SCRATCH: base });
            let mut row_errors = Vec::new();
            for spec in self.entry_fields(kind, SCRATCH) {
                if spec.is_editable_scalar() {
                    overlay_field(&mut scratch, &spec, &sub, &mut row_errors);
                }
            }
            let mut entry = scratch
                .get_mut(SCRATCH)
                .map(Value::take)
                .unwrap_or_default();
            row_errors.extend(apply_credential(&sub, &mut entry));
            errors.extend(row_errors.into_iter().map(|e| renamed(e, &to)));
            built.push(entry);
        }
        let Some(equipment) = config.get_mut("equipment").and_then(Value::as_object_mut) else {
            errors.push(FieldError {
                path: String::new(),
                msg: "the configuration file has no equipment object".to_string(),
            });
            return;
        };
        if kind == MOUNT {
            match built.pop() {
                Some(mount) => _ = equipment.insert(MOUNT.key.to_string(), mount),
                None => _ = equipment.remove(MOUNT.key),
            }
        } else if !built.is_empty() || equipment.contains_key(kind.key) {
            // A kind the file leaves out stays out while the save adds none.
            _ = equipment.insert(kind.key.to_string(), Value::Array(built));
        }
    }
}

/// "a" or "an", as `noun` takes.
fn article(noun: &str) -> &'static str {
    if noun.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

/// The error a save answers when the file's entries of `kind` are not the
/// ones the page drew.
fn moved_on(kind: Kind) -> FieldError {
    FieldError {
        path: String::new(),
        msg: format!(
            "The configuration file's {} changed after this page was drawn; nothing was saved. \
             Reload the page and make the change again.",
            kind.heading.to_lowercase()
        ),
    }
}

/// The entry's lamp: connected, not connected, or not in the roster rp is
/// running.
fn led(live: Option<&Value>) -> Markup {
    let class = match live
        .and_then(|l| l.get("connected"))
        .and_then(Value::as_bool)
    {
        Some(true) => "led on",
        Some(false) => "led off",
        None => "led idle",
    };
    html! { span class=(class) {} }
}

/// The credential rp presents to the entry's driver: a username and a
/// password, which a page never carries back. `from` is the prefix the row
/// was submitted under, when the page is drawn again after a rejected save.
fn credential_fields(
    prefix: &str,
    entry: &Value,
    from: Option<&str>,
    view: SectionView<'_>,
) -> Markup {
    let user = format!("{prefix}.auth.username");
    let pass = format!("{prefix}.auth.password");
    let stored_user = entry.pointer("/auth/username").and_then(Value::as_str);
    let stored = entry
        .pointer("/auth/password")
        .and_then(Value::as_str)
        .is_some();
    let submitted = |field: &str| {
        from.zip(view.submitted)
            .and_then(|(from, form)| form.get(&format!("{from}.auth.{field}")))
    };
    let shown_user = submitted("username").or(stored_user).unwrap_or_default();
    let typed = submitted("password").is_some_and(|p| !p.is_empty());
    let error = |name: &str| {
        view.errors
            .iter()
            .find(|e| e.path == name)
            .map(|e| e.msg.clone())
    };
    let user_error = error(&user);
    let pass_error = error(&pass);
    html! {
        div.field.invalid[user_error.is_some()] {
            label for=(user) { "Username rp presents" }
            input type="text" id=(user) name=(user) value=(shown_user) autocomplete="off";
            div.hint { "Blank, with a blank password: rp presents no credential." }
            @if let Some(msg) = &user_error { div.error { (msg) } }
        }
        div.field.invalid[pass_error.is_some()] {
            label for=(pass) { "Password" }
            input type="password" id=(pass) name=(pass) value="" autocomplete="new-password";
            div.hint {
                @if typed {
                    "Type the password again: a page never carries one back."
                } @else if stored {
                    "Blank keeps the stored password."
                } @else {
                    "Needed with a username."
                }
            }
            @if let Some(msg) = &pass_error { div.error { (msg) } }
        }
    }
}

/// Apply a row's credential fields (named under [`SCRATCH`]) onto `entry`.
/// A row whose submission carries no username control keeps its block.
fn apply_credential(form: &FormValues, entry: &mut Value) -> Vec<FieldError> {
    let user_control = format!("{SCRATCH}.auth.username");
    let pass_control = format!("{SCRATCH}.auth.password");
    let Some(user) = form.get(&user_control).map(str::trim) else {
        return Vec::new();
    };
    let typed = form.get(&pass_control).unwrap_or_default();
    let Some(object) = entry.as_object_mut() else {
        return Vec::new();
    };
    let stored = object
        .get("auth")
        .and_then(|auth| auth.get("password"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let refuse = |path: &str, msg: &str| {
        vec![FieldError {
            path: path.to_string(),
            msg: msg.to_string(),
        }]
    };
    let mut set = |password: &str| {
        let mut auth = Map::new();
        _ = auth.insert("username".to_string(), Value::String(user.to_string()));
        _ = auth.insert("password".to_string(), Value::String(password.to_string()));
        _ = object.insert("auth".to_string(), Value::Object(auth));
        Vec::new()
    };
    match (user.is_empty(), typed.is_empty()) {
        (true, true) => {
            _ = object.remove("auth");
            Vec::new()
        }
        (true, false) => refuse(
            &user_control,
            "is needed with a password; clear both to present no credential",
        ),
        (false, false) => set(typed),
        (false, true) => stored.as_deref().map_or_else(
            || refuse(&pass_control, "is needed with a username: none is stored"),
            set,
        ),
    }
}

/// The registered plugins, edited in the file.
struct PluginsSection;

impl CustomSection for PluginsSection {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let plugins = view
            .config
            .get("plugins")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        html! {
            fieldset.plugins {
                legend { "Plugins" }
                @if plugins.is_empty() {
                    p.hint { "None registered." }
                } @else {
                    ul.devices {
                        @for plugin in &plugins {
                            li {
                                span { (text(plugin, "name")) }
                                span.kind { (text(plugin, "type")) }
                            }
                        }
                    }
                }
                p.file-only {
                    "A registration is the plugin's own vocabulary, which no form here renders: "
                    "the plugins are edited in the configuration file ("
                    code { "plugins" } ")."
                }
            }
        }
    }

    fn has_controls(&self) -> bool {
        false
    }

    fn apply(&self, _form: &FormValues, _config: &mut Value) -> Vec<FieldError> {
        Vec::new()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    fn file(id: &str, index: usize) -> Origin {
        Origin::File {
            index,
            id: id.to_string(),
        }
    }

    const CAMERAS: Kind = KINDS[0];

    fn section(live: Value, facades: &[&str]) -> RosterSection {
        RosterSection {
            schema: Arc::new(serde_json::to_value(schemars::schema_for!(Config)).unwrap()),
            live,
            facades: facades.iter().map(|f| (*f).to_string()).collect(),
        }
    }

    fn render(
        section: &RosterSection,
        config: &Value,
        submitted: Option<&FormValues>,
        host: Option<&str>,
    ) -> String {
        section
            .render(SectionView {
                config,
                submitted,
                errors: &[],
                host,
            })
            .into_string()
    }

    fn two_cameras() -> Value {
        json!({ "equipment": { "cameras": [
            { "id": "a", "alpaca_url": "http://127.0.0.1:1", "device_number": 0 },
            { "id": "b", "alpaca_url": "http://127.0.0.1:2", "device_number": 0 }
        ] } })
    }

    #[test]
    fn an_origin_survives_its_encoding() {
        for origin in [
            Origin::New,
            file("main-cam", 3),
            file("http://127.0.0.1:3", 0),
        ] {
            assert_eq!(Origin::decode(&origin.encode()), Some(origin));
        }
        assert_eq!(Origin::decode("file:x:cam"), None);
        assert_eq!(Origin::decode("elsewhere"), None);
    }

    #[test]
    fn a_save_plans_the_rows_kept_in_the_pages_order_then_the_added_one() {
        let submitted = form(&[
            ("origin:equipment.cameras.1", "file:1:b"),
            ("origin:equipment.cameras.0", "file:0:a"),
            ("origin:equipment.cameras.2", "file:2:c"),
            ("remove:equipment.cameras.1", "on"),
            ("add.cameras.id", "d"),
        ]);
        assert_eq!(
            plan(&submitted, CAMERAS),
            vec![
                Planned {
                    origin: file("a", 0),
                    from: "equipment.cameras.0".to_string()
                },
                Planned {
                    origin: file("c", 2),
                    from: "equipment.cameras.2".to_string()
                },
                Planned {
                    origin: Origin::New,
                    from: "add.cameras".to_string()
                },
            ]
        );
        assert_eq!(removed_origins(&submitted, CAMERAS), vec![file("b", 1)]);
    }

    #[test]
    fn a_blank_add_adds_nothing_and_a_kept_mount_leaves_no_room_for_another() {
        let submitted = form(&[
            ("add.cameras.id", "  "),
            ("origin:equipment.mount", "file:0:http://m"),
            ("add.mount.alpaca_url", "http://n"),
        ]);
        assert_eq!(plan(&submitted, CAMERAS), Vec::new());
        assert_eq!(plan(&submitted, MOUNT).len(), 1);
    }

    #[test]
    fn an_error_is_named_by_the_entrys_place_in_the_saved_list() {
        let error = |path: &str| FieldError {
            path: path.to_string(),
            msg: "bad".to_string(),
        };
        assert_eq!(
            renamed(error("entry.gain"), "equipment.cameras.1").path,
            "equipment.cameras.1.gain"
        );
        assert_eq!(renamed(error("entryway"), "x").path, "entryway");
        assert_eq!(renamed(error(""), "x").path, "");
    }

    fn credential(
        pairs: &[(&str, &str)],
        stored: Option<(&str, &str)>,
    ) -> (Value, Vec<FieldError>) {
        let mut entry = json!({ "id": "a" });
        if let Some((user, pass)) = stored {
            entry["auth"] = json!({ "username": user, "password": pass });
        }
        let errors = apply_credential(&form(pairs), &mut entry);
        (entry, errors)
    }

    #[test]
    fn a_username_with_a_password_sets_the_credential() {
        let (entry, errors) = credential(
            &[
                ("entry.auth.username", "obs"),
                ("entry.auth.password", "pw"),
            ],
            None,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            entry["auth"],
            json!({ "username": "obs", "password": "pw" })
        );
    }

    #[test]
    fn a_blank_password_keeps_the_stored_one() {
        let (entry, errors) = credential(
            &[
                ("entry.auth.username", "operator"),
                ("entry.auth.password", ""),
            ],
            Some(("obs", "hunter2")),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            entry["auth"],
            json!({ "username": "operator", "password": "hunter2" })
        );
    }

    #[test]
    fn a_username_with_no_password_stored_is_refused_on_the_password() {
        let (entry, errors) = credential(&[("entry.auth.username", "obs")], None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "entry.auth.password");
        assert!(entry.get("auth").is_none());
    }

    #[test]
    fn a_password_without_a_username_is_refused_on_the_username() {
        let (_, errors) = credential(
            &[("entry.auth.username", " "), ("entry.auth.password", "pw")],
            Some(("obs", "hunter2")),
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "entry.auth.username");
    }

    #[test]
    fn clearing_both_removes_the_credential_and_no_username_control_keeps_it() {
        let (entry, errors) = credential(
            &[("entry.auth.username", ""), ("entry.auth.password", "")],
            Some(("obs", "hunter2")),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(entry.get("auth").is_none());
        let (entry, _) = credential(&[], Some(("obs", "hunter2")));
        assert_eq!(entry["auth"]["password"], "hunter2");
    }

    #[test]
    fn a_save_overlays_removes_and_adds_by_the_rows_the_page_drew() {
        let mut config = two_cameras();
        let submitted = form(&[
            ("origin:equipment.cameras.0", "file:0:a"),
            ("remove:equipment.cameras.0", "on"),
            ("origin:equipment.cameras.1", "file:1:b"),
            ("equipment.cameras.1.id", "b"),
            ("equipment.cameras.1.gain", "120"),
            ("add.cameras.id", "c"),
            ("add.cameras.alpaca_url", "http://127.0.0.1:3"),
            ("add.cameras.device_number", "1"),
        ]);
        let errors = section(json!({}), &[]).apply(&submitted, &mut config);
        assert!(errors.is_empty(), "{errors:?}");
        let cameras = config["equipment"]["cameras"].as_array().unwrap();
        assert_eq!(cameras.len(), 2, "{cameras:?}");
        assert_eq!(cameras[0]["id"], "b");
        assert_eq!(cameras[0]["gain"], 120);
        assert_eq!(cameras[1]["id"], "c");
        assert_eq!(cameras[1]["device_number"], 1);
    }

    #[test]
    fn a_rejected_value_is_named_by_its_entrys_place_after_the_removal() {
        let mut config = two_cameras();
        let submitted = form(&[
            ("origin:equipment.cameras.0", "file:0:a"),
            ("remove:equipment.cameras.0", "on"),
            ("origin:equipment.cameras.1", "file:1:b"),
            ("equipment.cameras.1.gain", "loud"),
        ]);
        let errors = section(json!({}), &[]).apply(&submitted, &mut config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "equipment.cameras.0.gain");
    }

    #[test]
    fn a_save_from_a_page_drawn_before_the_roster_changed_is_refused() {
        let section = section(json!({}), &[]);
        // The file gained an entry the page did not draw.
        let mut config = two_cameras();
        let errors = section.apply(
            &form(&[("origin:equipment.cameras.0", "file:0:a")]),
            &mut config,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.msg.contains("changed after this page was drawn")),
            "{errors:?}"
        );
        assert_eq!(config, two_cameras());
        // An entry moved to another place.
        let mut config = two_cameras();
        let errors = section.apply(
            &form(&[
                ("origin:equipment.cameras.0", "file:0:b"),
                ("origin:equipment.cameras.1", "file:1:a"),
            ]),
            &mut config,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.msg.contains("changed after this page was drawn")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_page_drawn_again_carries_the_entries_the_save_removes() {
        let section = section(json!({}), &[]);
        let submitted = form(&[
            ("origin:equipment.cameras.0", "file:0:a"),
            ("remove:equipment.cameras.0", "on"),
            ("origin:equipment.cameras.1", "file:1:b"),
        ]);
        let mut shown = two_cameras();
        _ = section.apply(&submitted, &mut shown);
        let html = render(&section, &shown, Some(&submitted), None);
        assert!(
            html.contains(r#"name="removed:equipment.cameras" value="file:0:a""#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="origin:equipment.cameras.0" value="file:1:b""#),
            "{html}"
        );
        assert!(html.contains("Removed by this save"), "{html}");
        // The page drawn again saves: the removal it carries is accounted for.
        let mut again = two_cameras();
        let resubmitted = form(&[
            ("removed:equipment.cameras", "file:0:a"),
            ("origin:equipment.cameras.0", "file:1:b"),
        ]);
        let errors = section.apply(&resubmitted, &mut again);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(again["equipment"]["cameras"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_loopback_entry_links_its_driver_at_the_browsers_host() {
        let html = render(
            &section(json!({}), &[]),
            &two_cameras(),
            None,
            Some("rig.example"),
        );
        assert!(
            html.contains(r#"href="http://rig.example:1/setup""#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="http://rig.example:2/setup/v1/camera/0/setup""#),
            "{html}"
        );
    }

    #[test]
    fn an_https_link_reached_by_an_ip_address_notes_the_certificate() {
        let config = json!({ "equipment": { "cameras": [
            { "id": "a", "alpaca_url": "https://127.0.0.1:1", "device_number": 0 }
        ] } });
        let html = render(&section(json!({}), &[]), &config, None, Some("192.0.2.1"));
        assert!(html.contains("--extra-san"), "{html}");
        let html = render(&section(json!({}), &[]), &config, None, Some("rig.example"));
        assert!(!html.contains("--extra-san"), "{html}");
    }

    #[test]
    fn the_facade_links_the_guiders_page_or_says_why_it_cannot() {
        let mut config = two_cameras();
        let html = render(
            &section(json!({}), &["a"]),
            &config,
            None,
            Some("rig.example"),
        );
        assert!(html.contains("no guiding address"), "{html}");
        config["equipment"]["mount"] = json!({
            "alpaca_url": "http://127.0.0.1:9",
            "device_number": 0,
            "guiding": { "url": "http://127.0.0.1:11130/" }
        });
        let html = render(
            &section(json!({}), &["a"]),
            &config,
            None,
            Some("rig.example"),
        );
        assert!(
            html.contains(r#"class="guider-setup" href="http://rig.example:11130/setup""#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="http://rig.example:2/setup""#),
            "the other camera is a camera: {html}"
        );
    }

    #[test]
    fn an_entry_reads_its_live_state() {
        let live = json!({ "cameras": [
            { "id": "a", "connected": true, "device_name": "Sim", "unique_id": "u-1" }
        ] });
        let html = render(&section(live, &[]), &two_cameras(), None, None);
        assert!(html.contains("connected"), "{html}");
        assert!(html.contains("Sim · u-1"), "{html}");
        assert!(
            html.contains("not in the roster rp is running"),
            "b is not running: {html}"
        );
    }

    #[test]
    fn an_add_block_names_its_kind_with_its_article() {
        assert_eq!(article("observing conditions device"), "an");
        assert_eq!(article("camera"), "a");
        let html = render(
            &section(json!({}), &[]),
            &json!({ "equipment": {} }),
            None,
            None,
        );
        assert!(
            html.contains("Add an observing conditions device"),
            "{html}"
        );
        assert!(html.contains("Add a mount"), "{html}");
    }

    #[test]
    fn the_plugins_section_lists_the_registered_plugins_as_edited_in_the_file() {
        let config = json!({ "plugins": [{ "name": "focus-model", "type": "tool_provider" }] });
        let html = PluginsSection
            .render(SectionView {
                config: &config,
                submitted: None,
                errors: &[],
                host: None,
            })
            .into_string();
        assert!(html.contains("focus-model"), "{html}");
        assert!(html.contains("tool_provider"), "{html}");
        assert!(html.contains("edited in the configuration file"), "{html}");
        assert!(!PluginsSection.has_controls());
    }

    #[test]
    fn a_rows_error_is_shown_by_the_section() {
        let section = section(json!({}), &[]);
        assert!(section.shows_error("equipment.cameras.3.gain"));
        assert!(section.shows_error("equipment.cameras.0.auth.password"));
        assert!(section.shows_error("equipment.mount.device_number"));
        assert!(section.shows_error("equipment.reconnect_interval"));
        assert!(!section.shows_error("equipment.optical_trains.0.devices"));
        assert!(!section.shows_error("equipment.cameras.x.gain"));
    }
}

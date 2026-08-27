//! Server-rendered pages and fragments (Maud + HTMX) for the configuration UI,
//! plus the **schema-driven** form ⇆ Config mapping.
//!
//! The form is rendered generically from a driver's `config.schema` (a JSON
//! Schema + editability tiers): [`FieldModel::from_schema`] walks the schema into
//! a flat list of scalar leaves (resolving `$ref`, recursing plain objects, and
//! skipping `oneOf`/`anyOf`/`enum`/`const` subtrees, which round-trip untouched
//! via the hidden blob). [`config_card`] renders any driver's form from that
//! model; [`merge_form`] coerces a submission back into a `Config` using the same
//! model. No driver-specific field lists live here — every tier and field type
//! comes from the driver's own `config.schema` / `config.get`.
//!
//! The HTMX swap unit is the `#config-card` element: `GET /config/{service}`
//! returns the full page (or just the card for an HTMX request); `POST` and the
//! reconnect poll return a fresh `#config-card` fragment that HTMX swaps in by
//! `outerHTML`.

pub mod equipment;
pub mod stream;
pub mod targets;

use maud::{html, Markup, DOCTYPE};
use serde_json::Value;

use crate::driver_client::{ConfigClientError, ConfigSchemaResponse, FieldError};

/// Page identity passed to the card/fragment renderers: the service id (used in
/// every route URL) plus the display strings shown in the card header.
pub struct Page<'a> {
    pub service: &'a str,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Whether the "Restart via Sentinel" affordances render: true only when
    /// the BFF has a `sentinel` block configured (the driver then has a
    /// Sentinel-side service name to target).
    pub can_restart: bool,
}

impl Page<'_> {
    /// The BFF route the restart affordances post to.
    fn restart_url(&self) -> String {
        format!("/config/{}/restart", self.service)
    }
}

/// A status banner rendered above the form.
#[derive(Debug, Clone)]
pub enum Banner {
    /// `config.apply` returned `status:"ok"` — persisted, no reload needed.
    Saved,
    /// `config.apply` returned `status:"ok"` with `restart_required[]` paths:
    /// persisted, but the listed changes only take effect on the target's next
    /// process start (`ApplyDisposition::Restart` — rp, which has no in-process
    /// reload — or a driver field classified restart-required). The restart
    /// callout; when a Sentinel is configured, the "Restart via Sentinel"
    /// affordance attaches inline.
    SavedRestartRequired(Vec<String>),
    /// `config.apply` returned `status:"invalid"`.
    Invalid,
    /// The reconnect poll found the driver back after a reload.
    Reconnected,
}

/// Which top-nav tab a page belongs to (highlighted as active in the shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    /// The activity stream (`/stream`).
    Activity,
    /// The equipment roster (`/equipment`).
    Equipment,
    /// The targets inbox (`/targets`).
    Targets,
    /// The config pages (`/`, `/config/{service}`).
    Configuration,
}

/// The full HTML shell: dark theme, embedded CSS + HTMX, and the top
/// nav.
///
/// The nav carries the three surface tabs plus the mock's pure-CSS
/// night-vision toggle (a page-level red filter via
/// `body:has(#night-vision:checked)`; no JavaScript).
#[must_use]
pub fn layout(title: &str, body: &Markup) -> Markup {
    layout_with_nav(title, NavTab::Configuration, body)
}

/// [`layout`] with an explicit active tab (the config-page shell defaults to
/// [`NavTab::Configuration`]).
#[must_use]
pub fn layout_with_nav(title: &str, active: NavTab, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                nav.topnav {
                    div.logo {}
                    span.title { "rusty-photon" }
                    div.nav-tabs {
                        a .active[active == NavTab::Activity] href="/stream" { "Activity" }
                        a .active[active == NavTab::Equipment] href="/equipment" { "Equipment" }
                        a .active[active == NavTab::Targets] href="/targets" { "Targets" }
                        a .active[active == NavTab::Configuration] href="/" { "Configuration" }
                    }
                    span.grow {}
                    label.night-toggle title="Toggle red night-vision mode" {
                        // Visually hidden, not `hidden`: the checkbox must stay
                        // focusable so the toggle works from the keyboard.
                        input type="checkbox" id="night-vision" class="visually-hidden";
                        span.nv-icon { "☾" }
                        span.nv-lbl { "Night vision" }
                        span.nv-dot {}
                    }
                }
                main.container { (body) }
            }
        }
    }
}

// --- the schema-derived field model -------------------------------------------

/// How a submitted form value is coerced back into JSON for a single leaf,
/// inferred from the schema's `{type, minimum, maximum}`.
#[derive(Debug, Clone, PartialEq)]
enum FieldKind {
    /// A `string` leaf — rendered as a text input, set verbatim.
    /// `nullable` (schema `type:["string","null"]`) persists `null` when
    /// cleared, exactly as it does for [`FieldKind::Int`]/[`FieldKind::Num`]:
    /// an optional field the operator emptied is *unset*, and `""` is a
    /// value the driver is entitled to reject.
    Str { nullable: bool },
    /// A `boolean` leaf — rendered as a checkbox (present ⇒ true, absent ⇒ false).
    Bool,
    /// An `integer` leaf. `nullable` (schema `type:["integer","null"]`) persists
    /// `null` when cleared; `min`/`max` (schema bounds) gate the parsed value.
    Int {
        nullable: bool,
        min: Option<i64>,
        max: Option<i64>,
    },
    /// A `number` (floating-point) leaf. `nullable` persists `null` when cleared.
    Num { nullable: bool },
    /// An `array` whose `items` are an integer `enum` — rendered as a
    /// checkbox group, one checkbox per enumerated value in schema order
    /// (e.g. rp's per-camera `cooler_targets_c` grid). Checked values are
    /// written back as an array in `options` order; nothing checked ⇒ an
    /// empty array. Every other array shape stays [`Shape::Skip`].
    IntSet { options: Vec<i64> },
}

/// One renderable config field discovered in the schema.
#[derive(Debug, Clone)]
pub struct FieldSpec {
    /// Dotted path (`serial.port`) — the form input `name` and the tier key.
    name: String,
    /// RFC-6901 JSON pointer (`/serial/port`) into the config blob.
    pointer: String,
    /// Top-level section (`serial`) — fields are grouped into a fieldset by this.
    section: String,
    /// Human-readable label (the humanised sub-path after the section).
    label: String,
    kind: FieldKind,
}

impl FieldSpec {
    const fn input_type(&self) -> &'static str {
        match self.kind {
            FieldKind::Bool | FieldKind::IntSet { .. } => "checkbox",
            FieldKind::Int { .. } | FieldKind::Num { .. } => "number",
            FieldKind::Str { .. } => "text",
        }
    }
}

/// The driver's form model: the ordered scalar leaves plus the editability tiers
/// (both straight from `config.schema`). Drives both rendering and `merge_form`.
pub struct FieldModel {
    fields: Vec<FieldSpec>,
    locked: Vec<String>,
    read_only: Vec<String>,
}

impl FieldModel {
    /// Build the model from a driver's `config.schema` response.
    #[must_use]
    pub fn from_schema(resp: &ConfigSchemaResponse) -> Self {
        Self {
            fields: build_fields(&resp.schema),
            locked: resp.locked_fields.clone(),
            read_only: resp.read_only_fields.clone(),
        }
    }

    /// Build the model for **one equipment entry** of `kind_key` from rp's
    /// config schema: navigate `properties.equipment.properties.{kind_key}`,
    /// step into `items` for the list kinds, unwrap the optional wrapper
    /// (`Option<MountConfig>` renders as an `anyOf` with a null branch), and
    /// walk that entry object as the root — field names come out relative
    /// (`alpaca_url`, not `equipment.cameras.0.alpaca_url`). The editability
    /// tiers don't apply inside an entry, so they are empty. `None` when the
    /// schema doesn't carry that kind (an rp older than the roster page).
    #[must_use]
    pub fn from_item_schema(resp: &ConfigSchemaResponse, kind_key: &str) -> Option<Self> {
        let root = &resp.schema;
        let equipment = resolve(root.get("properties")?.get("equipment")?, root);
        let kind_node = resolve(equipment.get("properties")?.get(kind_key)?, root);
        // A list kind reads the array's item schema; the singular mount
        // uses the property itself (optional-wrapped).
        let entry_node = kind_node
            .get("items")
            .map_or(kind_node, |items| resolve(items, root));
        let entry_node = unwrap_optional(entry_node, root)?;
        let mut fields = Vec::new();
        walk_schema(entry_node, root, "", &mut fields);
        if fields.is_empty() {
            return None;
        }
        Some(Self {
            fields,
            locked: Vec::new(),
            read_only: Vec::new(),
        })
    }

    fn is_locked(&self, name: &str) -> bool {
        self.locked.iter().any(|n| n == name)
    }

    fn is_read_only(&self, name: &str) -> bool {
        self.read_only.iter().any(|n| n == name)
    }

    /// The ordered scalar leaves — for renderers in sibling page modules.
    pub(crate) fn field_specs(&self) -> &[FieldSpec] {
        &self.fields
    }
}

impl FieldSpec {
    /// The RFC-6901 pointer segments (used to build add-form skeletons).
    pub(crate) fn pointer_segments(&self) -> Vec<&str> {
        self.pointer.trim_start_matches('/').split('/').collect()
    }
}

/// Unwrap an `Option<T>` schema node: schemars renders it as an
/// `anyOf`/`oneOf` whose branches are `T` and `{"type":"null"}` — return the
/// resolved non-null branch. A node that is not such a wrapper is returned
/// as-is; `None` when the wrapper has no single non-null branch.
fn unwrap_optional<'a>(node: &'a Value, root: &'a Value) -> Option<&'a Value> {
    let branches = node
        .get("anyOf")
        .or_else(|| node.get("oneOf"))
        .and_then(Value::as_array);
    let Some(branches) = branches else {
        return Some(node);
    };
    let non_null: Vec<&Value> = branches
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) != Some("null"))
        .collect();
    match non_null.as_slice() {
        [only] => Some(resolve(only, root)),
        _ => None,
    }
}

/// Walk a JSON Schema into a flat, ordered list of scalar leaf [`FieldSpec`]s.
/// Resolves `$ref` into `$defs`, recurses plain objects, and **skips** any
/// `oneOf`/`anyOf`/`allOf`/`enum`/`const` subtree (optional nested objects,
/// tagged enums, custom-serde types) — those round-trip untouched through the
/// hidden blob, which is exactly how redacted secrets stay safe.
fn build_fields(schema: &Value) -> Vec<FieldSpec> {
    let mut out = Vec::new();
    walk_schema(schema, schema, "", &mut out);
    out
}

/// Resolve a single-level `#/$defs/...` `$ref` against the schema root. A node
/// with no `$ref` is returned as-is. (Sibling keys like `default`/`description`
/// on a `$ref` node are irrelevant to leaf classification, so they are dropped.)
fn resolve<'a>(node: &'a Value, root: &'a Value) -> &'a Value {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        if let Some(name) = reference.strip_prefix("#/$defs/") {
            if let Some(target) = root.pointer(&format!("/$defs/{name}")) {
                return target;
            }
        }
    }
    node
}

/// Classification of a (resolved) schema node.
enum Shape {
    /// A scalar leaf with its JSON base type and nullability.
    Scalar { base: String, nullable: bool },
    /// An array of enumerated integers — a checkbox-group leaf.
    IntEnumArray { options: Vec<i64> },
    /// A plain object to recurse into.
    Object,
    /// A composite/enum/custom subtree to skip (round-trips via the blob).
    Skip,
}

fn classify(node: &Value, root: &Value) -> Shape {
    // Composite or enum/const subtrees are never rendered as scalar inputs.
    // (An enum on an array's *items* is fine — that check is on the node
    // itself, and the integer-enum-array case below depends on it.)
    if ["oneOf", "anyOf", "allOf", "enum", "const"]
        .iter()
        .any(|k| node.get(k).is_some())
    {
        return Shape::Skip;
    }
    // `type` is either a string, or an array like ["integer","null"] for an
    // optional scalar.
    let (base, nullable) = match node.get("type") {
        Some(Value::String(s)) => (Some(s.clone()), false),
        Some(Value::Array(arr)) => {
            let nullable = arr.iter().any(|v| v.as_str() == Some("null"));
            let non_null: Vec<&str> = arr
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| *s != "null")
                .collect();
            let base = match non_null.as_slice() {
                [only] => Some((*only).to_string()),
                _ => None,
            };
            (base, nullable)
        }
        _ => (None, false),
    };
    match base.as_deref() {
        Some(t @ ("string" | "integer" | "number" | "boolean")) => Shape::Scalar {
            base: t.to_string(),
            nullable,
        },
        Some("object") => Shape::Object,
        // The one renderable array shape: items are an integer enum
        // (e.g. rp's `cooler_targets_c` grid) — a checkbox group. All
        // other arrays (objects, `$ref` items, un-enumerated scalars)
        // fall through to `Skip` and round-trip via the blob.
        Some("array") => int_enum_options(node, root)
            .map_or(Shape::Skip, |options| Shape::IntEnumArray { options }),
        // An object def may omit `type` but carry `properties`.
        None if node.get("properties").is_some() => Shape::Object,
        _ => Shape::Skip,
    }
}

/// The enumerated integer values of an array node's `items`, when that is
/// what they are: `items` (after `$ref` resolution) typed `integer` with a
/// non-empty all-integer `enum`.
fn int_enum_options(node: &Value, root: &Value) -> Option<Vec<i64>> {
    let items = resolve(node.get("items")?, root);
    if items.get("type").and_then(Value::as_str) != Some("integer") {
        return None;
    }
    let options: Vec<i64> = items
        .get("enum")?
        .as_array()?
        .iter()
        .map(Value::as_i64)
        .collect::<Option<Vec<i64>>>()?;
    (!options.is_empty()).then_some(options)
}

fn walk_schema(node: &Value, root: &Value, prefix: &str, out: &mut Vec<FieldSpec>) {
    let resolved = resolve(node, root);
    match classify(resolved, root) {
        Shape::Object => {
            if let Some(props) = resolved.get("properties").and_then(Value::as_object) {
                // Walk properties in sorted key order so the rendered field order
                // is deterministic *regardless* of serde_json's `preserve_order`
                // feature. `serde_json::Map` iterates sorted only when
                // `preserve_order` is off (BTreeMap) and in insertion order when
                // it is on (IndexMap) — and a dev-dependency (thirtyfour) unifies
                // that feature on under `--all-features`. Sorting here keeps the
                // output identical across that feature, build systems, and OSes.
                let mut props: Vec<_> = props.iter().collect();
                props.sort_by_key(|(name, _)| *name);
                for (name, child) in props {
                    let child_prefix = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    walk_schema(child, root, &child_prefix, out);
                }
            }
        }
        Shape::Scalar { base, nullable } => {
            if prefix.is_empty() {
                return; // a top-level scalar config is not a form field
            }
            out.push(make_field(prefix, &base, nullable, resolved));
        }
        Shape::IntEnumArray { options } => {
            if prefix.is_empty() {
                return; // a top-level array config is not a form field
            }
            out.push(field_spec(prefix, FieldKind::IntSet { options }));
        }
        Shape::Skip => {}
    }
}

fn make_field(name: &str, base: &str, nullable: bool, node: &Value) -> FieldSpec {
    let kind = match base {
        "boolean" => FieldKind::Bool,
        "number" => FieldKind::Num { nullable },
        "integer" => FieldKind::Int {
            nullable,
            min: node.get("minimum").and_then(Value::as_i64),
            max: node.get("maximum").and_then(Value::as_i64),
        },
        // "string" and any unexpected base fall back to a text field.
        _ => FieldKind::Str { nullable },
    };
    field_spec(name, kind)
}

fn field_spec(name: &str, kind: FieldKind) -> FieldSpec {
    let (section, sub) = match name.split_once('.') {
        Some((s, rest)) => (s.to_string(), rest.to_string()),
        None => (name.to_string(), name.to_string()),
    };
    FieldSpec {
        name: name.to_string(),
        pointer: dotted_to_pointer(name),
        section,
        label: humanize(&sub),
        kind,
    }
}

// --- rendering ----------------------------------------------------------------

/// The per-render state every field is rendered against.
struct FieldCtx<'a> {
    model: &'a FieldModel,
    config: &'a Value,
    overrides: &'a [String],
    unlocked: &'a [String],
    errors: &'a [FieldError],
}

/// Render a driver's configuration form from its schema-derived [`FieldModel`],
/// filled from the effective config.
///
/// Override-pinned and hard-read-only fields render disabled;
/// locked/identity fields render disabled behind an "unlock to edit"
/// escape hatch unless listed in `unlocked`; `errors` annotate fields
/// after a rejected apply.
#[must_use]
pub fn config_card(
    page: &Page<'_>,
    model: &FieldModel,
    config: &Value,
    overrides: &[String],
    unlocked: &[String],
    errors: &[FieldError],
    banner: Option<Banner>,
) -> Markup {
    // Sorted-key serialization so the hidden blob's bytes are deterministic
    // regardless of serde_json's `preserve_order` feature (see `canonical_json`
    // and the note in `walk_schema`). Key order is semantically irrelevant to the
    // round-trip, but it must be stable for the cross-OS byte snapshots (P2).
    let config_blob = canonical_json(config);
    let overrides_blob = serde_json::to_string(overrides).unwrap_or_default();
    // The unlocked set round-trips on POST so an invalid submission re-renders
    // with the identity field still unlocked (rather than snapping shut and
    // discarding the user's in-progress edit).
    let unlocked_blob = serde_json::to_string(unlocked).unwrap_or_default();
    let ctx = FieldCtx {
        model,
        config,
        overrides,
        unlocked,
        errors,
    };
    let action = format!("/config/{}", page.service);
    html! {
        div #config-card.card {
            @if let Some(b) = banner { (banner_markup(page, &b)) }
            h1 { (page.title) }
            p.subtitle { (page.subtitle) }
            // htmx-driven, JavaScript-required: the form submits via `hx-post`.
            // There is no non-JS `method`/`action` fallback — the UI requires htmx
            // (UI-testing plan §7; the genuine recovery path is ssh + edit the file).
            form hx-post=(action) hx-target="#config-card" hx-swap="outerHTML" {
                input type="hidden" name="__config" value=(config_blob);
                input type="hidden" name="__overrides" value=(overrides_blob);
                input type="hidden" name="__unlocked" value=(unlocked_blob);

                @for (section, fields) in group_by_section(&model.fields) {
                    fieldset {
                        legend { (humanize(section)) }
                        @for spec in fields { (render_field(page, &ctx, spec)) }
                    }
                }
                div.actions { button.primary type="submit" { "Apply" } }
            }
            // The recovery hammer (config-actions plan Phase 4): Sentinel owns
            // process restart; this posts to the BFF's restart route, which
            // calls Sentinel's REST API. Outside the form so the POST carries
            // no form data, and rendered only when a Sentinel is configured.
            @if page.can_restart {
                div.card-footer {
                    div.hint {
                        (format!(
                            "Sentinel can restart {}'s process — the recovery hammer \
                             for a wedged service, and how saved changes that need a \
                             process restart take effect.",
                            page.service
                        ))
                    }
                    (restart_button(page))
                }
            }
        }
    }
}

/// The "Restart via Sentinel" htmx button (JS-required, plan §7). The native
/// `hx-confirm` prompt guards against a stray click bouncing a healthy driver.
fn restart_button(page: &Page<'_>) -> Markup {
    let confirm = format!(
        "Restart {}'s process via Sentinel? In-flight operations are lost.",
        page.service
    );
    html! {
        button.restart-sentinel type="button" hx-post=(page.restart_url())
            hx-target="#config-card" hx-swap="outerHTML" hx-confirm=(confirm) {
            "Restart via Sentinel"
        }
    }
}

/// Group fields by their top-level section, preserving first-seen order.
fn group_by_section(fields: &[FieldSpec]) -> Vec<(&str, Vec<&FieldSpec>)> {
    let mut groups: Vec<(&str, Vec<&FieldSpec>)> = Vec::new();
    for f in fields {
        if let Some(group) = groups.iter_mut().find(|(s, _)| *s == f.section) {
            group.1.push(f);
        } else {
            groups.push((f.section.as_str(), vec![f]));
        }
    }
    groups
}

/// Why a rendered field is disabled (or how its identity lock stands),
/// in the same priority order the hint text follows: a pinned field
/// stays pinned even if it is also read-only or locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldGuard {
    /// Pinned by a command-line override.
    Pinned,
    /// Hard read-only.
    ReadOnly,
    /// Identity field, still locked.
    Locked,
    /// Identity field the operator unlocked this session.
    Unlocked,
    /// Freely editable.
    Editable,
}

impl FieldGuard {
    /// A locked/identity field is disabled until the user explicitly
    /// unlocks it; pinned and hard-read-only always disable regardless.
    const fn disables(self) -> bool {
        matches!(self, Self::Pinned | Self::ReadOnly | Self::Locked)
    }
}

fn render_field(page: &Page<'_>, ctx: &FieldCtx<'_>, spec: &FieldSpec) -> Markup {
    let name = &spec.name;
    let guard = if ctx.overrides.iter().any(|o| o == name) {
        FieldGuard::Pinned
    } else if ctx.model.is_read_only(name) {
        FieldGuard::ReadOnly
    } else if ctx.model.is_locked(name) {
        if ctx.unlocked.iter().any(|u| u == name) {
            FieldGuard::Unlocked
        } else {
            FieldGuard::Locked
        }
    } else {
        FieldGuard::Editable
    };
    let disabled = guard.disables();
    let err = ctx.errors.iter().find(|e| &e.path == name);
    html! {
        div.field.pinned[disabled].invalid[err.is_some()] {
            @match &spec.kind {
                FieldKind::Bool => {
                    div.checkbox {
                        input type="checkbox" id=(name) name=(name)
                            checked[bool_at(ctx.config, &spec.pointer)] disabled[disabled];
                        label for=(name) { (spec.label) }
                    }
                }
                // One checkbox per enumerated value, in schema order; all
                // share the field's `name`, so the checked values arrive as
                // repeated form pairs (see `FormValues`). The group heading
                // is a span (a bare `label` would reference no control);
                // `role="group"` + `aria-label` name the group for
                // assistive tech, and each member keeps its own real label.
                FieldKind::IntSet { options } => {
                    span.group-label { (spec.label) }
                    div.checkbox-group role="group" aria-label=(spec.label) {
                        @for option in options {
                            @let option_id = format!("{name}.{option}");
                            div.checkbox {
                                input type="checkbox" id=(option_id) name=(name) value=(option)
                                    checked[array_contains(ctx.config, &spec.pointer, *option)]
                                    disabled[disabled];
                                label for=(option_id) { (option) }
                            }
                        }
                    }
                }
                _ => {
                    label for=(name) { (spec.label) }
                    input type=(spec.input_type()) id=(name) name=(name)
                        value=(str_at(ctx.config, &spec.pointer)) disabled[disabled];
                }
            }
            @if let Some(e) = err { div.error { (e.msg) } }
            (field_hints(page, spec, guard))
        }
    }
}

fn field_hints(page: &Page<'_>, spec: &FieldSpec, guard: FieldGuard) -> Markup {
    let unlock_href = format!("/config/{}?unlock={}", page.service, spec.name);
    let lock_href = format!("/config/{}", page.service);
    html! {
        @if guard == FieldGuard::Pinned {
            div.hint { "Pinned by a command-line override; change the driver's launch flags to edit it." }
        } @else if guard == FieldGuard::ReadOnly {
            div.hint {
                "Read-only for now — editing it here would lose the connection to "
                "the driver. Change it in the driver's configuration file."
            }
        } @else if guard == FieldGuard::Locked {
            div.hint {
                "Identity — the driver owns this. Editing is an escape hatch "
                "for a misbehaving driver. "
                // A link-styled htmx button (JS-required, no `href` fallback —
                // plan §7). `type="button"` is load-bearing: this renders inside
                // the form, so a default-type button would submit it on click.
                button.link type="button" hx-get=(unlock_href)
                    hx-target="#config-card" hx-swap="outerHTML" { "Unlock to edit" }
            }
        } @else if guard == FieldGuard::Unlocked {
            div.hint.warning {
                "Unlocked — editing the driver's identity is an escape hatch "
                "for a misbehaving driver. "
                button.link type="button" hx-get=(lock_href)
                    hx-target="#config-card" hx-swap="outerHTML" { "Lock again" }
            }
        }
    }
}

/// The "applying — reconnecting" fragment: polls `…/status` once a second until
/// the driver answers and the poll swaps in a fresh card.
#[must_use]
pub fn reconnecting_card(service: &str) -> Markup {
    polling_card(service, "Saved — the driver is reloading. Reconnecting…")
}

/// The restart-accepted fragment: Sentinel restarted the driver's unit
/// through the platform service manager, so the driver's process is
/// coming back — same poll wiring as the reload flow.
///
/// `recovery_timed_out` adds that Sentinel's recovery check never
/// confirmed recovery within its budget (the poll may still succeed —
/// the budget is Sentinel's, not the driver's).
#[must_use]
pub fn restarting_card(service: &str, recovery_timed_out: bool) -> Markup {
    let message = if recovery_timed_out {
        "Restart requested via Sentinel, but its health check did not confirm \
         recovery within the budget. Reconnecting anyway…"
    } else {
        "Restart requested via Sentinel — the driver is restarting. Reconnecting…"
    };
    polling_card(service, message)
}

/// Shared skeleton of the reconnect-polling fragments: polls `…/status` once a
/// second until the driver answers and the poll swaps in a fresh card.
fn polling_card(service: &str, message: &str) -> Markup {
    let status_url = format!("/config/{service}/status");
    html! {
        div #config-card.card hx-get=(status_url) hx-trigger="every 1s"
            hx-swap="outerHTML" hx-target="this" {
            div class="banner applying" {
                span.dot {}
                span { (message) }
            }
        }
    }
}

/// An error card derived from a `ConfigClientError`, with a retry affordance.
#[must_use]
pub fn error_card(service: &str, err: &ConfigClientError) -> Markup {
    let message = if err.is_action_not_implemented() {
        "This driver does not expose configuration actions.".to_string()
    } else {
        err.to_string()
    };
    error_card_with_message(service, &message)
}

/// An error card with an explicit message (e.g. a malformed form submission).
#[must_use]
pub fn message_error_card(service: &str, message: &str) -> Markup {
    error_card_with_message(service, message)
}

/// An error card for a request that named a service the BFF doesn't know.
#[must_use]
pub fn unknown_service_card(service: &str) -> Markup {
    html! {
        div #config-card.card {
            div class="banner error" {
                span.dot {}
                span { (format!("No configured driver named \"{service}\".")) }
            }
            p { a href="/" { "Back to configuration" } }
        }
    }
}

/// The card for a `/config/{service}` key that resolved to no usable driver.
/// Each [`crate::ResolveError`] cause gets its own honest message — an
/// unreachable rp or an unusable roster entry is not "no such driver".
pub(crate) fn resolve_failure_card(service: &str, err: &crate::ResolveError) -> Markup {
    match err {
        crate::ResolveError::Unknown => unknown_service_card(service),
        crate::ResolveError::RpUnreachable(e) => error_card_with_message(
            service,
            &format!("Could not read rp's roster to resolve \"{service}\": {e}"),
        ),
        crate::ResolveError::BadRosterEntry(e) => html! {
            div #config-card.card {
                div class="banner error" {
                    span.dot {}
                    span { (format!("This device's roster entry can't be used: {e}")) }
                }
                p { a href="/equipment" { "Fix it on the Equipment page" } }
            }
        },
    }
}

fn error_card_with_message(service: &str, message: &str) -> Markup {
    let retry = format!("/config/{service}");
    html! {
        div #config-card.card {
            div class="banner error" { span.dot {} span { (message) } }
            p {
                // A link-styled htmx button (JS-required, no `href` fallback — §7).
                button.link type="button" hx-get=(retry) hx-target="#config-card"
                    hx-swap="outerHTML" { "Retry" }
            }
        }
    }
}

fn simple_banner(kind: &str, text: &str) -> Markup {
    html! {
        div class=(format!("banner {kind}")) { span.dot {} span { (text) } }
    }
}

fn banner_markup(page: &Page<'_>, banner: &Banner) -> Markup {
    match banner {
        // The restart callout lists the pending paths — persisted, but only in
        // effect after the target's next process start. When a Sentinel is
        // configured, the restart affordance attaches inline so the operator
        // can act on the callout directly.
        Banner::SavedRestartRequired(paths) => html! {
            div class="banner warn" {
                span.dot {}
                span {
                    (format!("Saved. These changes take effect when {} is restarted: ", page.service))
                    span.mono { (paths.join(", ")) }
                    @if page.can_restart { " " (restart_button(page)) }
                }
            }
        },
        Banner::Saved => simple_banner("ok", "Saved. No reload was needed."),
        Banner::Invalid => simple_banner(
            "error",
            "Some values were rejected. Fix the highlighted fields and apply again.",
        ),
        Banner::Reconnected => simple_banner(
            "ok",
            "Reconnected. The driver reloaded with the new configuration.",
        ),
    }
}

// --- form ⇆ config mapping -----------------------------------------------------

/// The merged config produced from a submitted form, ready to send to
/// `config.apply`, plus the override-pinned paths (echoed back so a re-render
/// keeps those fields disabled).
#[derive(Debug)]
pub struct MergedForm {
    pub config: Value,
    pub overrides: Vec<String>,
    /// The locked/identity fields that were unlocked on this submission (read
    /// back from the hidden `__unlocked` field). Echoed so a re-render after an
    /// invalid apply keeps them unlocked.
    pub unlocked: Vec<String>,
    /// BFF-side parse/range errors for numeric fields (e.g. a port above its
    /// schema maximum). When non-empty, the form is re-rendered with these field
    /// errors rather than sent to the driver.
    pub errors: Vec<FieldError>,
}

/// A malformed form submission (a missing or unparseable hidden field).
///
/// Both required hidden fields are always emitted by [`config_card`],
/// so their absence or corruption means the submission did not come
/// from a rendered page.
#[derive(Debug, thiserror::Error)]
pub enum FormError {
    #[error("the form was missing the hidden configuration field")]
    MissingConfig,
    #[error("the hidden configuration field was not valid JSON: {0}")]
    BadConfig(String),
    #[error("the form was missing the hidden overrides field")]
    MissingOverrides,
    #[error("the hidden overrides field was not valid JSON: {0}")]
    BadOverrides(String),
}

/// Submitted form pairs with duplicate keys preserved.
///
/// A checkbox group posts one `name=value` pair per checked box, and
/// `serde_urlencoded` collapses duplicates when decoding into a map —
/// so the handlers extract `Form<Vec<(String, String)>>` and wrap the
/// pairs here. Single-value lookups take the first occurrence.
#[derive(Debug, Default)]
pub struct FormValues(Vec<(String, String)>);

impl From<Vec<(String, String)>> for FormValues {
    fn from(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }
}

impl FormValues {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn contains_key(&self, name: &str) -> bool {
        self.0.iter().any(|(k, _)| k == name)
    }

    /// Replace every pair under `name` with a single `value` (appending
    /// when absent) — the equipment entry forms re-seed `__config` this way.
    pub fn set(&mut self, name: &str, value: String) {
        self.0.retain(|(k, _)| k != name);
        self.0.push((name.to_string(), value));
    }
}

/// Rebuild the full Config from a submitted form: start from the hidden
/// round-tripped blob and overlay each editable schema leaf by JSON pointer.
///
/// Override-pinned, hard-read-only, and not-unlocked locked fields are
/// not overlaid (they round-trip from the blob); schema subtrees the
/// model skipped (`oneOf`/`anyOf`/secrets) round-trip untouched too.
///
/// # Errors
///
/// Returns a [`FormError`] if either required hidden field
/// (`__config`, `__overrides`) is missing or not valid JSON.
pub fn merge_form(form: &FormValues, model: &FieldModel) -> Result<MergedForm, FormError> {
    let raw = form.get("__config").ok_or(FormError::MissingConfig)?;
    let mut config: Value =
        serde_json::from_str(raw).map_err(|e| FormError::BadConfig(e.to_string()))?;
    // `__overrides` is required and validated like `__config`: a malformed value
    // would otherwise be silently treated as "no overrides", letting pinned
    // fields be overlaid on a re-render instead of surfacing the bad submission.
    let overrides_raw = form.get("__overrides").ok_or(FormError::MissingOverrides)?;
    let overrides: Vec<String> =
        serde_json::from_str(overrides_raw).map_err(|e| FormError::BadOverrides(e.to_string()))?;

    // `__unlocked` is optional; a missing or malformed value means "nothing
    // unlocked" (the safe default — locked fields stay read-only), and the set is
    // filtered to the schema's locked fields so a forged value can only ever
    // unlock a genuine identity field.
    let unlocked = unlocked_set_from_json(model, form.get("__unlocked"));

    let is_pinned = |name: &str| overrides.iter().any(|o| o == name);
    let is_unlocked = |name: &str| unlocked.iter().any(|u| u == name);

    let mut errors = Vec::new();
    for spec in &model.fields {
        if is_pinned(&spec.name) || model.is_read_only(&spec.name) {
            continue;
        }
        // A locked/identity field round-trips from the blob untouched unless the
        // user explicitly unlocked it (and it isn't pinned).
        if model.is_locked(&spec.name) && !is_unlocked(&spec.name) {
            continue;
        }
        overlay_field(&mut config, spec, form, &mut errors);
    }

    Ok(MergedForm {
        config,
        overrides,
        unlocked,
        errors,
    })
}

/// Overlay one editable schema leaf from the submitted form onto the
/// config blob ([`merge_form`] applies the pinned/read-only/locked
/// skip rules before calling this).
fn overlay_field(
    config: &mut Value,
    spec: &FieldSpec,
    form: &FormValues,
    errors: &mut Vec<FieldError>,
) {
    match &spec.kind {
        // A checkbox submits its name only when checked: present ⇒ true.
        // (Only reached for editable booleans; read-only ones are skipped
        // by the caller and round-trip from the blob.)
        FieldKind::Bool => {
            set_pointer(
                config,
                &spec.pointer,
                Value::Bool(form.contains_key(&spec.name)),
            );
        }
        // A checkbox group submits one pair per checked box; absent
        // pairs mean nothing is checked ⇒ an empty array (same
        // present/absent semantics as `Bool`). Values are written in
        // `options` (schema `enum`) order, which also dedupes.
        FieldKind::IntSet { options } => {
            let selected: Result<Vec<i64>, ()> = form
                .get_all(&spec.name)
                .map(|raw| match raw.trim().parse::<i64>() {
                    Ok(n) if options.contains(&n) => Ok(n),
                    _ => Err(()),
                })
                .collect();
            match selected {
                Ok(selected) => {
                    let chosen: Vec<Value> = options
                        .iter()
                        .filter(|o| selected.contains(o))
                        .map(|o| Value::from(*o))
                        .collect();
                    set_pointer(config, &spec.pointer, Value::Array(chosen));
                }
                Err(()) => {
                    errors.push(field_error(
                        &spec.name,
                        "must be values from the allowed set",
                    ));
                }
            }
        }
        FieldKind::Str { nullable } => {
            if let Some(raw) = form.get(&spec.name) {
                // Optional: clear to null, the same gesture the number
                // kinds honour below. `""` is not a spelling of "unset" —
                // a driver that distinguishes the two (rp's naming
                // patterns: absent means a documented default, empty
                // means malformed) must receive the one the operator
                // meant. Required: keep writing the empty string, so a
                // driver that validates its own required fields still
                // gets to say so.
                if *nullable && raw.trim().is_empty() {
                    set_pointer(config, &spec.pointer, Value::Null);
                } else {
                    set_pointer(config, &spec.pointer, Value::String(raw.to_string()));
                }
            }
        }
        FieldKind::Int { nullable, min, max } => {
            let Some(raw) = form.get(&spec.name) else {
                return;
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                // Optional: clear to null. Required: keep the prior value
                // (clearing a port must not silently become 0).
                if *nullable {
                    set_pointer(config, &spec.pointer, Value::Null);
                }
            } else {
                match trimmed.parse::<i64>() {
                    Ok(n) if min.is_none_or(|lo| n >= lo) && max.is_none_or(|hi| n <= hi) => {
                        set_pointer(config, &spec.pointer, Value::from(n));
                    }
                    _ => errors.push(field_error(&spec.name, &int_error(*min, *max))),
                }
            }
        }
        FieldKind::Num { nullable } => {
            let Some(raw) = form.get(&spec.name) else {
                return;
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                if *nullable {
                    set_pointer(config, &spec.pointer, Value::Null);
                }
            } else {
                match trimmed.parse::<f64>() {
                    Ok(n) => set_pointer(config, &spec.pointer, Value::from(n)),
                    Err(_) => errors.push(field_error(&spec.name, "must be a number")),
                }
            }
        }
    }
}

fn int_error(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(lo), Some(hi)) => format!("must be a whole number between {lo} and {hi}"),
        _ => "must be a whole number".to_string(),
    }
}

/// Parse a JSON string array of field names into the set of currently-unlocked
/// locked/identity fields, keeping only names that are actually locked in the
/// schema. `None`, empty input, or any parse failure yields an empty set.
fn unlocked_set_from_json(model: &FieldModel, raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let names: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    names.into_iter().filter(|n| model.is_locked(n)).collect()
}

/// Compute the unlocked set from a `?unlock=<field>` query value. Only a name
/// that is actually a locked/identity field (per the schema) is honoured.
#[must_use]
pub fn unlocked_from_query(model: &FieldModel, unlock: Option<&str>) -> Vec<String> {
    match unlock {
        Some(name) if model.is_locked(name) => vec![name.to_string()],
        _ => Vec::new(),
    }
}

// --- small JSON helpers --------------------------------------------------------

/// Serialize `value` to JSON with object keys sorted recursively, so the bytes
/// are deterministic regardless of `serde_json`'s `preserve_order` feature (which
/// changes `Value` map ordering and is unified on by a dev-dependency under
/// `--all-features`). Inserting keys in sorted order is stable under both the
/// `IndexMap` (`preserve_order`) and `BTreeMap` (default) `Map` backends.
fn canonical_json(value: &Value) -> String {
    fn sort_keys(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = serde_json::Map::new();
                for key in keys {
                    sorted.insert(key.clone(), sort_keys(&map[key]));
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort_keys(value)).unwrap_or_default()
}

fn str_at(config: &Value, pointer: &str) -> String {
    match config.pointer(pointer) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn bool_at(config: &Value, pointer: &str) -> bool {
    config
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the config array at `pointer` contains the integer `value` —
/// drives the `checked` state of a checkbox-group member.
fn array_contains(config: &Value, pointer: &str, value: i64) -> bool {
    config
        .pointer(pointer)
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|v| v.as_i64() == Some(value)))
}

fn field_error(path: &str, msg: &str) -> FieldError {
    FieldError {
        path: path.to_string(),
        msg: msg.to_string(),
    }
}

fn set_pointer(config: &mut Value, pointer: &str, value: Value) {
    if let Some(slot) = config.pointer_mut(pointer) {
        *slot = value;
    }
}

/// Convert a dotted path (`serial.port`) to an RFC-6901 JSON pointer (`/serial/port`).
fn dotted_to_pointer(dotted: &str) -> String {
    let mut pointer = String::with_capacity(dotted.len().saturating_add(1));
    pointer.push('/');
    pointer.push_str(&dotted.replace('.', "/"));
    pointer
}

/// Turn a dotted, `snake_case` sub-path into a readable label, e.g.
/// `dec_limits.max_degrees` → `Dec limits · Max degrees`.
fn humanize(path: &str) -> String {
    path.split('.')
        .map(|seg| {
            let spaced = seg.replace('_', " ");
            let mut chars = spaced.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    /// A representative dsd-fp2-shaped `config.schema` response: `$defs` + `$ref`
    /// (so the walker's ref-resolution is exercised), an `anyOf` optional struct
    /// (`server.auth`, skipped), a nullable optional int (`server.discovery_port`),
    /// and the three editability tiers the driver reports.
    fn sample_schema() -> ConfigSchemaResponse {
        ConfigSchemaResponse {
            schema: json!({
                "$defs": {
                    "AuthConfig": {
                        "type": "object",
                        "properties": { "password_hash": { "type": "string" } },
                    },
                    "SerialConfig": {
                        "type": "object",
                        "properties": {
                            "port": { "type": "string" },
                            "baud_rate": { "type": "integer", "format": "uint32", "minimum": 0 },
                            "polling_interval": { "type": "string" },
                            "timeout": { "type": "string" },
                        },
                    },
                    "ServerConfig": {
                        "type": "object",
                        "properties": {
                            "port": { "type": "integer", "format": "uint16", "minimum": 0, "maximum": 65535 },
                            "discovery_port": { "type": ["integer", "null"], "format": "uint16", "minimum": 0, "maximum": 65535 },
                            "auth": { "anyOf": [ { "$ref": "#/$defs/AuthConfig" }, { "type": "null" } ] },
                        },
                    },
                    "CoverCalibratorConfig": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "unique_id": { "type": "string" },
                            "description": { "type": "string" },
                            "enabled": { "type": "boolean" },
                            "max_brightness": { "type": "integer", "format": "uint32", "minimum": 0 },
                        },
                    },
                },
                "type": "object",
                "properties": {
                    "serial": { "$ref": "#/$defs/SerialConfig" },
                    "server": { "$ref": "#/$defs/ServerConfig" },
                    "cover_calibrator": { "$ref": "#/$defs/CoverCalibratorConfig" },
                },
            }),
            locked_fields: vec!["cover_calibrator.unique_id".to_string()],
            read_only_fields: vec![
                "server.port".to_string(),
                "cover_calibrator.enabled".to_string(),
            ],
        }
    }

    fn sample_model() -> FieldModel {
        FieldModel::from_schema(&sample_schema())
    }

    /// An rp-shaped config schema: `equipment` with an array kind (`cameras`,
    /// items behind a `$ref`) and the optional singular `mount`
    /// (`anyOf [$ref, null]` — the `Option<MountConfig>` shape schemars emits).
    fn rp_like_schema() -> ConfigSchemaResponse {
        ConfigSchemaResponse {
            schema: json!({
                "$defs": {
                    "CameraConfig": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "alpaca_url": { "type": "string" },
                            "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                            "gain": { "type": "integer", "format": "uint32", "minimum": 0 },
                            // The integer-enum array shape (rp's dark-library
                            // cooler grid) — renders as a checkbox group.
                            "cooler_targets_c": {
                                "type": "array",
                                "items": { "type": "integer", "enum": [-15, -10, -5, 0, 5] },
                                "uniqueItems": true,
                            },
                            "auth": { "anyOf": [ { "$ref": "#/$defs/DeviceAuth" }, { "type": "null" } ] },
                        },
                    },
                    "DeviceAuth": {
                        "type": "object",
                        "properties": { "username": { "type": "string" }, "password": { "type": "string" } },
                    },
                    "MountConfig": {
                        "type": "object",
                        "properties": {
                            "alpaca_url": { "type": "string" },
                            "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                        },
                    },
                    "EquipmentConfig": {
                        "type": "object",
                        "properties": {
                            "cameras": { "type": "array", "items": { "$ref": "#/$defs/CameraConfig" } },
                            "mount": { "anyOf": [ { "$ref": "#/$defs/MountConfig" }, { "type": "null" } ] },
                        },
                    },
                },
                "type": "object",
                "properties": {
                    "equipment": { "$ref": "#/$defs/EquipmentConfig" },
                    "server": { "type": "object", "properties": { "port": { "type": "integer" } } },
                    // rp's `session`: a required string beside an
                    // `Option<String>`, the pair that makes the nullable
                    // distinction observable.
                    "session": {
                        "type": "object",
                        "properties": {
                            "data_directory": { "type": "string" },
                            "file_naming_pattern": { "type": ["string", "null"] },
                        },
                    },
                },
            }),
            locked_fields: vec![],
            read_only_fields: vec!["server.port".to_string()],
        }
    }

    #[test]
    fn from_item_schema_walks_an_array_kind_with_relative_names() {
        let model =
            FieldModel::from_item_schema(&rp_like_schema(), "cameras").expect("cameras item model");
        let names: Vec<&str> = model.fields.iter().map(|f| f.name.as_str()).collect();
        // Relative, sorted leaf names; the optional `auth` subtree is skipped
        // (anyOf) exactly like on the config pages, while the integer-enum
        // array (`cooler_targets_c`) is a renderable leaf.
        assert_eq!(
            names,
            vec![
                "alpaca_url",
                "cooler_targets_c",
                "device_number",
                "gain",
                "id"
            ]
        );
        // Item models carry no tiers — the entry form has no read-only fields.
        assert_eq!(model.locked, Vec::<String>::new());
        assert_eq!(model.read_only, Vec::<String>::new());
    }

    #[test]
    fn walker_classifies_an_integer_enum_array_as_a_checkbox_group() {
        let model =
            FieldModel::from_item_schema(&rp_like_schema(), "cameras").expect("cameras item model");
        let spec = model
            .fields
            .iter()
            .find(|f| f.name == "cooler_targets_c")
            .expect("cooler_targets_c must be a walked leaf");
        assert_eq!(
            spec.kind,
            FieldKind::IntSet {
                options: vec![-15, -10, -5, 0, 5]
            },
            "options must carry the schema enum in its declared order"
        );
    }

    #[test]
    fn walker_still_skips_object_arrays() {
        // rp's `equipment.cameras` is an array of objects — it must keep
        // round-tripping via the blob, not render.
        let model = FieldModel::from_schema(&rp_like_schema());
        assert!(
            !model
                .fields
                .iter()
                .any(|f| f.name.starts_with("equipment.cameras")),
            "object arrays must stay skipped: {:?}",
            model.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
    }

    /// A minimal schema with one integer-enum array leaf at a nested path,
    /// for the checkbox-group render + merge tests.
    fn grid_schema() -> ConfigSchemaResponse {
        ConfigSchemaResponse {
            schema: json!({
                "type": "object",
                "properties": {
                    "cooling": {
                        "type": "object",
                        "properties": {
                            "targets": {
                                "type": "array",
                                "items": { "type": "integer", "enum": [-10, -5, 0] },
                                "uniqueItems": true,
                            },
                        },
                    },
                },
            }),
            locked_fields: vec![],
            read_only_fields: vec![],
        }
    }

    fn grid_model() -> FieldModel {
        FieldModel::from_schema(&grid_schema())
    }

    #[test]
    fn config_card_renders_a_checkbox_group_with_checked_members() {
        let model = grid_model();
        let config = json!({ "cooling": { "targets": [-10, 0] } });
        let markup = config_card(&page(), &model, &config, &[], &[], &[], None).into_string();
        // One checkbox per enum value, sharing the field name, with a
        // per-value id; the values present in the config render checked.
        for (id, value, checked) in [
            ("cooling.targets.-10", "-10", true),
            ("cooling.targets.-5", "-5", false),
            ("cooling.targets.0", "0", true),
        ] {
            let tag_start = format!(r#"id="{id}""#);
            let tag = markup
                .split("<input")
                .find(|frag| frag.contains(&tag_start))
                .unwrap_or_else(|| panic!("missing checkbox {id}:\n{markup}"));
            assert!(tag.contains(r#"name="cooling.targets""#), "{tag}");
            assert!(tag.contains(&format!(r#"value="{value}""#)), "{tag}");
            assert_eq!(
                tag.contains("checked"),
                checked,
                "checkbox {id} checked-state mismatch: {tag}"
            );
        }
    }

    #[test]
    fn form_values_preserves_duplicates_and_set_replaces_them() {
        let mut form = FormValues::from(vec![
            ("targets".to_string(), "-10".to_string()),
            ("other".to_string(), "x".to_string()),
            ("targets".to_string(), "0".to_string()),
        ]);
        assert_eq!(form.get("targets"), Some("-10"), "get takes the first pair");
        assert_eq!(
            form.get_all("targets").collect::<Vec<_>>(),
            vec!["-10", "0"]
        );
        assert!(form.contains_key("other"));
        assert!(!form.contains_key("absent"));
        assert_eq!(form.get("absent"), None);

        form.set("targets", "5".to_string());
        assert_eq!(form.get_all("targets").collect::<Vec<_>>(), vec!["5"]);
        form.set("fresh", "1".to_string());
        assert_eq!(form.get("fresh"), Some("1"), "set appends when absent");
    }

    #[test]
    fn merge_form_collects_checkbox_group_values_in_enum_order() {
        let form = form_from(&[
            ("__config", r#"{"cooling":{"targets":[0]}}"#),
            ("__overrides", "[]"),
            // Submitted out of enum order — the merge normalizes.
            ("cooling.targets", "0"),
            ("cooling.targets", "-10"),
        ]);
        let merged = merge_form(&form, &grid_model()).unwrap();
        assert_eq!(
            merged.config.pointer("/cooling/targets"),
            Some(&json!([-10, 0])),
            "checked values must be written back in schema enum order"
        );
        assert!(merged.errors.is_empty(), "{:?}", merged.errors);
    }

    #[test]
    fn merge_form_empty_checkbox_group_becomes_empty_array() {
        // Nothing checked ⇒ no pairs under the field name ⇒ empty array
        // (same present/absent semantics as a boolean checkbox).
        let form = form_from(&[
            ("__config", r#"{"cooling":{"targets":[-10,0]}}"#),
            ("__overrides", "[]"),
        ]);
        let merged = merge_form(&form, &grid_model()).unwrap();
        assert_eq!(
            merged.config.pointer("/cooling/targets"),
            Some(&json!([])),
            "unchecking every box must clear the array, not keep the prior value"
        );
    }

    #[test]
    fn merge_form_checkbox_group_value_outside_the_enum_is_a_field_error() {
        let form = form_from(&[
            ("__config", r#"{"cooling":{"targets":[]}}"#),
            ("__overrides", "[]"),
            ("cooling.targets", "-10"),
            ("cooling.targets", "7"),
        ]);
        let merged = merge_form(&form, &grid_model()).unwrap();
        assert_eq!(merged.errors.len(), 1, "{:?}", merged.errors);
        assert_eq!(merged.errors[0].path, "cooling.targets");
        assert_eq!(
            merged.config.pointer("/cooling/targets"),
            Some(&json!([])),
            "a rejected submission must leave the blob value untouched"
        );
    }

    #[test]
    fn from_item_schema_unwraps_the_optional_singular_mount() {
        let model =
            FieldModel::from_item_schema(&rp_like_schema(), "mount").expect("mount item model");
        let names: Vec<&str> = model.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["alpaca_url", "device_number"]);
    }

    #[test]
    fn from_item_schema_unknown_kind_is_none() {
        assert!(FieldModel::from_item_schema(&rp_like_schema(), "rotators").is_none());
        // A schema without an equipment block at all.
        let bare = ConfigSchemaResponse {
            schema: json!({ "type": "object", "properties": {} }),
            locked_fields: vec![],
            read_only_fields: vec![],
        };
        assert!(FieldModel::from_item_schema(&bare, "cameras").is_none());
    }

    fn sample_config() -> Value {
        json!({
            "serial": { "port": "/dev/ttyACM0", "baud_rate": 115_200, "polling_interval": "500ms", "timeout": "3s" },
            "server": { "port": 11119, "discovery_port": 32227, "tls": null, "auth": null },
            "cover_calibrator": { "name": "FP2", "unique_id": "dsd-fp2-001", "description": "panel", "enabled": true, "max_brightness": 4096 }
        })
    }

    fn page() -> Page<'static> {
        Page {
            service: "dsd-fp2",
            title: "Deep Sky Dad FP2",
            subtitle: "dsd-fp2 · covercalibrator",
            can_restart: false,
        }
    }

    fn card(unlocked: &[String], errors: &[FieldError], banner: Option<Banner>) -> String {
        let model = sample_model();
        config_card(
            &page(),
            &model,
            &sample_config(),
            &[],
            unlocked,
            errors,
            banner,
        )
        .into_string()
    }

    fn form_from(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    /// The `<input ...>` tag whose `name` attribute is `name`.
    fn input_tag(markup: &str, name: &str) -> String {
        let pos = markup.find(&format!(r#"name="{name}""#)).unwrap();
        let start = markup
            .get(..pos)
            .and_then(|head| head.rfind("<input"))
            .unwrap();
        let tag = markup.get(start..).unwrap();
        let end = tag.find('>').unwrap();
        tag.get(..=end).unwrap().to_string()
    }

    // --- schema walker -------------------------------------------------------

    #[test]
    fn walker_produces_scalar_leaves_and_skips_anyof() {
        let model = sample_model();
        let names: Vec<&str> = model.fields.iter().map(|f| f.name.as_str()).collect();
        // Plain scalar leaves are rendered (including the nullable optional int)…
        for expected in [
            "serial.port",
            "serial.baud_rate",
            "server.port",
            "server.discovery_port",
            "cover_calibrator.unique_id",
            "cover_calibrator.enabled",
            "cover_calibrator.max_brightness",
        ] {
            assert!(
                names.contains(&expected),
                "missing leaf {expected}: {names:?}"
            );
        }
        // …but the anyOf optional struct (a secret-bearing subtree) is skipped.
        assert!(
            !names.iter().any(|n| n.starts_with("server.auth")),
            "server.auth should be skipped (round-trips via blob): {names:?}"
        );
    }

    #[test]
    fn walker_infers_field_kinds() {
        let model = sample_model();
        let kind = |name: &str| {
            model
                .fields
                .iter()
                .find(|f| f.name == name)
                .unwrap()
                .kind
                .clone()
        };
        assert_eq!(kind("serial.port"), FieldKind::Str { nullable: false });
        assert_eq!(kind("cover_calibrator.enabled"), FieldKind::Bool);
        assert_eq!(
            kind("server.discovery_port"),
            FieldKind::Int {
                nullable: true,
                min: Some(0),
                max: Some(65535)
            }
        );
        assert_eq!(
            kind("server.port"),
            FieldKind::Int {
                nullable: false,
                min: Some(0),
                max: Some(65535)
            }
        );
    }

    // --- rendering -----------------------------------------------------------

    #[test]
    fn config_card_embeds_current_values_and_hidden_blob() {
        let markup = card(&[], &[], None);
        assert!(markup.contains(r#"value="/dev/ttyACM0""#), "{markup}");
        assert!(markup.contains(r#"value="4096""#), "{markup}");
        assert!(markup.contains(r#"name="__config""#), "{markup}");
        assert!(markup.contains(r#"name="__unlocked""#), "{markup}");
        // Form posts to the service-scoped route.
        assert!(markup.contains(r#"hx-post="/config/dsd-fp2""#), "{markup}");
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        // Under the test build serde_json's `preserve_order` is unified on (a
        // dev-dependency requires it), so a Value built in this order would
        // otherwise serialize unsorted. canonical_json must still sort — at every
        // depth, including inside arrays — so the hidden blob's bytes are stable.
        let value = json!({ "b": 1, "a": { "y": 2, "x": 3 }, "c": [ { "n": 1, "m": 2 } ] });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"x":3,"y":2},"b":1,"c":[{"m":2,"n":1}]}"#
        );
    }

    #[test]
    fn config_card_disables_override_pinned_fields() {
        let model = sample_model();
        let overrides = vec!["serial.port".to_string()];
        let markup = config_card(
            &page(),
            &model,
            &sample_config(),
            &overrides,
            &[],
            &[],
            None,
        )
        .into_string();
        let tag = input_tag(&markup, "serial.port");
        assert!(tag.contains("disabled"), "serial.port not disabled: {tag}");
        assert!(
            markup.contains("Pinned by a command-line override"),
            "{markup}"
        );
    }

    #[test]
    fn config_card_shows_field_errors() {
        let errors = vec![FieldError {
            path: "serial.baud_rate".to_string(),
            msg: "must be greater than 0".to_string(),
        }];
        let markup = card(&[], &errors, Some(Banner::Invalid));
        assert!(markup.contains("must be greater than 0"), "{markup}");
        assert!(markup.contains("invalid"), "{markup}");
    }

    #[test]
    fn config_card_renders_enabled_read_only_via_schema_tier() {
        // `cover_calibrator.enabled` is in the schema's read_only_fields, so the
        // checkbox renders disabled — no hardcoded list needed.
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "cover_calibrator.enabled");
        assert!(
            tag.contains("disabled"),
            "enabled checkbox not disabled: {tag}"
        );
        assert!(
            tag.contains(r#"type="checkbox""#),
            "enabled not a checkbox: {tag}"
        );
    }

    #[test]
    fn config_card_renders_server_port_read_only_via_schema_tier() {
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "server.port");
        assert!(tag.contains("disabled"), "server.port not disabled: {tag}");
    }

    #[test]
    fn config_card_renders_unique_id_locked_by_default() {
        let markup = card(&[], &[], None);
        let tag = input_tag(&markup, "cover_calibrator.unique_id");
        assert!(tag.contains("disabled"), "unique_id not disabled: {tag}");
        assert!(
            markup.contains("Identity — the driver owns this"),
            "missing identity hint:\n{markup}"
        );
        assert!(
            markup.contains(r#"hx-get="/config/dsd-fp2?unlock=cover_calibrator.unique_id""#),
            "missing unlock link:\n{markup}"
        );
    }

    #[test]
    fn config_card_renders_unique_id_editable_when_unlocked() {
        let unlocked = vec!["cover_calibrator.unique_id".to_string()];
        let markup = card(&unlocked, &[], None);
        let tag = input_tag(&markup, "cover_calibrator.unique_id");
        assert!(!tag.contains("disabled"), "unique_id still disabled: {tag}");
        assert!(
            markup
                .contains(r#"name="__unlocked" value="[&quot;cover_calibrator.unique_id&quot;]""#),
            "missing/empty __unlocked hidden field:\n{markup}"
        );
        assert!(
            markup.contains("Lock again"),
            "missing lock-again link:\n{markup}"
        );
    }

    #[test]
    fn unlocked_from_query_only_honours_locked_fields() {
        let model = sample_model();
        assert_eq!(
            unlocked_from_query(&model, Some("cover_calibrator.unique_id")),
            vec!["cover_calibrator.unique_id".to_string()]
        );
        // A hard-read-only field, an editable field, a typo, or no query → nothing.
        assert_eq!(
            unlocked_from_query(&model, Some("server.port")),
            Vec::<String>::new()
        );
        assert_eq!(
            unlocked_from_query(&model, Some("serial.port")),
            Vec::<String>::new()
        );
        assert_eq!(
            unlocked_from_query(&model, Some("nonsense")),
            Vec::<String>::new()
        );
        assert_eq!(unlocked_from_query(&model, None), Vec::<String>::new());
    }

    #[test]
    fn reconnecting_card_polls_service_status() {
        let markup = reconnecting_card("dsd-fp2").into_string();
        assert!(
            markup.contains(r#"hx-get="/config/dsd-fp2/status""#),
            "{markup}"
        );
        assert!(markup.contains(r#"hx-trigger="every 1s""#), "{markup}");
    }

    #[test]
    fn error_card_explains_action_not_implemented() {
        let err = ConfigClientError::Ascom {
            code: crate::driver_client::ACTION_NOT_IMPLEMENTED,
            message: "nope".to_string(),
        };
        let markup = error_card("dsd-fp2", &err).into_string();
        assert!(
            markup.contains("does not expose configuration actions"),
            "{markup}"
        );
    }

    // --- merge_form ----------------------------------------------------------

    #[test]
    fn merge_form_overlays_editable_fields() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("serial.port", "/dev/ttyACM5"),
            ("cover_calibrator.max_brightness", "2048"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/serial/port")
                .and_then(Value::as_str),
            Some("/dev/ttyACM5")
        );
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/max_brightness")
                .and_then(Value::as_u64),
            Some(2048)
        );
    }

    #[test]
    fn merge_form_does_not_overlay_pinned_fields() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", r#"["serial.port"]"#),
            ("serial.port", "/dev/ttyACM9"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/serial/port")
                .and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        assert_eq!(merged.overrides, vec!["serial.port".to_string()]);
    }

    #[test]
    fn merge_form_never_changes_read_only_enabled() {
        // `enabled` is read-only via the schema tier, so a forged value is
        // ignored and the field round-trips from the blob (stays true).
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("cover_calibrator.enabled", "false"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/enabled")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn merge_form_empty_optional_becomes_null() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("server.discovery_port", ""),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert!(merged
            .config
            .pointer("/server/discovery_port")
            .unwrap()
            .is_null());
    }

    /// Clearing an optional *text* box unsets the field, exactly as
    /// clearing an optional number does. `""` is a different state — rp
    /// reads an absent `file_naming_pattern` as "no templated naming" but
    /// an empty one as malformed — so the form must send the state the
    /// operator actually chose.
    #[test]
    fn merge_form_empty_nullable_string_becomes_null() {
        let model = FieldModel::from_schema(&rp_like_schema());
        let config = json!({
            "session": {
                "data_directory": "/var/lib/rusty-photon",
                "file_naming_pattern": "{target}_{filter}_{binning}_{exposure_duration}_{uuid8}",
            },
        });
        let form = form_from(&[
            ("__config", &config.to_string()),
            ("__overrides", "[]"),
            ("session.file_naming_pattern", ""),
            ("session.data_directory", "/var/lib/rusty-photon"),
        ]);
        let merged = merge_form(&form, &model).unwrap();
        assert!(
            merged
                .config
                .pointer("/session/file_naming_pattern")
                .unwrap()
                .is_null(),
            "a cleared optional string must reach the driver as null, not \"\""
        );
    }

    /// The other half of the rule: a *required* string keeps sending `""`,
    /// so a driver that validates its own required fields still gets the
    /// chance to reject the empty value rather than seeing it as unset.
    #[test]
    fn merge_form_empty_required_string_stays_a_string() {
        let model = FieldModel::from_schema(&rp_like_schema());
        let config = json!({ "session": { "data_directory": "/var/lib/rusty-photon" } });
        let form = form_from(&[
            ("__config", &config.to_string()),
            ("__overrides", "[]"),
            ("session.data_directory", ""),
        ]);
        let merged = merge_form(&form, &model).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/session/data_directory")
                .and_then(Value::as_str),
            Some("")
        );
    }

    #[test]
    fn merge_form_missing_blob_is_an_error() {
        let form = form_from(&[("serial.port", "/dev/ttyACM0")]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::MissingConfig));
    }

    #[test]
    fn merge_form_out_of_range_optional_int_is_a_field_error() {
        // 99999 doesn't fit the schema maximum (65535) for discovery_port.
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("server.discovery_port", "99999"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(merged.errors.len(), 1, "{:?}", merged.errors);
        assert_eq!(merged.errors[0].path, "server.discovery_port");
        // The prior value is kept (not coerced) so re-render shows it.
        assert_eq!(
            merged
                .config
                .pointer("/server/discovery_port")
                .and_then(Value::as_u64),
            Some(32227)
        );
    }

    #[test]
    fn merge_form_empty_required_number_keeps_prior_value() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("serial.baud_rate", ""),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert!(merged.errors.is_empty(), "{:?}", merged.errors);
        assert_eq!(
            merged
                .config
                .pointer("/serial/baud_rate")
                .and_then(Value::as_u64),
            Some(115_200)
        );
    }

    #[test]
    fn merge_form_never_changes_read_only_server_port() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("server.port", "22222"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/server/port")
                .and_then(Value::as_u64),
            Some(11119)
        );
    }

    #[test]
    fn merge_form_ignores_unique_id_when_not_unlocked() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_overlays_unique_id_when_unlocked() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", r#"["cover_calibrator.unique_id"]"#),
            ("cover_calibrator.unique_id", "fixed-by-operator"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("fixed-by-operator")
        );
        assert_eq!(
            merged.unlocked,
            vec!["cover_calibrator.unique_id".to_string()]
        );
    }

    #[test]
    fn merge_form_pinned_unique_id_wins_over_unlock() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", r#"["cover_calibrator.unique_id"]"#),
            ("__unlocked", r#"["cover_calibrator.unique_id"]"#),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
    }

    #[test]
    fn merge_form_forged_unlocked_cannot_unlock_non_locked_field() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", r#"["server.port"]"#),
            ("server.port", "22222"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/server/port")
                .and_then(Value::as_u64),
            Some(11119)
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_malformed_unlocked_is_treated_as_empty() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("__unlocked", "not json"),
            ("cover_calibrator.unique_id", "tampered-id"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/cover_calibrator/unique_id")
                .and_then(Value::as_str),
            Some("dsd-fp2-001")
        );
        assert!(merged.unlocked.is_empty(), "{:?}", merged.unlocked);
    }

    #[test]
    fn merge_form_non_numeric_baud_rate_is_a_field_error() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "[]"),
            ("serial.baud_rate", "fast"),
        ]);
        let merged = merge_form(&form, &sample_model()).unwrap();
        assert_eq!(merged.errors.len(), 1, "{:?}", merged.errors);
        assert_eq!(merged.errors[0].path, "serial.baud_rate");
    }

    #[test]
    fn merge_form_missing_overrides_is_an_error() {
        let form = form_from(&[("__config", &sample_config().to_string())]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::MissingOverrides), "{err:?}");
    }

    #[test]
    fn merge_form_invalid_overrides_is_an_error() {
        let form = form_from(&[
            ("__config", &sample_config().to_string()),
            ("__overrides", "not json"),
        ]);
        let err = merge_form(&form, &sample_model()).unwrap_err();
        assert!(matches!(err, FormError::BadOverrides(_)), "{err:?}");
    }

    #[test]
    fn merge_form_coerces_float_leaf() {
        // A `number` (f64) leaf parses as a float; this isn't present in the
        // dsd-fp2 sample, so exercise it against a minimal one-field model.
        let schema = ConfigSchemaResponse {
            schema: json!({
                "type": "object",
                "properties": {
                    "optics": {
                        "type": "object",
                        "properties": { "focal_length_mm": { "type": "number", "format": "double" } }
                    }
                }
            }),
            locked_fields: vec![],
            read_only_fields: vec![],
        };
        let model = FieldModel::from_schema(&schema);
        let form = form_from(&[
            (
                "__config",
                &json!({ "optics": { "focal_length_mm": 200.0 } }).to_string(),
            ),
            ("__overrides", "[]"),
            ("optics.focal_length_mm", "135.5"),
        ]);
        let merged = merge_form(&form, &model).unwrap();
        assert_eq!(
            merged
                .config
                .pointer("/optics/focal_length_mm")
                .and_then(Value::as_f64),
            Some(135.5)
        );
    }

    #[test]
    fn humanize_makes_readable_labels() {
        assert_eq!(humanize("port"), "Port");
        assert_eq!(humanize("max_brightness"), "Max brightness");
        assert_eq!(
            humanize("dec_limits.max_degrees"),
            "Dec limits · Max degrees"
        );
    }
}

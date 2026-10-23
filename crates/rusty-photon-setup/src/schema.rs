//! The schema walker: a service's config schema (`schemars`) turned into the
//! fields a setup page renders.
//!
//! [`FieldModel::from_schema`] walks the schema into a flat, ordered list of
//! [`FieldSpec`]s: it resolves `$ref` into `$defs`, recurses plain objects in
//! sorted key order, and classifies every leaf into a [`FieldKind`]. A leaf
//! the form cannot edit is never dropped — it becomes a
//! [`FieldKind::FileOnly`] leaf the page names as edited in the configuration
//! file. An internally tagged enum whose variants are objects becomes a
//! [`FieldKind::Variants`] leaf: one radio button per variant, with the
//! chosen variant's fields.

use serde_json::Value;

/// How a field renders and how its submission is coerced back into JSON,
/// inferred from the schema node's `{type, minimum, maximum, …}`.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    /// A `string` leaf — a text input, set verbatim. `nullable` (schema
    /// `type:["string","null"]`) persists `null` when cleared: an optional
    /// field the operator emptied is *unset*, and `""` is a value the service
    /// is entitled to reject.
    Str { nullable: bool },
    /// A `boolean` leaf — a checkbox (present ⇒ `true`, absent ⇒ `false`).
    Bool,
    /// An `integer` leaf. `nullable` persists `null` when cleared; `min`/`max`
    /// (schema bounds) gate the parsed value.
    Int {
        nullable: bool,
        min: Option<i64>,
        max: Option<i64>,
    },
    /// A `number` (floating-point) leaf. `nullable` persists `null` when
    /// cleared; `min`/`max` (schema bounds) gate the parsed value, which must
    /// be finite.
    Num {
        nullable: bool,
        min: Option<f64>,
        max: Option<f64>,
    },
    /// An `array` whose `items` are an integer `enum` — a checkbox group, one
    /// checkbox per enumerated value in schema order. Checked values are
    /// written back in `options` order; nothing checked ⇒ an empty array.
    IntSet { options: Vec<i64> },
    /// A `string` leaf the service names as a secret — an empty password
    /// input whose blank submission means "unchanged"; its stored value never
    /// reaches a page.
    Secret,
    /// An internally tagged enum whose variants are plain objects: one radio
    /// per variant under the tag property, with each variant's own fields.
    Variants { tag: String, variants: Vec<Variant> },
    /// A leaf the form cannot render (an optional object, a composite, an
    /// array of objects, a custom-serde type): named on the page as edited in
    /// the configuration file, and left as the file has it by a save.
    FileOnly,
}

/// One variant of a [`FieldKind::Variants`] enum: its tag value and the
/// fields it carries beside the tag. A field's `name` is the control name
/// (`transport:usb.port`); its `pointer` is relative to the enum's node.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub fields: Vec<FieldSpec>,
}

/// One field discovered in the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    name: String,
    pointer: String,
    section: String,
    label: String,
    kind: FieldKind,
    default: Option<Value>,
}

impl FieldSpec {
    /// The schema's `default` for the field, if it states one: what an input
    /// shows when the config has no value at the field's pointer — the
    /// fields of a tagged enum's unchosen variant, so a switch to it starts
    /// from the defaults rather than from blanks.
    #[must_use]
    pub const fn default_value(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    /// Dotted path (`serial.port`) — the form control's `name`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// RFC-6901 JSON pointer (`/serial/port`) into the config value.
    #[must_use]
    pub fn pointer(&self) -> &str {
        &self.pointer
    }

    /// Top-level section (`serial`) — fields are grouped into a fieldset by
    /// this.
    #[must_use]
    pub fn section(&self) -> &str {
        &self.section
    }

    /// Human-readable label (the humanised sub-path after the section).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// How the field renders and coerces.
    #[must_use]
    pub const fn kind(&self) -> &FieldKind {
        &self.kind
    }

    /// The HTML `type` of the field's input, for the scalar kinds.
    #[must_use]
    pub const fn input_type(&self) -> &'static str {
        match self.kind {
            FieldKind::Bool | FieldKind::IntSet { .. } => "checkbox",
            FieldKind::Int { .. } | FieldKind::Num { .. } => "number",
            FieldKind::Secret => "password",
            FieldKind::Str { .. } | FieldKind::Variants { .. } | FieldKind::FileOnly => "text",
        }
    }

    /// The pointer's segments (`["serial", "port"]`).
    #[must_use]
    pub fn pointer_segments(&self) -> Vec<&str> {
        self.pointer.trim_start_matches('/').split('/').collect()
    }

    /// Whether the field is a scalar the form edits (not file-only, not a
    /// variant selector).
    #[must_use]
    pub const fn is_editable_scalar(&self) -> bool {
        !matches!(self.kind, FieldKind::Variants { .. } | FieldKind::FileOnly)
    }

    /// For a [`FieldKind::Variants`] leaf: the chosen variant's field at
    /// `pointer`, relative to the enum (`/port`), where `value` is the enum's
    /// node in the config (the object carrying the tag). `None` for any other
    /// kind, a tag no variant has, or a pointer no field of that variant has.
    #[must_use]
    pub fn variant_field(&self, value: &Value, pointer: &str) -> Option<&Self> {
        let FieldKind::Variants { tag, variants } = &self.kind else {
            return None;
        };
        let chosen = value.get(tag).and_then(Value::as_str)?;
        variants
            .iter()
            .find(|v| v.name == chosen)?
            .fields
            .iter()
            .find(|f| f.pointer == pointer)
    }
}

/// The ordered fields of a config schema.
#[derive(Debug, Clone, Default)]
pub struct FieldModel {
    fields: Vec<FieldSpec>,
}

impl FieldModel {
    /// Walk a config schema (the `schemars` output for the config type) into
    /// its fields.
    #[must_use]
    pub fn from_schema(schema: &Value) -> Self {
        let mut fields = Vec::new();
        walk_schema(schema, schema, "", &mut fields);
        Self { fields }
    }

    /// Walk `node` (a subschema of `root`, resolved against it) as the form's
    /// root, so field names come out relative to it — an array entry's own
    /// fields, not `equipment.cameras.0.alpaca_url`.
    #[must_use]
    pub fn from_node(root: &Value, node: &Value) -> Self {
        let mut fields = Vec::new();
        walk_schema(node, root, "", &mut fields);
        Self { fields }
    }

    /// Mark every string leaf at one of `pointers` as a [`FieldKind::Secret`],
    /// the fields a tagged enum's variants carry included. A pointer segment
    /// may be the wildcard `*`, matching any one segment.
    #[must_use]
    pub fn with_secrets(mut self, pointers: &[&str]) -> Self {
        for field in &mut self.fields {
            mark_secret(field, "", pointers);
        }
        self
    }

    /// Name every field at one of `pointers` as [`FieldKind::FileOnly`],
    /// whatever the schema made of it: a field the service writes at
    /// runtime (a mount's park position, captured by `SetPark`), which a
    /// form filled before such a write must not carry back. The page names
    /// it as edited in the configuration file. A pointer segment may be the
    /// wildcard `*`; a pointer naming a tagged enum's field reaches into it.
    #[must_use]
    pub fn with_file_only(mut self, pointers: &[&str]) -> Self {
        for field in &mut self.fields {
            mark_file_only(field, "", pointers);
        }
        self
    }

    /// Drop every field whose top-level section is one of `sections`: the
    /// keys a driver's own sections edit, which no generated field may edit
    /// too.
    #[must_use]
    pub fn without_sections(mut self, sections: &[String]) -> Self {
        self.fields.retain(|f| !sections.contains(&f.section));
        self
    }

    /// The ordered fields.
    #[must_use]
    pub fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }

    /// The fields whose top-level section is `section`.
    pub fn in_section<'a>(&'a self, section: &'a str) -> impl Iterator<Item = &'a FieldSpec> {
        self.fields.iter().filter(move |f| f.section == section)
    }

    /// The fields whose top-level section is none of `sections`.
    pub fn outside_sections<'a>(
        &'a self,
        sections: &'a [String],
    ) -> impl Iterator<Item = &'a FieldSpec> {
        self.fields
            .iter()
            .filter(move |f| !sections.contains(&f.section))
    }

    /// The field named `name`, if any.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The top-level sections, in first-seen (sorted) order.
    #[must_use]
    pub fn sections(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for f in &self.fields {
            if !out.contains(&f.section.as_str()) {
                out.push(&f.section);
            }
        }
        out
    }
}

/// Mark `field` a secret when a pattern names it. A variant's field carries
/// a pointer relative to its enum's node, so `base` is the enum's own
/// pointer, and the enum's variants are reached into.
fn mark_secret(field: &mut FieldSpec, base: &str, pointers: &[&str]) {
    let absolute = format!("{base}{}", field.pointer);
    if let FieldKind::Variants { variants, .. } = &mut field.kind {
        for variant in variants {
            for inner in &mut variant.fields {
                mark_secret(inner, &absolute, pointers);
            }
        }
        return;
    }
    if matches!(field.kind, FieldKind::Str { .. })
        && pointers.iter().any(|p| pointer_matches(p, &absolute))
    {
        field.kind = FieldKind::Secret;
    }
}

/// Name `field` file-only when a pattern names it, reaching into a tagged
/// enum's variants as [`mark_secret`] does. A pattern naming the enum
/// itself makes the whole enum file-only.
fn mark_file_only(field: &mut FieldSpec, base: &str, pointers: &[&str]) {
    let absolute = format!("{base}{}", field.pointer);
    if pointers.iter().any(|p| pointer_matches(p, &absolute)) {
        field.kind = FieldKind::FileOnly;
        return;
    }
    if let FieldKind::Variants { variants, .. } = &mut field.kind {
        for variant in variants {
            for inner in &mut variant.fields {
                mark_file_only(inner, &absolute, pointers);
            }
        }
    }
}

/// Whether a secret-pointer pattern (`/a/*/b`) names `pointer`.
fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').skip(1).collect();
    let pointer: Vec<&str> = pointer.split('/').skip(1).collect();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(p, s)| *p == "*" || p == s)
}

/// Resolve a single-level `#/$defs/...` `$ref` against the schema root.
///
/// A node with no `$ref` is returned as-is. (Sibling keys like
/// `default`/`description` on a `$ref` node are irrelevant to leaf
/// classification, so they are dropped.)
#[must_use]
pub fn resolve<'a>(node: &'a Value, root: &'a Value) -> &'a Value {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        if let Some(name) = reference.strip_prefix("#/$defs/") {
            if let Some(target) = root.pointer(&format!("/$defs/{name}")) {
                return target;
            }
        }
    }
    node
}

/// Unwrap an `Option<T>` schema node.
///
/// schemars renders it as an `anyOf`/`oneOf` whose branches are `T` and
/// `{"type":"null"}` — return the resolved non-null branch. A node that is
/// not such a wrapper is returned as-is; `None` when the wrapper has no
/// single non-null branch.
#[must_use]
pub fn unwrap_optional<'a>(node: &'a Value, root: &'a Value) -> Option<&'a Value> {
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

/// Classification of a (resolved) schema node.
enum Shape {
    /// A scalar leaf with its JSON base type and nullability.
    Scalar { base: String, nullable: bool },
    /// An array of enumerated integers — a checkbox-group leaf.
    IntEnumArray { options: Vec<i64> },
    /// A plain object to recurse into.
    Object,
    /// An internally tagged enum of object variants.
    Tagged { tag: String, branches: Vec<Value> },
    /// A composite/enum/custom subtree the form cannot render.
    FileOnly,
}

fn classify(node: &Value, root: &Value) -> Shape {
    if let Some((tag, branches)) = tagged_enum(node, root) {
        return Shape::Tagged { tag, branches };
    }
    // Composite or enum/const subtrees are never rendered as scalar inputs.
    // (An enum on an array's *items* is fine — that check is on the node
    // itself, and the integer-enum-array case below depends on it.)
    if ["oneOf", "anyOf", "allOf", "enum", "const"]
        .iter()
        .any(|k| node.get(k).is_some())
    {
        return Shape::FileOnly;
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
        // An object with properties is recursed into, whether or not its def
        // spells `type` out. One without them — a map such as a driver's
        // switch labels, `additionalProperties` and nothing else — would walk
        // to no field at all, so it falls through to a named file-only leaf.
        Some("object") | None if node.get("properties").is_some() => Shape::Object,
        // The one renderable array shape: items are an integer enum
        // (e.g. rp's `cooler_targets_c` grid) — a checkbox group. All
        // other arrays (objects, `$ref` items, un-enumerated scalars)
        // are file-only.
        Some("array") => int_enum_options(node, root)
            .map_or(Shape::FileOnly, |options| Shape::IntEnumArray { options }),
        _ => Shape::FileOnly,
    }
}

/// An internally tagged enum, as `schemars` renders `#[serde(tag = "…")]`:
/// a `oneOf` (or `anyOf`) whose every branch is an object with a common
/// property fixed to one string (`const`, or an `enum` of one). Returns the
/// tag property's name and the resolved branches.
fn tagged_enum(node: &Value, root: &Value) -> Option<(String, Vec<Value>)> {
    let branches = node
        .get("oneOf")
        .or_else(|| node.get("anyOf"))
        .and_then(Value::as_array)?;
    let branches: Vec<&Value> = branches.iter().map(|b| resolve(b, root)).collect();
    if branches.len() < 2 {
        return None;
    }
    let first = branches.first()?.get("properties")?.as_object()?;
    let tag = first.iter().find_map(|(name, schema)| {
        branches
            .iter()
            .all(|b| {
                b.get("properties")
                    .and_then(|p| p.get(name))
                    .is_some_and(|s| fixed_string(resolve(s, root)).is_some())
            })
            .then(|| name.clone())
            .filter(|_| fixed_string(resolve(schema, root)).is_some())
    })?;
    Some((tag, branches.into_iter().cloned().collect()))
}

/// The one string a schema node is fixed to: `const: "x"` or `enum: ["x"]`.
fn fixed_string(node: &Value) -> Option<String> {
    if let Some(s) = node.get("const").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    match node.get("enum").and_then(Value::as_array)?.as_slice() {
        [Value::String(s)] => Some(s.clone()),
        _ => None,
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
                // feature (a `BTreeMap` map iterates sorted, an `IndexMap` one in
                // insertion order, and a dev-dependency can unify the feature on).
                let mut props: Vec<_> = props.iter().collect();
                props.sort_by_key(|(name, _)| *name);
                for (name, child) in props {
                    let child_prefix = join(prefix, name);
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
                return;
            }
            let mut spec = field_spec(prefix, FieldKind::IntSet { options });
            spec.default = resolved.get("default").cloned();
            out.push(spec);
        }
        Shape::Tagged { tag, branches } => {
            if prefix.is_empty() {
                return;
            }
            let variants = branches
                .iter()
                .filter_map(|branch| variant_of(branch, root, &tag, prefix))
                .collect();
            out.push(field_spec(prefix, FieldKind::Variants { tag, variants }));
        }
        Shape::FileOnly => {
            if prefix.is_empty() {
                return;
            }
            out.push(field_spec(prefix, FieldKind::FileOnly));
        }
    }
}

/// One variant of a tagged enum at `prefix`: its tag value and its fields,
/// named `{prefix}:{variant}.{field}` with pointers relative to the enum.
fn variant_of(branch: &Value, root: &Value, tag: &str, prefix: &str) -> Option<Variant> {
    let props = branch.get("properties")?.as_object()?;
    let name = fixed_string(resolve(props.get(tag)?, root))?;
    let mut fields = Vec::new();
    let mut entries: Vec<_> = props.iter().filter(|(k, _)| *k != tag).collect();
    entries.sort_by_key(|(k, _)| *k);
    for (key, child) in entries {
        let mut inner = Vec::new();
        walk_schema(child, root, key, &mut inner);
        for mut field in inner {
            field.name = format!("{prefix}:{name}.{}", field.name);
            field.section = prefix.split('.').next().unwrap_or(prefix).to_string();
            fields.push(field);
        }
    }
    Some(Variant { name, fields })
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn make_field(name: &str, base: &str, nullable: bool, node: &Value) -> FieldSpec {
    let kind = match base {
        "boolean" => FieldKind::Bool,
        "number" => FieldKind::Num {
            nullable,
            min: node.get("minimum").and_then(Value::as_f64),
            max: node.get("maximum").and_then(Value::as_f64),
        },
        "integer" => FieldKind::Int {
            nullable,
            min: node.get("minimum").and_then(Value::as_i64),
            max: node.get("maximum").and_then(Value::as_i64),
        },
        // "string" and any unexpected base fall back to a text field.
        _ => FieldKind::Str { nullable },
    };
    let mut spec = field_spec(name, kind);
    spec.default = node.get("default").cloned();
    spec
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
        default: None,
    }
}

/// Convert a dotted path (`serial.port`) to an RFC-6901 JSON pointer
/// (`/serial/port`).
#[must_use]
pub fn dotted_to_pointer(dotted: &str) -> String {
    let mut pointer = String::with_capacity(dotted.len().saturating_add(1));
    pointer.push('/');
    pointer.push_str(&dotted.replace('.', "/"));
    pointer
}

/// Turn a dotted, `snake_case` sub-path into a readable label, e.g.
/// `dec_limits.max_degrees` → `Dec limits · Max degrees`.
#[must_use]
pub fn humanize(path: &str) -> String {
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
    use schemars::{schema_for, JsonSchema};
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    /// A representative driver-shaped schema: `$defs` + `$ref` (so the walker's
    /// ref-resolution is exercised), an `anyOf` optional struct (`server.auth`,
    /// file-only), and a nullable optional int (`server.discovery_port`).
    fn sample_schema() -> Value {
        json!({
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
        })
    }

    fn sample_model() -> FieldModel {
        FieldModel::from_schema(&sample_schema())
    }

    fn names(model: &FieldModel) -> Vec<&str> {
        model.fields().iter().map(FieldSpec::name).collect()
    }

    #[test]
    fn walker_produces_scalar_leaves_in_sorted_order() {
        let model = sample_model();
        assert_eq!(
            names(&model),
            vec![
                "cover_calibrator.description",
                "cover_calibrator.enabled",
                "cover_calibrator.max_brightness",
                "cover_calibrator.name",
                "cover_calibrator.unique_id",
                "serial.baud_rate",
                "serial.polling_interval",
                "serial.port",
                "serial.timeout",
                "server.auth",
                "server.discovery_port",
                "server.port",
            ]
        );
    }

    #[test]
    fn an_optional_struct_is_a_named_file_only_leaf() {
        // Never silently absent: the page names it.
        let model = sample_model();
        let auth = model.field("server.auth").unwrap();
        assert_eq!(auth.kind(), &FieldKind::FileOnly);
        assert_eq!(auth.pointer(), "/server/auth");
        assert!(!auth.is_editable_scalar());
    }

    #[test]
    fn a_number_carries_its_schemas_bounds() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "site": { "type": "object", "properties": {
                "latitude": { "type": "number", "minimum": -90, "maximum": 90.0 },
                "elevation": { "type": ["number", "null"] },
            } } },
        });
        let model = FieldModel::from_schema(&schema);
        assert_eq!(
            model.field("site.latitude").unwrap().kind(),
            &FieldKind::Num {
                nullable: false,
                min: Some(-90.0),
                max: Some(90.0)
            }
        );
        assert_eq!(
            model.field("site.elevation").unwrap().kind(),
            &FieldKind::Num {
                nullable: true,
                min: None,
                max: None
            }
        );
    }

    #[test]
    fn walker_infers_field_kinds() {
        let model = sample_model();
        let kind = |name: &str| model.field(name).unwrap().kind().clone();
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
        assert_eq!(model.field("serial.port").unwrap().input_type(), "text");
        assert_eq!(model.field("server.port").unwrap().input_type(), "number");
        assert_eq!(
            model
                .field("cover_calibrator.enabled")
                .unwrap()
                .input_type(),
            "checkbox"
        );
    }

    #[test]
    fn with_secrets_marks_string_leaves_by_pointer_and_wildcard() {
        let schema = json!({
            "type": "object",
            "properties": {
                "follow": {
                    "type": "object",
                    "properties": {
                        "password": { "type": "string" },
                        "url": { "type": "string" },
                    },
                },
                "sites": {
                    "type": "object",
                    "properties": {
                        "a": { "type": "object", "properties": { "key": { "type": "string" } } },
                        "b": { "type": "object", "properties": { "key": { "type": "string" } } },
                    },
                },
            },
        });
        let model =
            FieldModel::from_schema(&schema).with_secrets(&["/follow/password", "/sites/*/key"]);
        assert_eq!(
            model.field("follow.password").unwrap().kind(),
            &FieldKind::Secret
        );
        assert_eq!(
            model.field("follow.password").unwrap().input_type(),
            "password"
        );
        assert_eq!(
            model.field("follow.url").unwrap().kind(),
            &FieldKind::Str { nullable: false }
        );
        assert_eq!(
            model.field("sites.a.key").unwrap().kind(),
            &FieldKind::Secret
        );
        assert_eq!(
            model.field("sites.b.key").unwrap().kind(),
            &FieldKind::Secret
        );
    }

    #[test]
    fn walker_classifies_an_integer_enum_array_as_a_checkbox_group() {
        let schema = json!({
            "type": "object",
            "properties": {
                "cooling": {
                    "type": "object",
                    "properties": {
                        "targets": {
                            "type": "array",
                            "items": { "type": "integer", "enum": [-15, -10, -5, 0, 5] },
                            "uniqueItems": true,
                        },
                    },
                },
            },
        });
        let model = FieldModel::from_schema(&schema);
        assert_eq!(
            model.field("cooling.targets").unwrap().kind(),
            &FieldKind::IntSet {
                options: vec![-15, -10, -5, 0, 5]
            },
            "options must carry the schema enum in its declared order"
        );
    }

    #[test]
    fn a_map_is_a_named_file_only_leaf() {
        // `additionalProperties` and no `properties`: a `BTreeMap`-style
        // field (the Pegasus drivers' switch labels). Never silently
        // absent from the page.
        let schema = json!({
            "type": "object",
            "properties": {
                "switch": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "labels": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                        },
                    },
                },
            },
        });
        let model = FieldModel::from_schema(&schema);
        assert_eq!(names(&model), vec!["switch.labels", "switch.name"]);
        let labels = model.field("switch.labels").unwrap();
        assert_eq!(labels.kind(), &FieldKind::FileOnly);
        assert_eq!(labels.pointer(), "/switch/labels");
    }

    #[test]
    fn an_array_of_objects_is_a_named_file_only_leaf() {
        let schema = json!({
            "$defs": { "Dev": { "type": "object", "properties": { "port": { "type": "string" } } } },
            "type": "object",
            "properties": {
                "usb_devices": { "type": "array", "items": { "$ref": "#/$defs/Dev" } },
            },
        });
        let model = FieldModel::from_schema(&schema);
        assert_eq!(names(&model), vec!["usb_devices"]);
        assert_eq!(
            model.field("usb_devices").unwrap().kind(),
            &FieldKind::FileOnly
        );
    }

    #[test]
    fn from_node_walks_an_entry_with_relative_names() {
        let root = json!({
            "$defs": {
                "CameraConfig": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "alpaca_url": { "type": "string" },
                        "device_number": { "type": "integer", "format": "uint32", "minimum": 0 },
                        "auth": { "anyOf": [ { "$ref": "#/$defs/DeviceAuth" }, { "type": "null" } ] },
                    },
                },
                "DeviceAuth": {
                    "type": "object",
                    "properties": { "username": { "type": "string" }, "password": { "type": "string" } },
                },
            },
            "type": "object",
            "properties": {
                "cameras": { "type": "array", "items": { "$ref": "#/$defs/CameraConfig" } },
            },
        });
        let items = root.pointer("/properties/cameras/items").unwrap();
        let model = FieldModel::from_node(&root, items);
        assert_eq!(
            names(&model),
            vec!["alpaca_url", "auth", "device_number", "id"]
        );
        assert_eq!(model.field("auth").unwrap().kind(), &FieldKind::FileOnly);
        assert_eq!(model.field("id").unwrap().pointer_segments(), vec!["id"]);
    }

    #[test]
    fn unwrap_optional_returns_the_non_null_branch() {
        let root = json!({ "$defs": { "T": { "type": "object", "properties": {} } } });
        let node = json!({ "anyOf": [ { "$ref": "#/$defs/T" }, { "type": "null" } ] });
        let unwrapped = unwrap_optional(&node, &root).unwrap();
        assert_eq!(
            unwrapped.get("type").and_then(Value::as_str),
            Some("object")
        );
        let plain = json!({ "type": "string" });
        assert_eq!(unwrap_optional(&plain, &root), Some(&plain));
        let two = json!({ "anyOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(unwrap_optional(&two, &root).is_none());
    }

    /// The `GTi` mount's transport shape: an internally tagged enum of object
    /// variants, as `schemars` really renders `#[serde(tag = "kind")]`.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "kind", rename_all = "lowercase")]
    #[allow(dead_code)]
    enum Transport {
        Usb { port: String, baud_rate: u32 },
        Udp { host: String, port: u16 },
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct WithTransport {
        name: String,
        transport: Transport,
    }

    #[test]
    fn a_tagged_enum_of_objects_is_a_variants_leaf() {
        let schema = serde_json::to_value(schema_for!(WithTransport)).unwrap();
        let model = FieldModel::from_schema(&schema);
        assert_eq!(names(&model), vec!["name", "transport"]);
        let FieldKind::Variants { tag, variants } = model.field("transport").unwrap().kind() else {
            panic!(
                "transport must be a Variants leaf: {:?}",
                model.field("transport")
            );
        };
        assert_eq!(tag, "kind");
        let variant_names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(variant_names, vec!["usb", "udp"]);
        let usb = &variants[0];
        let usb_fields: Vec<(&str, &str)> =
            usb.fields.iter().map(|f| (f.name(), f.pointer())).collect();
        assert_eq!(
            usb_fields,
            vec![
                ("transport:usb.baud_rate", "/baud_rate"),
                ("transport:usb.port", "/port"),
            ]
        );
        assert_eq!(usb.fields[0].section(), "transport");
        assert_eq!(
            usb.fields[0].default_value(),
            None,
            "a field without a schema default has none"
        );
        let udp = &variants[1];
        assert_eq!(
            udp.fields[1].kind(),
            &FieldKind::Int {
                nullable: false,
                min: Some(0),
                max: Some(65535)
            }
        );
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "kind", rename_all = "lowercase")]
    #[allow(dead_code)]
    enum Link {
        Serial { port: String },
        Network { host: String, password: String },
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct WithLink {
        link: Link,
        follow: Follow,
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Follow {
        password: String,
    }

    #[test]
    fn with_secrets_reaches_the_fields_a_variant_carries() {
        // A secret pointer names the field by its config path; the field
        // lives under the enum's node, where the walker's relative pointer
        // alone would never match it.
        let schema = serde_json::to_value(schema_for!(WithLink)).unwrap();
        let model =
            FieldModel::from_schema(&schema).with_secrets(&["/link/password", "/follow/password"]);
        let FieldKind::Variants { variants, .. } = model.field("link").unwrap().kind() else {
            panic!("link must be a Variants leaf");
        };
        let network = variants.iter().find(|v| v.name == "network").unwrap();
        let kind_of = |name: &str| {
            network
                .fields
                .iter()
                .find(|f| f.name() == name)
                .unwrap_or_else(|| panic!("no field {name}"))
                .kind()
                .clone()
        };
        assert_eq!(kind_of("link:network.password"), FieldKind::Secret);
        assert_eq!(
            kind_of("link:network.host"),
            FieldKind::Str { nullable: false }
        );
        let serial = variants.iter().find(|v| v.name == "serial").unwrap();
        assert_eq!(
            serial.fields[0].kind(),
            &FieldKind::Str { nullable: false },
            "the other variant's string is not the secret"
        );
        assert_eq!(
            model.field("follow.password").unwrap().kind(),
            &FieldKind::Secret,
            "a plain field is still marked"
        );
    }

    #[test]
    fn with_file_only_names_a_renderable_field_as_file_only_inside_a_variant_too() {
        // A field the service writes at runtime is kept off the form
        // whatever the schema made of it — a plain integer, or a string
        // inside a tagged enum's variant.
        let schema = json!({
            "type": "object",
            "properties": {
                "mount": { "type": "object", "properties": {
                    "name": { "type": "string" },
                    "park_ra_ticks": { "type": ["integer", "null"] },
                } },
                "link": { "oneOf": [
                    { "type": "object", "properties": { "kind": { "const": "serial" }, "port": { "type": "string" } } },
                    { "type": "object", "properties": { "kind": { "const": "network" }, "host": { "type": "string" }, "lease": { "type": "string" } } },
                ] },
            },
        });
        let model = FieldModel::from_schema(&schema)
            .with_file_only(&["/mount/park_ra_ticks", "/link/lease"]);
        assert_eq!(
            model.field("mount.park_ra_ticks").unwrap().kind(),
            &FieldKind::FileOnly
        );
        assert_eq!(
            model.field("mount.name").unwrap().kind(),
            &FieldKind::Str { nullable: false },
            "a field no pointer names is untouched"
        );
        let FieldKind::Variants { variants, .. } = model.field("link").unwrap().kind() else {
            panic!("link must be a Variants leaf");
        };
        let network = variants.iter().find(|v| v.name == "network").unwrap();
        let kind_of = |name: &str| {
            network
                .fields
                .iter()
                .find(|f| f.name() == name)
                .unwrap()
                .kind()
                .clone()
        };
        assert_eq!(kind_of("link:network.lease"), FieldKind::FileOnly);
        assert_eq!(
            kind_of("link:network.host"),
            FieldKind::Str { nullable: false }
        );
        // Naming the enum itself keeps the whole selector off the form.
        let whole = FieldModel::from_schema(&schema).with_file_only(&["/link"]);
        assert_eq!(whole.field("link").unwrap().kind(), &FieldKind::FileOnly);
    }

    #[test]
    fn variant_field_finds_the_chosen_variants_field_by_its_relative_pointer() {
        let schema = serde_json::to_value(schema_for!(WithLink)).unwrap();
        let model = FieldModel::from_schema(&schema);
        let link = model.field("link").unwrap();
        let network = json!({ "kind": "network", "host": "h" });
        assert_eq!(
            link.variant_field(&network, "/host").map(FieldSpec::name),
            Some("link:network.host")
        );
        assert_eq!(
            link.variant_field(&network, "/port").map(FieldSpec::name),
            None,
            "the other variant's field is not the chosen one's"
        );
        let serial = json!({ "kind": "serial", "port": "/dev/a" });
        assert_eq!(
            link.variant_field(&serial, "/port").map(FieldSpec::name),
            Some("link:serial.port")
        );
        assert!(link
            .variant_field(&json!({ "kind": "bluetooth" }), "/port")
            .is_none());
        assert!(link.variant_field(&json!(null), "/port").is_none());
        assert!(
            model
                .field("follow.password")
                .unwrap()
                .variant_field(&serial, "/port")
                .is_none(),
            "only a Variants leaf has variant fields"
        );
    }

    #[test]
    fn a_fields_schema_default_is_kept() {
        // `serde(default = …)` on a variant's field lands in the schema as
        // `default`; the unchosen variant's inputs start from it.
        let schema = json!({
            "type": "object",
            "properties": {
                "transport": { "oneOf": [
                    { "type": "object", "properties": {
                        "kind": { "const": "usb" },
                        "baud_rate": { "type": "integer", "default": 115_200 },
                        "port": { "type": "string" },
                    } },
                    { "type": "object", "properties": {
                        "kind": { "const": "udp" },
                        "timeout": { "type": "string", "default": "2s" },
                    } },
                ] },
                "cooling": { "type": "object", "properties": {
                    "targets": { "type": "array", "items": { "type": "integer", "enum": [-10, 0] }, "default": [0] },
                } },
            },
        });
        let model = FieldModel::from_schema(&schema);
        let FieldKind::Variants { variants, .. } = model.field("transport").unwrap().kind() else {
            panic!("transport must be a Variants leaf");
        };
        let default_of = |variant: &str, name: &str| {
            variants
                .iter()
                .find(|v| v.name == variant)
                .unwrap()
                .fields
                .iter()
                .find(|f| f.name() == name)
                .unwrap()
                .default_value()
                .cloned()
        };
        assert_eq!(
            default_of("usb", "transport:usb.baud_rate"),
            Some(json!(115_200))
        );
        assert_eq!(default_of("usb", "transport:usb.port"), None);
        assert_eq!(
            default_of("udp", "transport:udp.timeout"),
            Some(json!("2s"))
        );
        assert_eq!(
            model.field("cooling.targets").unwrap().default_value(),
            Some(&json!([0]))
        );
    }

    #[test]
    fn sections_and_filters_group_the_fields() {
        let model = sample_model();
        assert_eq!(
            model.sections(),
            vec!["cover_calibrator", "serial", "server"]
        );
        assert_eq!(model.in_section("cover_calibrator").count(), 5);
        let device_sections = ["cover_calibrator".to_string()];
        let server: Vec<&str> = model
            .outside_sections(&device_sections)
            .map(FieldSpec::name)
            .collect();
        assert_eq!(server.len(), 7);
        assert!(server.iter().all(|n| !n.starts_with("cover_calibrator")));
    }

    #[test]
    fn humanize_makes_readable_labels() {
        assert_eq!(humanize("port"), "Port");
        assert_eq!(humanize("max_brightness"), "Max brightness");
        assert_eq!(
            humanize("dec_limits.max_degrees"),
            "Dec limits · Max degrees"
        );
        assert_eq!(dotted_to_pointer("serial.port"), "/serial/port");
    }
}

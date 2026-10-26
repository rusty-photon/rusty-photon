//! The pages' markup: the shell, a field per [`FieldKind`], the device list,
//! the banners, and the page a save answers with.

use maud::{html, Markup, PreEscaped, DOCTYPE};
use rusty_photon_config::driver::FieldError;
use serde_json::Value;

use crate::form::{array_has, shown_value, text_of};
use crate::schema::{FieldKind, FieldSpec};

/// The stylesheet every page inlines.
const STYLESHEET: &str = include_str!("../assets/setup.css");

/// How a field renders beyond its value.
#[derive(Debug, Clone, Copy, Default)]
pub struct FieldState<'a> {
    /// The controls a command-line override pins: disabled, with a note. A
    /// tagged enum's own name pins its selector and every field inside it;
    /// a field inside one is pinned by its own control name.
    pub pinned: &'a [String],
    /// The errors a rejected save attached, by field name; a field shows
    /// its own, a variant's inner field included.
    pub errors: &'a [FieldError],
}

impl FieldState<'_> {
    fn is_pinned(&self, name: &str) -> bool {
        self.pinned.iter().any(|p| p == name)
    }

    fn error_for(&self, name: &str) -> Option<&str> {
        self.errors
            .iter()
            .find(|e| e.path == name)
            .map(|e| e.msg.as_str())
    }
}

/// The whole document: the shell around `body`, titled `title`, for the
/// service named `service`.
#[must_use]
pub fn document(service: &str, title: &str, generation: &str, body: &Markup) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // The handler's generation: a reload builds a new handler, so a
                // page from the rebuilt server carries a different value — the
                // signal the suites wait for after a save.
                meta name="setup-generation" content=(generation);
                title { (title) " · " (service) }
                style { (PreEscaped(STYLESHEET)) }
            }
            body {
                nav.topnav {
                    div.logo {}
                    span.title { "rusty-photon" }
                    span.service { (service) }
                }
                main.container { (body) }
            }
        }
    }
    .into_string()
}

/// A banner above the form.
#[must_use]
pub fn banner(kind: &str, id: &str, text: &str) -> Markup {
    html! {
        div class=(format!("banner {kind}")) id=(id) { span.dot {} span { (text) } }
    }
}

/// The errors a rejected save puts above the form.
///
/// A general one (no path), and any whose path is not a control on the
/// page — the file's other section failing validation, say — named with
/// its path, so an error is never silently dropped. `placed` names the
/// page's controls.
#[must_use]
pub fn rejected_banner(errors: &[FieldError], placed: &[&str]) -> Markup {
    let unplaced: Vec<&FieldError> = errors
        .iter()
        .filter(|e| !placed.contains(&e.path.as_str()))
        .collect();
    html! {
        @if unplaced.is_empty() {
            (banner("error", "rejected", "Some values were rejected. Fix the marked fields and save again."))
        } @else {
            @for e in unplaced {
                @if e.path.is_empty() {
                    (banner("error", "rejected", &e.msg))
                } @else {
                    div class="banner error" id="rejected" {
                        span.dot {}
                        span { "Rejected: " code { (e.path) } " " (e.msg) " — a field this page does not show." }
                    }
                }
            }
        }
    }
}

/// One field, filled from `config`, in `state`.
#[must_use]
pub fn field(spec: &FieldSpec, config: &Value, state: FieldState<'_>) -> Markup {
    match spec.kind() {
        FieldKind::FileOnly => file_only(spec, spec.pointer(), config, state),
        FieldKind::Variants { tag, variants } => variants_field(spec, tag, variants, config, state),
        _ => scalar_field(
            spec,
            spec.pointer(),
            config,
            state,
            state.is_pinned(spec.name()),
        ),
    }
}

/// A scalar field reading its value at `pointer` (a variant's field reads
/// under its enum's object), `pinned` by a command-line override or not.
fn scalar_field(
    spec: &FieldSpec,
    pointer: &str,
    config: &Value,
    state: FieldState<'_>,
    pinned: bool,
) -> Markup {
    let name = spec.name();
    let disabled = pinned;
    let error = state.error_for(name);
    // The config's value, else the schema's default: an unchosen variant's
    // field has no value yet, and a switch to it starts from the defaults.
    let value = shown_value(spec, config, pointer);
    html! {
        div.field.pinned[disabled].invalid[error.is_some()] {
            @match spec.kind() {
                FieldKind::Bool => {
                    div.checkbox {
                        input type="checkbox" id=(name) name=(name)
                            checked[value.as_bool() == Some(true)] disabled[disabled];
                        label for=(name) { (spec.label()) }
                    }
                }
                // One checkbox per enumerated value, in schema order; all
                // share the field's `name`, so the checked values arrive as
                // repeated form pairs.
                FieldKind::IntSet { options } => {
                    span.group-label { (spec.label()) }
                    div.checkbox-group role="group" aria-label=(spec.label()) {
                        @for option in options {
                            @let option_id = format!("{name}.{option}");
                            div.checkbox {
                                input type="checkbox" id=(option_id) name=(name) value=(option)
                                    checked[array_has(value, *option)]
                                    disabled[disabled];
                                label for=(option_id) { (option) }
                            }
                        }
                    }
                }
                // One option per value, in schema order; the shown value is
                // the one selected.
                FieldKind::Choice { options } => {
                    label for=(name) { (spec.label()) }
                    select id=(name) name=(name) disabled[disabled] {
                        @for option in options {
                            option value=(option.value)
                                selected[value.as_str() == Some(option.value.as_str())] {
                                (option.label)
                            }
                        }
                    }
                }
                FieldKind::Secret => {
                    label for=(name) { (spec.label()) }
                    input type="password" id=(name) name=(name) value="" disabled[disabled]
                        autocomplete="off";
                    div.hint { "Blank keeps the stored value." }
                }
                // A number input carries its `step` and its schema bounds
                // (`FieldSpec::step`, `FieldSpec::bounds`); a text input
                // carries neither.
                _ => {
                    @let (min, max) = spec.bounds();
                    label for=(name) { (spec.label()) }
                    input type=(spec.input_type()) step=[spec.step()] min=[min] max=[max]
                        id=(name) name=(name) value=(text_of(value)) disabled[disabled];
                }
            }
            @if let Some(hint) = spec.hint() { div.hint { (hint) } }
            @if let Some(e) = error { div.error { (e) } }
            @if pinned {
                div.hint { "Pinned by a command-line override; change the service's launch flags to edit it." }
            }
        }
    }
}

/// A leaf the form does not edit, named as file-only with whether it is
/// set at `pointer` (a variant's field reads under its enum's object), and
/// with its error when a rejected save attached one: a rule on such a
/// field is still the operator's to read.
fn file_only(spec: &FieldSpec, pointer: &str, config: &Value, state: FieldState<'_>) -> Markup {
    let set = config.pointer(pointer).is_some_and(|v| !v.is_null());
    let error = state.error_for(spec.name());
    html! {
        div.file-only.invalid[error.is_some()] id=(spec.name()) {
            code { (spec.name()) }
            " is edited in the configuration file; it is "
            @if set { "set." } @else { "not set." }
            @if let Some(hint) = spec.hint() { div.hint { (hint) } }
            @if let Some(e) = error { div.error { (e) } }
        }
    }
}

/// A tagged enum: one radio per variant, the chosen variant's fields shown,
/// each variant's fields in a fieldset [`VARIANT_SWITCH_SCRIPT`] switches
/// off while it is not chosen. A pin on the enum disables the selector and
/// every field; a pin on one field disables that field alone.
fn variants_field(
    spec: &FieldSpec,
    tag: &str,
    variants: &[crate::schema::Variant],
    config: &Value,
    state: FieldState<'_>,
) -> Markup {
    let name = spec.name();
    let enum_pointer = spec.pointer();
    let current = config
        .pointer(enum_pointer)
        .and_then(|v| v.get(tag))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let chosen = |variant: &str| variant == current;
    let pinned = state.is_pinned(name);
    let error = state.error_for(name);
    html! {
        div.field.variants.pinned[pinned].invalid[error.is_some()] role="group" aria-label=(spec.label()) {
            span.group-label { (spec.label()) }
            @for variant in variants {
                @let id = format!("{name}.{}", variant.name);
                div.variant {
                    div.choice.checkbox {
                        input type="radio" id=(id) name=(name) value=(variant.name)
                            checked[chosen(&variant.name)] disabled[pinned];
                        label for=(id) { (variant.name) }
                    }
                    fieldset.variant-fields {
                        @for inner in &variant.fields {
                            @let pointer = format!("{enum_pointer}{}", inner.pointer());
                            @let shown = if chosen(&variant.name) { config } else { &Value::Null };
                            @if matches!(inner.kind(), FieldKind::FileOnly) {
                                (file_only(inner, &pointer, shown, state))
                            } @else {
                                (scalar_field(inner, &pointer, shown, state, pinned || state.is_pinned(inner.name())))
                            }
                        }
                    }
                }
            }
            @if let Some(hint) = spec.hint() { div.hint { (hint) } }
            @if let Some(e) = error { div.error { (e) } }
            @if pinned {
                div.hint { "Pinned by a command-line override; change the service's launch flags to edit it." }
            }
            script { (PreEscaped(VARIANT_SWITCH_SCRIPT)) }
        }
    }
}

/// A tagged enum's switch, run in the group it closes: every fieldset but
/// the chosen variant's is disabled, so the browser neither checks nor
/// sends a hidden variant's fields, and a value typed there before the
/// switch cannot hold up the save. It syncs on each change in the group and
/// on `pageshow`, by when the browser has put back any choice it restores;
/// until then every fieldset is on, as it stays without script, and the
/// save reads the chosen variant's fields alone. A field's own `disabled`
/// (a pin) is left as it is.
const VARIANT_SWITCH_SCRIPT: &str = r#"(function (group) {
  function sync() {
    group.querySelectorAll(":scope > .variant").forEach(function (variant) {
      var choice = variant.querySelector(":scope > .choice input");
      variant.querySelector(":scope > .variant-fields").disabled = !choice.checked;
    });
  }
  group.addEventListener("change", sync);
  window.addEventListener("pageshow", sync);
})(document.currentScript.parentElement);"#;

/// A device the server page lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub name: String,
    pub device_type: String,
    pub device_number: u32,
    pub enabled: bool,
    /// Whether a command-line override pins the section's `enabled`, so the
    /// file is not where a disabled device is enabled again.
    pub pinned: bool,
    /// The device page, relative to the server page.
    pub href: String,
}

/// The server page's device list.
#[must_use]
pub fn device_list(devices: &[DeviceRow]) -> Markup {
    html! {
        fieldset {
            legend { "Devices" }
            @if devices.is_empty() {
                p.hint { "This server serves no device." }
            } @else {
                ul.devices {
                    @for d in devices {
                        li id=(format!("device-{}-{}", d.device_type, d.device_number)) {
                            @if d.enabled {
                                a href=(d.href) { (d.name) }
                            } @else {
                                span { (d.name) }
                            }
                            span.kind { (d.device_type) " " (d.device_number) }
                            @if !d.enabled {
                                @if d.pinned {
                                    span.note { "Disabled by a command-line override: its page is not served; change the service's launch flags to serve it." }
                                } @else {
                                    span.note { "Disabled: its page is not served; enable it in the configuration file." }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What a save answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// Nothing changed: no reload.
    Unchanged,
    /// The service is reloading; the page waits for it.
    Reloading,
    /// The changed fields take effect on the next start.
    RestartRequired(Vec<String>),
}

/// Where the browser goes after a save, when not back to the page it saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The URL: absolute when the server's address changed, relative when
    /// only the page changed.
    pub url: String,
    /// Why, when it is not that the pages moved: a disabled device's page is
    /// no longer served.
    pub note: Option<String>,
}

/// How to restart `service` where it is packaged: its systemd unit, its
/// Windows service and its Homebrew service all carry the one name.
#[must_use]
pub fn restart_how(service: &str) -> Markup {
    let name = format!("rusty-photon-{service}");
    html! {
        p.restart-how {
            "Restart the service to apply them: "
            code { "sudo systemctl restart " (name) } " on Linux, "
            code { "Restart-Service " (name) } " on Windows, or "
            code { "brew services restart " (name) } " on macOS."
        }
    }
}

/// A bound credential a save removed, and the settings whose change removed
/// it, as dotted paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unbound {
    /// The credential block: `rp.auth`.
    pub credential: String,
    /// The settings that changed: `rp.base_url`.
    pub settings: Vec<String>,
}

/// `items` in monospace, joined by "and".
#[must_use]
pub fn mono_list(items: &[String]) -> Markup {
    html! {
        @for (i, item) in items.iter().enumerate() {
            @if i > 0 { " and " }
            span.mono { (item) }
        }
    }
}

/// The page `service`'s save answers with.
///
/// What happened, the credentials the save removed because the server they
/// are presented to changed (`unbound`), where the browser goes if not back
/// to this page (`destination`), and the wait. A page that names a removal
/// waits for no reload: it stays on screen until it is left, so the warning
/// is read.
#[must_use]
pub fn saved_body(
    service: &str,
    saved: &Saved,
    destination: Option<&Destination>,
    back: &str,
    unbound: &[Unbound],
) -> Markup {
    let moved_to = destination.map(|d| d.url.as_str());
    let target = moved_to.map(str::to_string);
    let target_json = serde_json::to_string(&target).unwrap_or_else(|_| "null".to_string());
    html! {
        div.card {
            @match saved {
                Saved::Unchanged => {
                    (banner("ok", "saved-unchanged", "Saved. Nothing changed, so no reload is needed."))
                }
                Saved::Reloading if unbound.is_empty() => {
                    (banner("applying", "saved-reloading", "Saved. The service is reloading; this page reopens when it answers again."))
                }
                Saved::Reloading => {
                    (banner("applying", "saved-reloading", "Saved. The service is reloading; this page stays so the removal below is read."))
                }
                Saved::RestartRequired(fields) => {
                    div class="banner warn" id="saved-restart" {
                        span.dot {}
                        span {
                            "Saved. These fields take effect when the service is restarted: "
                            span.mono { (fields.join(", ")) }
                        }
                    }
                    (restart_how(service))
                }
            }
            @if !unbound.is_empty() {
                div class="banner warn" id="saved-unbound" {
                    span.dot {}
                    span {
                        "Removed from the file: "
                        @for (i, removed) in unbound.iter().enumerate() {
                            @if i > 0 { "; " }
                            span.mono { (removed.credential) }
                            ", since "
                            (mono_list(&removed.settings))
                            " changed"
                        }
                        ". A stored credential is presented only to the server it was set for: set it again in the configuration file."
                    }
                }
            }
            @if let Some(d) = destination {
                p id="moved" {
                    @if let Some(note) = &d.note {
                        (note) " " a href=(d.url) { "Open the server page" } "."
                    } @else {
                        "The setup pages now live at "
                        a href=(d.url) { (d.url) }
                        "."
                    }
                }
            }
            p {
                @match (saved, moved_to) {
                    (Saved::Reloading, Some(url)) if unbound.is_empty() => { "If it does not reopen, " a href=(url) { "open the new address" } " once the service is back." }
                    (Saved::Reloading, None) if unbound.is_empty() => { "If it does not reopen, " a href=(back) { "reload the page" } " once the service is back." }
                    (Saved::Reloading, Some(url)) => { a href=(url) { "Open the new address" } " once the service is back." }
                    (Saved::Reloading, None) => { a href=(back) { "Back to the page" } " once the service is back." }
                    (Saved::RestartRequired(_), Some(_)) => { "Nothing answers at the new address until the restart. " a href=(back) { "Back to the page" } "." }
                    _ => { a href=(back) { "Back to the page" } }
                }
            }
            @if matches!(saved, Saved::Reloading) && unbound.is_empty() {
                script { (PreEscaped(reload_wait_script(&target_json))) }
            }
        }
    }
}

/// The reload wait: poll the page's URL until any HTTP answer arrives — a
/// `401` after a credential change included, which means the listener is
/// back — then navigate to it. The probe is `no-cors`: after a port change
/// the URL is another origin, and the server sends no CORS headers, so a
/// plain fetch would reject every answer and never navigate.
fn reload_wait_script(target_json: &str) -> String {
    format!(
        "(function () {{\n  var url = {target_json} || window.location.href;\n  function probe() {{\n    fetch(url, {{ mode: \"no-cors\", cache: \"no-store\", credentials: \"same-origin\" }})\n      .then(function () {{ window.location.assign(url); }})\n      .catch(function () {{ window.setTimeout(probe, 500); }});\n  }}\n  window.setTimeout(probe, 1000);\n}})();"
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::schema::FieldModel;
    use serde_json::json;

    fn model() -> FieldModel {
        let schema = json!({
            "type": "object",
            "properties": {
                "serial": { "type": "object", "properties": {
                    "port": { "type": "string" },
                    "baud_rate": { "type": "integer", "minimum": 0 },
                    "gain": { "type": "number" },
                    "tilt": { "type": "number", "minimum": -90, "maximum": 90 },
                    "enabled": { "type": "boolean" },
                    "key": { "type": "string" },
                    "targets": { "type": "array", "items": { "type": "integer", "enum": [-10, 0] } },
                    "mode": { "oneOf": [
                        { "type": "string", "const": "auto", "title": "Every camera the scan places" },
                        { "type": "string", "const": "list" },
                    ] },
                } },
                "server": { "type": "object", "properties": {
                    "tls": { "anyOf": [ { "type": "object", "properties": { "cert": { "type": "string" } } }, { "type": "null" } ] },
                } },
                "transport": { "oneOf": [
                    { "type": "object", "properties": { "kind": { "const": "usb" }, "port": { "type": "string" }, "baud_rate": { "type": "integer", "default": 115_200 } } },
                    { "type": "object", "properties": { "kind": { "const": "udp" }, "host": { "type": "string" } } },
                ] },
            },
        });
        FieldModel::from_schema(&schema).with_secrets(&["/serial/key"])
    }

    fn config() -> Value {
        json!({
            "serial": { "port": "/dev/a", "baud_rate": 9600, "gain": 1.5, "tilt": 12.5, "enabled": true, "key": "stored", "targets": [0], "mode": "list" },
            "server": { "tls": null },
            "transport": { "kind": "udp", "host": "mount.local" },
        })
    }

    fn render(name: &str, state: FieldState<'_>) -> String {
        field(model().field(name).unwrap(), &config(), state).into_string()
    }

    fn errors(pairs: &[(&str, &str)]) -> Vec<FieldError> {
        pairs
            .iter()
            .map(|(path, msg)| FieldError {
                path: (*path).to_string(),
                msg: (*msg).to_string(),
            })
            .collect()
    }

    #[test]
    fn a_text_field_carries_its_value_and_a_pinned_one_is_disabled() {
        let markup = render("serial.port", FieldState::default());
        assert!(
            markup.contains(
                r#"<input type="text" id="serial.port" name="serial.port" value="/dev/a">"#
            ),
            "{markup}"
        );
        assert!(!markup.contains("disabled"), "{markup}");
        let pins = ["serial.port".to_string()];
        let pinned = render(
            "serial.port",
            FieldState {
                pinned: &pins,
                errors: &[],
            },
        );
        assert!(pinned.contains("disabled"), "{pinned}");
        assert!(
            pinned.contains("Pinned by a command-line override"),
            "{pinned}"
        );
        assert!(pinned.contains(r#"class="field pinned""#), "{pinned}");
    }

    #[test]
    fn a_rejected_field_shows_its_error() {
        let errs = errors(&[("serial.baud_rate", "must be greater than 0")]);
        let markup = render(
            "serial.baud_rate",
            FieldState {
                pinned: &[],
                errors: &errs,
            },
        );
        assert!(markup.contains(r#"class="field invalid""#), "{markup}");
        assert!(
            markup.contains(r#"<div class="error">must be greater than 0</div>"#),
            "{markup}"
        );
        assert!(markup.contains(r#"type="number""#), "{markup}");
    }

    #[test]
    fn a_number_field_takes_any_value_and_an_integer_steps_by_one() {
        let markup = render("serial.gain", FieldState::default());
        assert!(
            markup.contains(
                r#"<input type="number" step="any" id="serial.gain" name="serial.gain" value="1.5">"#
            ),
            "{markup}"
        );
        let integer = render("serial.baud_rate", FieldState::default());
        assert!(
            integer.contains(
                r#"<input type="number" min="0" id="serial.baud_rate" name="serial.baud_rate" value="9600">"#
            ),
            "{integer}"
        );
    }

    #[test]
    fn a_number_fields_schema_bounds_are_its_min_and_max() {
        let markup = render("serial.tilt", FieldState::default());
        assert!(
            markup.contains(
                r#"<input type="number" step="any" min="-90" max="90" id="serial.tilt" name="serial.tilt" value="12.5">"#
            ),
            "{markup}"
        );
    }

    #[test]
    fn a_checkbox_and_a_checkbox_group_render_their_checked_state() {
        let markup = render("serial.enabled", FieldState::default());
        assert!(
            markup.contains(r#"type="checkbox" id="serial.enabled" name="serial.enabled" checked"#),
            "{markup}"
        );
        let group = render("serial.targets", FieldState::default());
        assert!(
            group.contains(r#"id="serial.targets.-10" name="serial.targets" value="-10">"#),
            "{group}"
        );
        assert!(
            group.contains(r#"id="serial.targets.0" name="serial.targets" value="0" checked"#),
            "{group}"
        );
    }

    #[test]
    fn a_dropdown_offers_each_option_with_the_shown_one_selected() {
        let markup = render("serial.mode", FieldState::default());
        assert!(
            markup.contains(
                r#"<select id="serial.mode" name="serial.mode"><option value="auto">Every camera the scan places</option><option value="list" selected>list</option></select>"#
            ),
            "{markup}"
        );
        let pinned = ["serial.mode".to_string()];
        let markup = render(
            "serial.mode",
            FieldState {
                pinned: &pinned,
                ..FieldState::default()
            },
        );
        assert!(
            markup.contains(r#"<select id="serial.mode" name="serial.mode" disabled>"#),
            "{markup}"
        );
    }

    #[test]
    fn a_secret_renders_as_an_empty_password_input() {
        let markup = render("serial.key", FieldState::default());
        assert!(
            markup.contains(r#"type="password" id="serial.key" name="serial.key" value="""#),
            "{markup}"
        );
        assert!(!markup.contains(r#"value="stored""#), "{markup}");
    }

    #[test]
    fn a_file_only_leaf_is_named_with_whether_it_is_set() {
        let markup = render("server.tls", FieldState::default());
        assert!(markup.contains(r#"<div class="file-only" id="server.tls"><code>server.tls</code> is edited in the configuration file; it is not set.</div>"#), "{markup}");
        let set = field(
            model().field("server.tls").unwrap(),
            &json!({ "server": { "tls": { "cert": "x" } } }),
            FieldState::default(),
        )
        .into_string();
        assert!(set.contains("it is set."), "{set}");
        // A rule on a file-only field is shown in its note, not dropped.
        let errs = errors(&[("server.tls", "the certificate file does not exist")]);
        let rejected = field(
            model().field("server.tls").unwrap(),
            &json!({ "server": { "tls": { "cert": "x" } } }),
            FieldState {
                pinned: &[],
                errors: &errs,
            },
        )
        .into_string();
        assert!(
            rejected.contains(r#"class="file-only invalid""#),
            "{rejected}"
        );
        assert!(
            rejected.contains(r#"<div class="error">the certificate file does not exist</div>"#),
            "{rejected}"
        );
    }

    #[test]
    fn a_file_only_field_inside_a_variant_is_named_not_rendered_as_an_input() {
        let schema = json!({
            "type": "object",
            "properties": {
                "link": { "oneOf": [
                    { "type": "object", "properties": { "kind": { "const": "serial" }, "port": { "type": "string" } } },
                    { "type": "object", "properties": { "kind": { "const": "network" }, "host": { "type": "string" }, "lease": { "type": "string" } } },
                ] },
            },
        });
        let model = FieldModel::from_schema(&schema).with_file_only(&["/link/lease"]);
        let markup = field(
            model.field("link").unwrap(),
            &json!({ "link": { "kind": "network", "host": "h", "lease": "abc" } }),
            FieldState::default(),
        )
        .into_string();
        assert!(
            markup.contains(r#"<div class="file-only" id="link:network.lease"><code>link:network.lease</code> is edited in the configuration file; it is set.</div>"#),
            "{markup}"
        );
        assert!(!markup.contains(r#"name="link:network.lease""#), "{markup}");
        assert!(
            markup.contains(r#"name="link:network.host" value="h""#),
            "{markup}"
        );
    }

    #[test]
    fn a_fields_hint_sits_under_its_control_its_variants_and_its_file_only_note() {
        let schema = json!({
            "type": "object",
            "properties": {
                "camera": { "type": "object", "properties": {
                    "mode": { "oneOf": [
                        { "type": "string", "const": "fast" },
                        { "type": "string", "const": "slow" },
                    ], "x-setup-hint": "Fast skips the check." },
                    "tls": { "anyOf": [ { "type": "object", "properties": { "cert": { "type": "string" } } }, { "type": "null" } ], "x-setup-hint": "Doctor writes it." },
                    "plain": { "type": "string" },
                } },
                "link": { "oneOf": [
                    { "type": "object", "properties": { "kind": { "const": "serial" }, "port": { "type": "string", "x-setup-hint": "The device path." } } },
                    { "type": "object", "properties": { "kind": { "const": "network" }, "host": { "type": "string" } } },
                ], "x-setup-hint": "How the driver reaches the device." },
            },
        });
        let model = FieldModel::from_schema(&schema);
        let config = json!({
            "camera": { "mode": "fast", "tls": null, "plain": "" },
            "link": { "kind": "serial", "port": "/dev/a" },
        });
        let draw = |name: &str, state: FieldState<'_>| {
            field(model.field(name).unwrap(), &config, state).into_string()
        };
        let mode = draw("camera.mode", FieldState::default());
        assert!(
            mode.contains(r#"</select><div class="hint">Fast skips the check.</div></div>"#),
            "{mode}"
        );
        let tls = draw("camera.tls", FieldState::default());
        assert!(
            tls.contains(r#"it is not set.<div class="hint">Doctor writes it.</div></div>"#),
            "{tls}"
        );
        let link = draw("link", FieldState::default());
        assert!(
            link.contains(r#"name="link:serial.port" value="/dev/a"><div class="hint">The device path.</div></div>"#),
            "{link}"
        );
        assert!(
            link.contains(
                r#"</fieldset></div><div class="hint">How the driver reaches the device.</div>"#
            ),
            "{link}"
        );
        // The field's own explanation comes before a rejected save's error.
        let errs = errors(&[("camera.mode", "must be one of the listed values")]);
        let rejected = draw(
            "camera.mode",
            FieldState {
                pinned: &[],
                errors: &errs,
            },
        );
        assert!(
            rejected.contains(r#"<div class="hint">Fast skips the check.</div><div class="error">must be one of the listed values</div>"#),
            "{rejected}"
        );
        let plain = draw("camera.plain", FieldState::default());
        assert!(!plain.contains("hint"), "{plain}");
    }

    #[test]
    fn a_tagged_enum_renders_a_radio_per_variant_with_the_chosen_ones_values() {
        let markup = render("transport", FieldState::default());
        assert!(
            markup.contains(r#"type="radio" id="transport.usb" name="transport" value="usb">"#),
            "{markup}"
        );
        assert!(
            markup.contains(
                r#"type="radio" id="transport.udp" name="transport" value="udp" checked>"#
            ),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport:udp.host" value="mount.local""#),
            "{markup}"
        );
        // The unchosen variant's field renders empty, not with the udp value,
        // unless its schema states a default, which a switch starts from.
        assert!(
            markup.contains(r#"name="transport:usb.port" value="""#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport:usb.baud_rate" value="115200""#),
            "{markup}"
        );
        // The chosen variant shows its value, not its default.
        let usb = json!({ "transport": { "kind": "usb", "port": "/dev/a", "baud_rate": 9600 } });
        let chosen = field(
            model().field("transport").unwrap(),
            &usb,
            FieldState::default(),
        )
        .into_string();
        assert!(
            chosen.contains(r#"name="transport:usb.baud_rate" value="9600""#),
            "{chosen}"
        );
    }

    #[test]
    fn each_variants_fields_sit_in_a_fieldset_the_groups_script_switches() {
        let markup = render("transport", FieldState::default());
        // Every fieldset is drawn on: the script switches the unchosen one
        // off, and without script the save ignores its fields.
        assert_eq!(
            markup
                .matches(r#"<fieldset class="variant-fields">"#)
                .count(),
            2,
            "{markup}"
        );
        assert_eq!(markup.matches("<script>").count(), 1, "{markup}");
        assert!(
            markup.ends_with(&format!("<script>{VARIANT_SWITCH_SCRIPT}</script></div>")),
            "the script closes the group it switches: {markup}"
        );
    }

    #[test]
    fn a_variants_inner_field_shows_its_own_error() {
        let errs = errors(&[("transport:udp.host", "must not be empty")]);
        let markup = render(
            "transport",
            FieldState {
                pinned: &[],
                errors: &errs,
            },
        );
        assert!(
            markup.contains(r#"name="transport:udp.host" value="mount.local"><div class="error">must not be empty</div>"#),
            "{markup}"
        );
        let errs = errors(&[("transport", "must be one of the offered variants")]);
        let markup = render(
            "transport",
            FieldState {
                pinned: &[],
                errors: &errs,
            },
        );
        assert!(
            markup.contains(r#"class="field variants invalid""#),
            "{markup}"
        );
        assert!(
            markup.contains("must be one of the offered variants"),
            "{markup}"
        );
    }

    #[test]
    fn a_pin_on_a_variants_field_disables_that_field_alone() {
        // `--port` pins the host of the chosen variant: that input is
        // disabled with the note, the selector and the other inputs are not.
        let pins = ["transport:udp.host".to_string()];
        let markup = render(
            "transport",
            FieldState {
                pinned: &pins,
                errors: &[],
            },
        );
        assert!(
            markup.contains(r#"name="transport:udp.host" value="mount.local" disabled>"#),
            "{markup}"
        );
        assert!(
            markup.contains(
                r#"type="radio" id="transport.udp" name="transport" value="udp" checked>"#
            ),
            "the selector stays enabled: {markup}"
        );
        assert!(
            markup.contains(r#"name="transport:usb.port" value="">"#),
            "the other variant's field stays enabled: {markup}"
        );
        assert_eq!(
            markup.matches("Pinned by a command-line override").count(),
            1,
            "{markup}"
        );
    }

    #[test]
    fn a_pin_on_the_enum_disables_the_selector_and_every_field() {
        // `--transport` pins the whole block: the radios and every variant's
        // inputs are disabled, and the group carries the note once.
        let pins = ["transport".to_string()];
        let markup = render(
            "transport",
            FieldState {
                pinned: &pins,
                errors: &[],
            },
        );
        assert!(
            markup.contains(r#"class="field variants pinned""#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport" value="udp" checked disabled>"#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport" value="usb" disabled>"#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport:udp.host" value="mount.local" disabled>"#),
            "{markup}"
        );
        assert!(
            markup.contains(r#"name="transport:usb.port" value="" disabled>"#),
            "{markup}"
        );
    }

    #[test]
    fn the_device_list_links_enabled_devices_and_notes_disabled_ones() {
        let devices = vec![
            DeviceRow {
                name: "Panel".to_string(),
                device_type: "covercalibrator".to_string(),
                device_number: 0,
                enabled: true,
                pinned: false,
                href: "setup/v1/covercalibrator/0/setup".to_string(),
            },
            DeviceRow {
                name: "Heater".to_string(),
                device_type: "switch".to_string(),
                device_number: 0,
                enabled: false,
                pinned: false,
                href: "setup/v1/switch/0/setup".to_string(),
            },
        ];
        let markup = device_list(&devices).into_string();
        assert!(
            markup.contains(r#"<a href="setup/v1/covercalibrator/0/setup">Panel</a>"#),
            "{markup}"
        );
        assert!(
            !markup.contains(r#"<a href="setup/v1/switch/0/setup""#),
            "{markup}"
        );
        assert!(
            markup
                .contains("Disabled: its page is not served; enable it in the configuration file."),
            "{markup}"
        );
        assert!(device_list(&[]).into_string().contains("serves no device"));
    }

    #[test]
    fn a_device_a_flag_disables_is_noted_as_pinned_not_as_a_file_edit() {
        // `--enable-switch false` over a file that says true: editing the
        // file does nothing, so the note names the flags instead.
        let devices = vec![DeviceRow {
            name: "Heater".to_string(),
            device_type: "switch".to_string(),
            device_number: 0,
            enabled: false,
            pinned: true,
            href: "setup/v1/switch/0/setup".to_string(),
        }];
        let markup = device_list(&devices).into_string();
        assert!(
            markup.contains("Disabled by a command-line override: its page is not served; change the service's launch flags to serve it."),
            "{markup}"
        );
        assert!(
            !markup.contains("enable it in the configuration file"),
            "{markup}"
        );
    }

    #[test]
    fn the_saved_page_says_what_happened_and_waits_only_for_a_reload() {
        let reloading = saved_body("dsd-fp2", &Saved::Reloading, None, "", &[]).into_string();
        assert!(reloading.contains(r#"id="saved-reloading""#), "{reloading}");
        assert!(reloading.contains("<script>"), "{reloading}");
        assert!(
            reloading.contains("var url = null || window.location.href;"),
            "{reloading}"
        );
        assert!(reloading.contains(r#"mode: "no-cors""#), "{reloading}");
        let moved_dest = Destination {
            url: "http://h:1/setup".to_string(),
            note: None,
        };
        let moved =
            saved_body("dsd-fp2", &Saved::Reloading, Some(&moved_dest), "", &[]).into_string();
        assert!(
            moved.contains(r#"var url = "http://h:1/setup" || window.location.href;"#),
            "{moved}"
        );
        assert!(moved.contains(r#"<p id="moved">The setup pages now live at <a href="http://h:1/setup">http://h:1/setup</a>.</p>"#), "{moved}");
        let noted = Destination {
            url: "../../../../setup".to_string(),
            note: Some("This device is disabled now.".to_string()),
        };
        let noted = saved_body("dsd-fp2", &Saved::Reloading, Some(&noted), "", &[]).into_string();
        assert!(noted.contains(r#"<p id="moved">This device is disabled now. <a href="../../../../setup">Open the server page</a>.</p>"#), "{noted}");
        let unchanged = saved_body("dsd-fp2", &Saved::Unchanged, None, "", &[]).into_string();
        assert!(unchanged.contains(r#"id="saved-unchanged""#), "{unchanged}");
        assert!(!unchanged.contains("<script>"), "{unchanged}");
        let restart_dest = Destination {
            url: "http://h:2/setup".to_string(),
            note: None,
        };
        let restart = saved_body(
            "rp",
            &Saved::RestartRequired(vec!["server.port".to_string()]),
            Some(&restart_dest),
            "",
            &[],
        )
        .into_string();
        assert!(restart.contains(r#"id="saved-restart""#), "{restart}");
        assert!(restart.contains("server.port"), "{restart}");
        assert!(
            restart.contains("Nothing answers at the new address until the restart"),
            "{restart}"
        );
        assert!(
            restart.contains("<code>sudo systemctl restart rusty-photon-rp</code>"),
            "{restart}"
        );
        assert!(
            restart.contains("<code>Restart-Service rusty-photon-rp</code>"),
            "{restart}"
        );
        assert!(
            restart.contains("<code>brew services restart rusty-photon-rp</code>"),
            "{restart}"
        );
        assert!(
            !reloading.contains(r#"class="restart-how""#),
            "a reload says nothing of a restart: {reloading}"
        );
    }

    #[test]
    fn the_document_inlines_the_stylesheet_and_carries_the_generation() {
        let doc = document("dsd-fp2", "Setup", "gen-7", &html! { p { "hi" } });
        assert!(doc.starts_with("<!DOCTYPE html>"), "{doc}");
        assert!(doc.contains("<title>Setup · dsd-fp2</title>"), "{doc}");
        assert!(
            doc.contains(r#"<meta name="setup-generation" content="gen-7">"#),
            "{doc}"
        );
        assert!(doc.contains("--accent-a"), "{doc}");
        assert!(doc.contains("<p>hi</p>"), "{doc}");
    }

    #[test]
    fn the_rejected_banner_names_a_general_error_or_the_marked_fields() {
        let general = rejected_banner(
            &[FieldError {
                path: String::new(),
                msg: "not a configuration: x".to_string(),
            }],
            &["a"],
        )
        .into_string();
        assert!(general.contains("not a configuration: x"), "{general}");
        let fields = rejected_banner(
            &[FieldError {
                path: "a".to_string(),
                msg: "bad".to_string(),
            }],
            &["a"],
        )
        .into_string();
        assert!(fields.contains("Fix the marked fields"), "{fields}");
        // An error on a field the page does not show is named, never dropped.
        let elsewhere = rejected_banner(
            &[FieldError {
                path: "panel.unique_id".to_string(),
                msg: "must not be empty".to_string(),
            }],
            &["serial.port"],
        )
        .into_string();
        assert!(
            elsewhere.contains(
                "<code>panel.unique_id</code> must not be empty — a field this page does not show."
            ),
            "{elsewhere}"
        );
        assert!(!elsewhere.contains("Fix the marked fields"), "{elsewhere}");
    }
}

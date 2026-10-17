//! The pages' markup: the shell, a field per [`FieldKind`], the device list,
//! the banners, and the page a save answers with.

use maud::{html, Markup, PreEscaped, DOCTYPE};
use rusty_photon_config::actions::FieldError;
use serde_json::Value;

use crate::form::{array_contains, bool_at, text_at};
use crate::schema::{FieldKind, FieldSpec};

/// The stylesheet every page inlines.
const STYLESHEET: &str = include_str!("../assets/setup.css");

/// How a field renders beyond its value.
#[derive(Debug, Clone, Copy, Default)]
pub struct FieldState<'a> {
    /// Pinned by a command-line override: disabled, with a note.
    pub pinned: bool,
    /// The error a rejected save attached to the field.
    pub error: Option<&'a str>,
}

/// The whole document: the shell around `body`, titled `title`, for the
/// service named `service`.
#[must_use]
pub fn document(service: &str, title: &str, body: &Markup) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
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

/// The error a rejected save puts above the form.
#[must_use]
pub fn rejected_banner(errors: &[FieldError]) -> Markup {
    let general: Vec<&FieldError> = errors.iter().filter(|e| e.path.is_empty()).collect();
    html! {
        @if general.is_empty() {
            (banner("error", "rejected", "Some values were rejected. Fix the marked fields and save again."))
        } @else {
            @for e in general { (banner("error", "rejected", &e.msg)) }
        }
    }
}

/// One field, filled from `config`, in `state`.
#[must_use]
pub fn field(spec: &FieldSpec, config: &Value, state: FieldState<'_>) -> Markup {
    match spec.kind() {
        FieldKind::FileOnly => file_only(spec, config),
        FieldKind::Variants { tag, variants } => variants_field(spec, tag, variants, config, state),
        _ => scalar_field(spec, spec.pointer(), config, state),
    }
}

/// A scalar field reading its value at `pointer` (a variant's field reads
/// under its enum's object).
fn scalar_field(spec: &FieldSpec, pointer: &str, config: &Value, state: FieldState<'_>) -> Markup {
    let name = spec.name();
    let disabled = state.pinned;
    html! {
        div.field.pinned[disabled].invalid[state.error.is_some()] {
            @match spec.kind() {
                FieldKind::Bool => {
                    div.checkbox {
                        input type="checkbox" id=(name) name=(name)
                            checked[bool_at(config, pointer)] disabled[disabled];
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
                                    checked[array_contains(config, pointer, *option)]
                                    disabled[disabled];
                                label for=(option_id) { (option) }
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
                _ => {
                    label for=(name) { (spec.label()) }
                    input type=(spec.input_type()) id=(name) name=(name)
                        value=(text_at(config, pointer)) disabled[disabled];
                }
            }
            @if let Some(e) = state.error { div.error { (e) } }
            @if state.pinned {
                div.hint { "Pinned by a command-line override; change the service's launch flags to edit it." }
            }
        }
    }
}

/// A leaf the form cannot edit, named as file-only with whether it is set.
fn file_only(spec: &FieldSpec, config: &Value) -> Markup {
    let set = config.pointer(spec.pointer()).is_some_and(|v| !v.is_null());
    html! {
        div.file-only id=(spec.name()) {
            code { (spec.name()) }
            " is edited in the configuration file; it is "
            @if set { "set." } @else { "not set." }
        }
    }
}

/// A tagged enum: one radio per variant, the chosen variant's fields shown.
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
    html! {
        div.field.variants.invalid[state.error.is_some()] role="group" aria-label=(spec.label()) {
            span.group-label { (spec.label()) }
            @for variant in variants {
                @let id = format!("{name}.{}", variant.name);
                div.variant {
                    div.choice.checkbox {
                        input type="radio" id=(id) name=(name) value=(variant.name)
                            checked[chosen(&variant.name)] disabled[state.pinned];
                        label for=(id) { (variant.name) }
                    }
                    div.variant-fields {
                        @for inner in &variant.fields {
                            @let pointer = format!("{enum_pointer}{}", inner.pointer());
                            @let shown = if chosen(&variant.name) { config } else { &Value::Null };
                            (scalar_field(inner, &pointer, shown, FieldState { pinned: state.pinned, error: None }))
                        }
                    }
                }
            }
            @if let Some(e) = state.error { div.error { (e) } }
        }
    }
}

/// A device the server page lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub name: String,
    pub device_type: String,
    pub device_number: u32,
    pub enabled: bool,
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
                                span.note { "Disabled: its page is not served; enable it in the configuration file." }
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

/// The page a save answers with: what happened, where the pages now live
/// if the address changed (`moved_to`, an absolute URL), and the wait.
#[must_use]
pub fn saved_body(saved: &Saved, moved_to: Option<&str>, back: &str) -> Markup {
    let target = moved_to.map(str::to_string);
    let target_json = serde_json::to_string(&target).unwrap_or_else(|_| "null".to_string());
    html! {
        div.card {
            @match saved {
                Saved::Unchanged => {
                    (banner("ok", "saved-unchanged", "Saved. Nothing changed, so no reload is needed."))
                }
                Saved::Reloading => {
                    (banner("applying", "saved-reloading", "Saved. The service is reloading; this page reopens when it answers again."))
                }
                Saved::RestartRequired(fields) => {
                    div class="banner warn" id="saved-restart" {
                        span.dot {}
                        span {
                            "Saved. These fields take effect when the service is restarted: "
                            span.mono { (fields.join(", ")) }
                        }
                    }
                }
            }
            @if let Some(url) = moved_to {
                p id="moved" {
                    "The setup pages now live at "
                    a href=(url) { (url) }
                    "."
                }
            }
            p {
                @match (saved, moved_to) {
                    (Saved::Reloading, Some(url)) => { "If it does not reopen, " a href=(url) { "open the new address" } " once the service is back." }
                    (Saved::Reloading, None) => { "If it does not reopen, " a href=(back) { "reload the page" } " once the service is back." }
                    (Saved::RestartRequired(_), Some(_)) => { "Nothing answers at the new address until the restart. " a href=(back) { "Back to the page" } "." }
                    _ => { a href=(back) { "Back to the page" } }
                }
            }
            @if matches!(saved, Saved::Reloading) {
                script { (PreEscaped(reload_wait_script(&target_json))) }
            }
        }
    }
}

/// The reload wait: poll the page's URL until any HTTP answer arrives — a
/// `401` after a credential change included, which means the listener is
/// back — then navigate to it.
fn reload_wait_script(target_json: &str) -> String {
    format!(
        "(function () {{\n  var url = {target_json} || window.location.href;\n  function probe() {{\n    fetch(url, {{ cache: \"no-store\", credentials: \"same-origin\" }})\n      .then(function () {{ window.location.assign(url); }})\n      .catch(function () {{ window.setTimeout(probe, 500); }});\n  }}\n  window.setTimeout(probe, 1000);\n}})();"
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
                    "enabled": { "type": "boolean" },
                    "key": { "type": "string" },
                    "targets": { "type": "array", "items": { "type": "integer", "enum": [-10, 0] } },
                } },
                "server": { "type": "object", "properties": {
                    "tls": { "anyOf": [ { "type": "object", "properties": { "cert": { "type": "string" } } }, { "type": "null" } ] },
                } },
                "transport": { "oneOf": [
                    { "type": "object", "properties": { "kind": { "const": "usb" }, "port": { "type": "string" } } },
                    { "type": "object", "properties": { "kind": { "const": "udp" }, "host": { "type": "string" } } },
                ] },
            },
        });
        FieldModel::from_schema(&schema).with_secrets(&["/serial/key"])
    }

    fn config() -> Value {
        json!({
            "serial": { "port": "/dev/a", "baud_rate": 9600, "enabled": true, "key": "stored", "targets": [0] },
            "server": { "tls": null },
            "transport": { "kind": "udp", "host": "mount.local" },
        })
    }

    fn render(name: &str, state: FieldState<'_>) -> String {
        field(model().field(name).unwrap(), &config(), state).into_string()
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
        let pinned = render(
            "serial.port",
            FieldState {
                pinned: true,
                error: None,
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
        let markup = render(
            "serial.baud_rate",
            FieldState {
                pinned: false,
                error: Some("must be greater than 0"),
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
        // The unchosen variant's field renders empty, not with the udp value.
        assert!(
            markup.contains(r#"name="transport:usb.port" value="""#),
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
                href: "setup/v1/covercalibrator/0/setup".to_string(),
            },
            DeviceRow {
                name: "Heater".to_string(),
                device_type: "switch".to_string(),
                device_number: 0,
                enabled: false,
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
            markup.contains("Disabled: its page is not served"),
            "{markup}"
        );
        assert!(device_list(&[]).into_string().contains("serves no device"));
    }

    #[test]
    fn the_saved_page_says_what_happened_and_waits_only_for_a_reload() {
        let reloading = saved_body(&Saved::Reloading, None, "").into_string();
        assert!(reloading.contains(r#"id="saved-reloading""#), "{reloading}");
        assert!(reloading.contains("<script>"), "{reloading}");
        assert!(
            reloading.contains("var url = null || window.location.href;"),
            "{reloading}"
        );
        let moved = saved_body(&Saved::Reloading, Some("http://h:1/setup"), "").into_string();
        assert!(
            moved.contains(r#"var url = "http://h:1/setup" || window.location.href;"#),
            "{moved}"
        );
        assert!(moved.contains(r#"<p id="moved">The setup pages now live at <a href="http://h:1/setup">http://h:1/setup</a>.</p>"#), "{moved}");
        let unchanged = saved_body(&Saved::Unchanged, None, "").into_string();
        assert!(unchanged.contains(r#"id="saved-unchanged""#), "{unchanged}");
        assert!(!unchanged.contains("<script>"), "{unchanged}");
        let restart = saved_body(
            &Saved::RestartRequired(vec!["server.port".to_string()]),
            Some("http://h:2/setup"),
            "",
        )
        .into_string();
        assert!(restart.contains(r#"id="saved-restart""#), "{restart}");
        assert!(restart.contains("server.port"), "{restart}");
        assert!(
            restart.contains("Nothing answers at the new address until the restart"),
            "{restart}"
        );
    }

    #[test]
    fn the_document_inlines_the_stylesheet() {
        let doc = document("dsd-fp2", "Setup", &html! { p { "hi" } });
        assert!(doc.starts_with("<!DOCTYPE html>"), "{doc}");
        assert!(doc.contains("<title>Setup · dsd-fp2</title>"), "{doc}");
        assert!(doc.contains("--accent-a"), "{doc}");
        assert!(doc.contains("<p>hi</p>"), "{doc}");
    }

    #[test]
    fn the_rejected_banner_names_a_general_error_or_the_marked_fields() {
        let general = rejected_banner(&[FieldError {
            path: String::new(),
            msg: "not a configuration: x".to_string(),
        }])
        .into_string();
        assert!(general.contains("not a configuration: x"), "{general}");
        let fields = rejected_banner(&[FieldError {
            path: "a".to_string(),
            msg: "bad".to_string(),
        }])
        .into_string();
        assert!(fields.contains("Fix the marked fields"), "{fields}");
    }
}

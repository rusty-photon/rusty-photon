//! [`OverridePages`]: the own part of the setup pages of a driver whose
//! hardware names its devices, and whose configuration only renames them.
//!
//! Such a driver enumerates its devices at each start, each keyed by an id
//! it reads from the device, and its configuration holds a `devices` map of
//! overrides under those ids (`docs/services/zwo-focuser.md`, "Setup
//! pages"). The server page lists the devices the build registered; each
//! device's page shows what the driver knows about it and edits its
//! override's name and description.
//!
//! The override editor is shared with the camera drivers' page of a camera
//! served without a list (`claims`).

use maud::{html, Markup};
use rusty_photon_config::actions::{DeviceSection, FieldError};
use rusty_photon_setup::{
    BoxFuture, CustomSection, DriverPages, FormValues, PageScope, SectionView,
};
use serde_json::{Map, Value};

/// The configuration key of the overrides map.
pub(crate) const OVERRIDES: &str = "devices";

/// An override's name and description, as a device's page names them.
pub(crate) const OVERRIDE_NAME: &str = "override.name";
pub(crate) const OVERRIDE_DESCRIPTION: &str = "override.description";

/// The `devices` override under `key` in `config`, if any.
pub(crate) fn override_of<'a>(config: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    config.get(OVERRIDES)?.get(key)?.as_object()
}

pub(crate) fn text_in(object: Option<&Map<String, Value>>, key: &str) -> String {
    object
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Set `key` in `object` to `value`, trimmed; a blank value removes it.
pub(crate) fn set_text(object: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        _ = object.remove(key);
    } else {
        _ = object.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// The `devices` map in `config`, created when the file has none.
pub(crate) fn overrides_mut(config: &mut Value) -> Option<&mut Map<String, Value>> {
    let object = config.as_object_mut()?;
    object
        .entry(OVERRIDES)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

/// The facts a device's page shows.
pub(crate) fn facts(rows: &[(&str, &str)]) -> Markup {
    html! {
        dl.facts {
            @for (term, value) in rows {
                dt { (term) }
                dd { (value) }
            }
        }
    }
}

/// The override fieldset of the device whose override is read under `key`,
/// a `noun` in its hints: the file's names, or, re-rendering a save, the
/// submitted ones. `note` follows where the override is saved.
pub(crate) fn override_fields(
    key: &str,
    noun: &str,
    view: SectionView<'_>,
    note: Option<Markup>,
) -> Markup {
    let stored = override_of(view.config, key);
    let value = |control: &str, field: &str| {
        view.submitted
            .and_then(|form| form.get(control))
            .map_or_else(|| text_in(stored, field), str::to_string)
    };
    html! {
        fieldset {
            legend { "Override" }
            div.field {
                label for=(OVERRIDE_NAME) { "Name" }
                input type="text" id=(OVERRIDE_NAME) name=(OVERRIDE_NAME) value=(value(OVERRIDE_NAME, "name"));
                div.hint { "Blank keeps the name the " (noun) " gives itself." }
            }
            div.field {
                label for=(OVERRIDE_DESCRIPTION) { "Description" }
                input type="text" id=(OVERRIDE_DESCRIPTION) name=(OVERRIDE_DESCRIPTION)
                    value=(value(OVERRIDE_DESCRIPTION, "description"));
            }
            div.hint {
                "Saved under " code { (key) } " in the " code { (OVERRIDES) } " map."
                @if let Some(note) = note { " " (note) }
            }
        }
    }
}

/// Apply the override fieldset's submission onto the override under `key`
/// in `config`: the first save that names either field creates it, a blank
/// field removes its key, and an override left with no key is removed. Any
/// other key the override holds is kept.
pub(crate) fn apply_override(key: &str, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
    let Some(overrides) = overrides_mut(config) else {
        return vec![FieldError {
            path: String::new(),
            msg: "the configuration file is not a JSON object".to_string(),
        }];
    };
    let mut names = overrides
        .get(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    set_text(
        &mut names,
        "name",
        form.get(OVERRIDE_NAME).unwrap_or_default(),
    );
    set_text(
        &mut names,
        "description",
        form.get(OVERRIDE_DESCRIPTION).unwrap_or_default(),
    );
    if names.is_empty() {
        _ = overrides.remove(key);
    } else {
        _ = overrides.insert(key.to_string(), Value::Object(names));
    }
    Vec::new()
}

/// One device the build registered, as the pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideDevice {
    /// The name it is served under: its override's, or its own.
    pub name: String,
    /// The key its override is read under.
    pub key: String,
    /// What the driver knows about it, each a term and its value, shown
    /// above the override.
    pub facts: Vec<(String, String)>,
}

/// A driver's own part of its pages when its hardware names its devices: a
/// page per device the build registered, each editing that device's
/// `devices` override, which the walker leaves off every page.
#[derive(Debug)]
pub struct OverridePages {
    device_type: &'static str,
    noun: &'static str,
    devices: Vec<OverrideDevice>,
}

impl OverridePages {
    /// The pages of `devices`, served as `device_type` (in its Alpaca path
    /// spelling) and numbered in the order given — the order the build
    /// registers them in. `noun` names one in the pages' hints.
    #[must_use]
    pub const fn new(
        device_type: &'static str,
        noun: &'static str,
        devices: Vec<OverrideDevice>,
    ) -> Self {
        Self {
            device_type,
            noun,
            devices,
        }
    }
}

impl DriverPages for OverridePages {
    fn owned_keys(&self) -> Vec<String> {
        vec![OVERRIDES.to_string()]
    }

    fn devices(&self) -> Option<Vec<DeviceSection>> {
        Some(
            self.devices
                .iter()
                .zip(0_u32..)
                .map(|(device, number)| DeviceSection {
                    device_type: self.device_type.to_string(),
                    device_number: number,
                    pointer: String::new(),
                    name: device.name.clone(),
                    enabled: true,
                })
                .collect(),
        )
    }

    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        _saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        let page = match scope {
            PageScope::Device {
                device_type,
                device_number,
            } if device_type == self.device_type => usize::try_from(device_number)
                .ok()
                .and_then(|index| self.devices.get(index)),
            _ => None,
        }
        .map(|device| -> Box<dyn CustomSection> {
            Box::new(OverridePage {
                device: device.clone(),
                noun: self.noun,
            })
        });
        Box::pin(async move { page.into_iter().collect() })
    }
}

/// A device's page: what the driver knows about it, and its override.
struct OverridePage {
    device: OverrideDevice,
    noun: &'static str,
}

impl CustomSection for OverridePage {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let rows: Vec<(&str, &str)> = self
            .device
            .facts
            .iter()
            .map(|(term, value)| (term.as_str(), value.as_str()))
            .collect();
        html! {
            (facts(&rows))
            (override_fields(&self.device.key, self.noun, view, None))
        }
    }

    fn has_controls(&self) -> bool {
        true
    }

    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
        apply_override(&self.device.key, form, config)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(name: &str, key: &str) -> OverrideDevice {
        OverrideDevice {
            name: name.to_string(),
            key: key.to_string(),
            facts: vec![("Model".to_string(), "ZWO EAF".to_string())],
        }
    }

    fn pages() -> OverridePages {
        OverridePages::new(
            "focuser",
            "focuser",
            vec![device("Main", "aaaa"), device("ZWO EAF", "bbbb")],
        )
    }

    fn form(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    fn view(config: &Value) -> SectionView<'_> {
        SectionView {
            config,
            submitted: None,
            errors: &[],
            host: None,
        }
    }

    async fn page_of(
        pages: &OverridePages,
        device_type: &str,
        number: u32,
    ) -> Vec<Box<dyn CustomSection>> {
        pages
            .sections(
                PageScope::Device {
                    device_type,
                    device_number: number,
                },
                false,
            )
            .await
    }

    #[test]
    fn the_overrides_map_is_the_drivers_own() {
        assert_eq!(pages().owned_keys(), vec!["devices".to_string()]);
    }

    #[test]
    fn the_devices_are_numbered_in_the_order_given_with_no_section_of_their_own() {
        let devices = pages().devices().unwrap();
        let listed: Vec<_> = devices
            .iter()
            .map(|d| {
                (
                    d.device_type.as_str(),
                    d.device_number,
                    d.name.as_str(),
                    d.pointer.as_str(),
                    d.enabled,
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                ("focuser", 0, "Main", "", true),
                ("focuser", 1, "ZWO EAF", "", true)
            ]
        );
    }

    #[tokio::test]
    async fn a_device_page_edits_that_devices_override() {
        let pages = pages();
        let sections = page_of(&pages, "focuser", 1).await;
        assert_eq!(sections.len(), 1);
        assert!(sections[0].has_controls());
        let mut config = json!({"devices": {"aaaa": {"name": "Main"}}});
        let errors = sections[0].apply(
            &form(&[(OVERRIDE_NAME, "Second"), (OVERRIDE_DESCRIPTION, "")]),
            &mut config,
        );
        assert_eq!(errors, Vec::<FieldError>::new());
        assert_eq!(
            config,
            json!({"devices": {"aaaa": {"name": "Main"}, "bbbb": {"name": "Second"}}})
        );
    }

    #[tokio::test]
    async fn no_section_for_another_type_a_number_not_served_or_the_server_page() {
        let pages = pages();
        assert!(page_of(&pages, "camera", 0).await.is_empty());
        assert!(page_of(&pages, "focuser", 2).await.is_empty());
        assert!(pages.sections(PageScope::Server, false).await.is_empty());
    }

    #[tokio::test]
    async fn a_device_page_shows_its_facts_and_its_stored_names() {
        let pages = pages();
        let sections = page_of(&pages, "focuser", 0).await;
        let config = json!({"devices": {"aaaa": {"name": "Main", "description": "On the Askar"}}});
        let html = sections[0].render(view(&config)).into_string();
        assert!(html.contains("<dt>Model</dt><dd>ZWO EAF</dd>"), "{html}");
        assert!(
            html.contains(r#"name="override.name" value="Main""#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="override.description" value="On the Askar""#),
            "{html}"
        );
        assert!(html.contains("the focuser gives itself"), "{html}");
        assert!(html.contains("<code>aaaa</code>"), "{html}");
    }

    #[test]
    fn a_rerendered_save_shows_what_was_submitted() {
        let config = json!({"devices": {"aaaa": {"name": "Main"}}});
        let submitted = form(&[(OVERRIDE_NAME, "Typed"), (OVERRIDE_DESCRIPTION, "")]);
        let view = SectionView {
            config: &config,
            submitted: Some(&submitted),
            errors: &[],
            host: None,
        };
        let html = override_fields("aaaa", "focuser", view, None).into_string();
        assert!(
            html.contains(r#"name="override.name" value="Typed""#),
            "{html}"
        );
    }

    #[test]
    fn the_first_save_that_names_either_creates_the_override_and_the_map() {
        let mut config = json!({"server": {"port": 1}});
        let errors = apply_override(
            "aaaa",
            &form(&[
                (OVERRIDE_NAME, ""),
                (OVERRIDE_DESCRIPTION, " On the Askar "),
            ]),
            &mut config,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            config,
            json!({"server": {"port": 1}, "devices": {"aaaa": {"description": "On the Askar"}}})
        );
    }

    #[test]
    fn clearing_both_names_removes_the_override_and_keeps_the_others() {
        let mut config = json!({"devices": {"aaaa": {"name": "Main"}, "cccc": {"name": "Spare"}}});
        _ = apply_override(
            "aaaa",
            &form(&[(OVERRIDE_NAME, " "), (OVERRIDE_DESCRIPTION, "")]),
            &mut config,
        );
        assert_eq!(config, json!({"devices": {"cccc": {"name": "Spare"}}}));
    }

    #[test]
    fn an_overrides_other_keys_survive_and_keep_it() {
        let mut config = json!({"devices": {"aaaa": {"name": "Main", "filter_names": ["L"]}}});
        _ = apply_override(
            "aaaa",
            &form(&[(OVERRIDE_NAME, ""), (OVERRIDE_DESCRIPTION, "")]),
            &mut config,
        );
        assert_eq!(
            config,
            json!({"devices": {"aaaa": {"filter_names": ["L"]}}})
        );
    }

    #[test]
    fn a_file_that_is_not_an_object_is_refused() {
        let mut config = json!([]);
        let errors = apply_override("aaaa", &form(&[(OVERRIDE_NAME, "Main")]), &mut config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(config, json!([]));
    }
}

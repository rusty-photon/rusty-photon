//! [`ClaimsPages`]: a camera driver's own part of its setup pages — the
//! Cameras section that edits `usb_devices` on the server page, and each
//! camera's page (the driver-setup-pages plan's D11, the device-claims
//! plan's C8; `docs/services/svbony-camera.md`, "Setup pages").
//!
//! The driver records what its build registered — each number's camera or
//! placeholder, and every camera the join placed — and hands a scan the
//! pages run on each request for the server page, so its rows are the bus
//! as it is now. The scan is the passive one a start takes: no SDK call,
//! nothing opened.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use maud::{html, Markup};
use rusty_photon_config::actions::{DeviceSection, FieldError};
use rusty_photon_setup::{
    BoxFuture, Confirmation, CustomSection, DriverPages, FormValues, PageScope, SectionView,
};
use serde_json::{Map, Value};
use tracing::debug;

/// The camera device type in its Alpaca path spelling.
const DEVICE_TYPE: &str = "camera";

/// The configuration keys the Cameras section edits.
const LIST: &str = "usb_devices";
const OVERRIDES: &str = "devices";

/// The mode radio, and its two values.
const MODE: &str = "usb_devices.mode";
const AUTO: &str = "auto";
const LISTED: &str = "list";

/// A camera page's link to the Cameras section of the server page.
const CAMERAS_LINK: &str = "../../../../setup#cameras";

/// A camera's override, edited on its own page.
const OVERRIDE_NAME: &str = "override.name";
const OVERRIDE_DESCRIPTION: &str = "override.description";

/// A device on the bus that can be one of the driver's cameras, as a scan
/// found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusCamera {
    /// Its port, in the platform's native spelling.
    pub port: String,
    /// What the bus calls it: its product string, else `<vid>:<pid>`.
    pub label: String,
}

/// What a device number serves, as the last start or reload registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// A camera the join placed on `port`.
    Camera {
        port: String,
        /// The SDK's model name.
        model: String,
        unique_id: String,
        /// The key its `devices` override is read under.
        override_key: String,
        /// Whether a `usb_devices` entry lists it.
        listed: bool,
    },
    /// A placeholder holding a listed number.
    Placeholder { port: String, reason: String },
}

impl Served {
    fn port(&self) -> &str {
        match self {
            Self::Camera { port, .. } | Self::Placeholder { port, .. } => port,
        }
    }

    /// What the Cameras section says the running service serves at
    /// `number`.
    fn label(&self, number: u32) -> String {
        match self {
            Self::Camera { .. } => format!("camera {number}"),
            Self::Placeholder { .. } => format!("placeholder {number}"),
        }
    }
}

/// A camera the join placed on a port, listed or not: what the no-list
/// default serves, numbered in port order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedCamera {
    pub port: String,
    /// The key its `devices` override is read under.
    pub override_key: String,
}

/// The scan the server page runs. It blocks, so it runs on the blocking
/// pool.
pub type ScanCameras = Arc<dyn Fn() -> Result<Vec<BusCamera>, String> + Send + Sync>;

/// What a build registered, in number order, with every camera the join
/// placed.
#[derive(Debug, Clone, Default)]
struct Registry {
    /// Each device number's name and what it serves; the index is the
    /// number.
    served: Vec<(String, Served)>,
    placed: Vec<PlacedCamera>,
}

/// A camera driver's own part of its setup pages, built once per start or
/// reload.
pub struct ClaimsPages {
    /// The SDK's display name (`SVBony`).
    sdk: &'static str,
    scan: ScanCameras,
    /// Port order (`rusty-photon-doctor-checks`' `compare_ports`).
    order: fn(&str, &str) -> Ordering,
    registry: Mutex<Registry>,
}

impl std::fmt::Debug for ClaimsPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimsPages")
            .field("sdk", &self.sdk)
            .finish_non_exhaustive()
    }
}

impl ClaimsPages {
    /// The pages of a driver for `sdk`'s cameras, whose server page runs
    /// `scan` and orders ports by `order`.
    #[must_use]
    pub fn new(sdk: &'static str, scan: ScanCameras, order: fn(&str, &str) -> Ordering) -> Self {
        Self {
            sdk,
            scan,
            order,
            registry: Mutex::new(Registry::default()),
        }
    }

    /// Record the next device number the build registers — its name and
    /// what it serves — and answer the number, which its page is served at.
    pub fn register(&self, name: impl Into<String>, served: Served) -> u32 {
        let mut registry = self.lock();
        registry.served.push((name.into(), served));
        u32::try_from(registry.served.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    /// Record every camera the join placed, listed or not, in port order.
    pub fn placed(&self, placed: Vec<PlacedCamera>) {
        self.lock().placed = placed;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl DriverPages for ClaimsPages {
    fn owned_keys(&self) -> Vec<String> {
        vec![LIST.to_string(), OVERRIDES.to_string()]
    }

    fn devices(&self) -> Option<Vec<DeviceSection>> {
        Some(
            self.lock()
                .served
                .iter()
                .zip(0_u32..)
                .map(|((name, _), number)| DeviceSection {
                    device_type: DEVICE_TYPE.to_string(),
                    device_number: number,
                    pointer: String::new(),
                    name: name.clone(),
                    enabled: true,
                })
                .collect(),
        )
    }

    fn sections<'a>(&'a self, scope: PageScope<'a>) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        Box::pin(async move {
            let registry = self.lock().clone();
            match scope {
                PageScope::Server => {
                    let scan = Arc::clone(&self.scan);
                    // A scan that panics has failed to scan, like one that
                    // errors.
                    let bus = tokio::task::spawn_blocking(move || scan())
                        .await
                        .unwrap_or_else(|error| {
                            Err(format!("the USB scan did not finish: {error}"))
                        });
                    if let Err(error) = &bus {
                        debug!(%error, "the Cameras section's USB scan failed");
                    }
                    let section: Box<dyn CustomSection> = Box::new(CamerasSection {
                        sdk: self.sdk,
                        bus,
                        registry,
                        order: self.order,
                    });
                    vec![section]
                }
                PageScope::Device {
                    device_type,
                    device_number,
                } => device_page(&registry, device_type, device_number)
                    .into_iter()
                    .collect(),
            }
        })
    }
}

/// The section a camera's page draws, by what its number serves.
fn device_page(
    registry: &Registry,
    device_type: &str,
    device_number: u32,
) -> Option<Box<dyn CustomSection>> {
    if device_type != DEVICE_TYPE {
        return None;
    }
    let index = usize::try_from(device_number).ok()?;
    let (_, served) = registry.served.get(index)?;
    let page: Box<dyn CustomSection> = match served.clone() {
        Served::Camera {
            port,
            model,
            unique_id,
            listed: true,
            ..
        } => Box::new(ListedPage {
            number: device_number,
            port,
            model,
            unique_id,
        }),
        Served::Camera {
            port,
            model,
            unique_id,
            override_key,
            listed: false,
        } => Box::new(OverridePage {
            port,
            model,
            unique_id,
            key: override_key,
        }),
        Served::Placeholder { port, reason } => Box::new(PlaceholderPage { port, reason }),
    };
    Some(page)
}

/// One `usb_devices` entry, as the file holds it.
#[derive(Debug, Clone)]
struct Entry {
    number: u64,
    port: String,
    object: Map<String, Value>,
}

impl Entry {
    fn text(&self, key: &str) -> String {
        self.object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn has_names(&self) -> bool {
        !self.text("name").is_empty() || !self.text("description").is_empty()
    }
}

/// The `usb_devices` list in `config`, `None` with no list. An entry
/// without a port or a number is not one the load accepts, and is left out.
fn entries(config: &Value) -> Option<Vec<Entry>> {
    let list = config.get(LIST)?.as_array()?;
    Some(
        list.iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                Some(Entry {
                    number: object.get("device_number")?.as_u64()?,
                    port: object.get("usb_port")?.as_str()?.to_string(),
                    object: object.clone(),
                })
            })
            .collect(),
    )
}

/// The `devices` override under `key` in `config`, if any.
fn override_of<'a>(config: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    config.get(OVERRIDES)?.get(key)?.as_object()
}

fn text_in(object: Option<&Map<String, Value>>, key: &str) -> String {
    object
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Set `key` in `object` to `value`, trimmed; a blank value removes it.
fn set_text(object: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        _ = object.remove(key);
    } else {
        _ = object.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// The `devices` map in `config`, created when the file has none.
fn overrides_mut(config: &mut Value) -> Option<&mut Map<String, Value>> {
    let object = config.as_object_mut()?;
    object
        .entry(OVERRIDES)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

/// A row of the Cameras section.
#[derive(Debug, Clone)]
struct Row {
    port: String,
    checked: bool,
    name: String,
    description: String,
}

/// The server page's Cameras section.
struct CamerasSection {
    sdk: &'static str,
    /// The cameras on the bus now, or why the scan failed.
    bus: Result<Vec<BusCamera>, String>,
    registry: Registry,
    order: fn(&str, &str) -> Ordering,
}

impl CamerasSection {
    /// The number the running service serves at `port`, and what it is.
    fn served_on(&self, port: &str) -> Option<(u32, &Served)> {
        self.registry
            .served
            .iter()
            .zip(0_u32..)
            .find(|((_, served), _)| served.port() == port)
            .map(|((_, served), number)| (number, served))
    }

    /// The key the camera the join placed on `port` reads its override
    /// under.
    fn placed_key(&self, port: &str) -> Option<&str> {
        self.registry
            .placed
            .iter()
            .find(|p| p.port == port)
            .map(|p| p.override_key.as_str())
    }

    /// The port of the placed camera whose override key is `key`.
    fn placed_port(&self, key: &str) -> Option<&str> {
        self.registry
            .placed
            .iter()
            .find(|p| p.override_key == key)
            .map(|p| p.port.as_str())
    }

    /// The numbers `config` serves each port at: its list's, or with no
    /// list the port order of the cameras the join placed.
    fn numbers(&self, config: &Value) -> BTreeMap<String, u64> {
        entries(config).map_or_else(
            || {
                self.registry
                    .placed
                    .iter()
                    .zip(0_u64..)
                    .map(|(p, n)| (p.port.clone(), n))
                    .collect()
            },
            |list| list.into_iter().map(|e| (e.port, e.number)).collect(),
        )
    }

    fn label(&self, port: &str) -> Option<&str> {
        self.bus
            .as_ref()
            .ok()?
            .iter()
            .find(|c| c.port == port)
            .map(|c| c.label.as_str())
    }

    /// Every port a row stands for: the bus's, the list's, and the running
    /// service's, in port order.
    fn ports(&self, list: Option<&[Entry]>) -> Vec<String> {
        let mut ports: Vec<String> = self
            .bus
            .as_ref()
            .map(|bus| bus.iter().map(|c| c.port.clone()).collect())
            .unwrap_or_default();
        ports.extend(list.unwrap_or_default().iter().map(|e| e.port.clone()));
        ports.extend(
            self.registry
                .served
                .iter()
                .map(|(_, served)| served.port().to_string()),
        );
        ports.sort_by(|a, b| (self.order)(a, b));
        ports.dedup();
        ports
    }

    /// The rows `config` shows. With a list, a row is checked when an
    /// entry lists its port, with the entry's names; without one, when the
    /// running service serves a camera there, with that camera's override
    /// names, so changing to a list keeps both.
    fn rows(&self, config: &Value) -> Vec<Row> {
        let list = entries(config);
        self.ports(list.as_deref())
            .into_iter()
            .map(|port| {
                list.as_deref().map_or_else(
                    || self.unlisted_row(config, port.clone()),
                    |list| Self::listed_row(list, port.clone()),
                )
            })
            .collect()
    }

    /// `port`'s row with a list: checked when an entry lists it.
    fn listed_row(list: &[Entry], port: String) -> Row {
        let entry = list.iter().find(|e| e.port == port);
        Row {
            checked: entry.is_some(),
            name: entry.map(|e| e.text("name")).unwrap_or_default(),
            description: entry.map(|e| e.text("description")).unwrap_or_default(),
            port,
        }
    }

    /// `port`'s row without a list: checked when the running service serves
    /// a camera there, with that camera's override names.
    fn unlisted_row(&self, config: &Value, port: String) -> Row {
        let names = self
            .placed_key(&port)
            .and_then(|key| override_of(config, key));
        Row {
            checked: matches!(self.served_on(&port), Some((_, Served::Camera { .. }))),
            name: text_in(names, "name"),
            description: text_in(names, "description"),
            port,
        }
    }

    /// The rows a submission carries, in its order.
    fn rows_from(form: &FormValues) -> Vec<Row> {
        (0_usize..)
            .map_while(|i| {
                let port = form.get(&format!("usb_devices.{i}.port"))?;
                Some(Row {
                    port: port.to_string(),
                    checked: form.contains_key(&format!("usb_devices.{i}.serve")),
                    name: form
                        .get(&format!("usb_devices.{i}.name"))
                        .unwrap_or_default()
                        .to_string(),
                    description: form
                        .get(&format!("usb_devices.{i}.description"))
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect()
    }

    /// The list the checked `rows` make, numbered from what `before` serves
    /// them at: the rows with a number keep its order, and the rest follow
    /// in row order. A kept entry's other keys stay as `before` has them.
    fn list_of(&self, rows: &[Row], before: &Value) -> Vec<Value> {
        let numbers = self.numbers(before);
        let previous = entries(before).unwrap_or_default();
        let mut kept: Vec<(u64, &Row)> = rows
            .iter()
            .filter(|r| r.checked)
            .filter_map(|r| numbers.get(&r.port).map(|n| (*n, r)))
            .collect();
        kept.sort_by_key(|(n, _)| *n);
        let added = rows
            .iter()
            .filter(|r| r.checked && !numbers.contains_key(&r.port));
        kept.into_iter()
            .map(|(_, row)| row)
            .chain(added)
            .zip(0_u64..)
            .map(|(row, number)| {
                let mut object = previous
                    .iter()
                    .find(|e| e.port == row.port)
                    .map(|e| e.object.clone())
                    .unwrap_or_default();
                _ = object.insert("device_number".to_string(), Value::from(number));
                _ = object.insert("usb_port".to_string(), Value::String(row.port.clone()));
                set_text(&mut object, "name", &row.name);
                set_text(&mut object, "description", &row.description);
                Value::Object(object)
            })
            .collect()
    }

    /// The rows of the Cameras section, one per port.
    fn rows_table(&self, rows: &[Row]) -> Markup {
        let sdk = self.sdk;
        html! {
        table.rows {
            thead {
                tr {
                    th.only-b { "Serve" }
                    th { "USB port" }
                    th { "On the bus" }
                    th { "Served now" }
                    th.only-b { "Name" }
                    th.only-b { "Description" }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr { td colspan="6" { "No " (sdk) " camera is on the bus, and none is listed." } }
                }
                @for (i, row) in rows.iter().enumerate() {
                    @let serves = self
                        .served_on(&row.port)
                        .map_or_else(|| "not served".to_string(), |(n, s)| s.label(n));
                    tr data-port=(row.port) {
                        td.only-b {
                            input type="hidden" name=(format!("usb_devices.{i}.port")) value=(row.port);
                            input type="checkbox" name=(format!("usb_devices.{i}.serve"))
                                aria-label=(format!("Serve the camera on {}", row.port))
                                checked[row.checked];
                        }
                        td.mono { (row.port) }
                        td { (self.label(&row.port).unwrap_or("nothing on this port now")) }
                        td.mono.serves { (serves) }
                        td.only-b {
                            input type="text" name=(format!("usb_devices.{i}.name"))
                                aria-label="Name" value=(row.name);
                        }
                        td.only-b {
                            input type="text" name=(format!("usb_devices.{i}.description"))
                                aria-label="Description" value=(row.description);
                        }
                    }
                }
            }
        }
        }
    }

    /// Each port served at one number in `before` and another in `after`.
    fn moves(&self, before: &Value, after: &Value) -> Vec<(String, u64, u64)> {
        let to = self.numbers(after);
        let mut moves: Vec<(String, u64, u64)> = self
            .numbers(before)
            .into_iter()
            .filter_map(|(port, from)| {
                let to = *to.get(&port)?;
                (from != to).then_some((port, from, to))
            })
            .collect();
        moves.sort_by(|a, b| (self.order)(&a.0, &b.0));
        moves
    }

    /// Each number `before` and `after` both serve at different ports,
    /// where the new port did not move there from another number: a number
    /// a dropped camera held, given to a camera new to the numbering. Like a
    /// move, it leaves an rp entry bound to the number on another camera.
    fn retaken(
        &self,
        before: &Value,
        after: &Value,
        moves: &[(String, u64, u64)],
    ) -> Vec<(u64, String, String)> {
        let held: BTreeMap<u64, String> = self
            .numbers(before)
            .into_iter()
            .map(|(port, number)| (number, port))
            .collect();
        let mut retaken: Vec<(u64, String, String)> = self
            .numbers(after)
            .into_iter()
            .filter_map(|(port, number)| {
                let old = held.get(&number)?;
                let moved_in = moves.iter().any(|(p, _, to)| *p == port && *to == number);
                (*old != port && !moved_in).then(|| (number, old.clone(), port))
            })
            .collect();
        retaken.sort_by_key(|(number, _, _)| *number);
        retaken
    }

    /// The names `before` holds that `after` keeps nowhere: an entry's,
    /// leaving a list, whose camera has no override key the page knows; an
    /// override's, entering a list, whose camera's row is not listed. Each
    /// is named by its port, else by its override key.
    fn dropped(&self, before: &Value, after: &Value) -> Vec<Dropped> {
        let mut dropped: Vec<Dropped> = match (entries(before), entries(after)) {
            (Some(list), None) => list
                .into_iter()
                .filter(|e| e.has_names() && self.placed_key(&e.port).is_none())
                .map(|e| Dropped::Port(e.port))
                .collect(),
            (None, Some(list)) => before
                .get(OVERRIDES)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, names)| names.as_object().is_some_and(|o| !o.is_empty()))
                .filter_map(|(key, _)| match self.placed_port(key) {
                    Some(port) if list.iter().any(|e| e.port == port) => None,
                    Some(port) => Some(Dropped::Port(port.to_string())),
                    None => Some(Dropped::Key(key.clone())),
                })
                .collect(),
            _ => Vec::new(),
        };
        // In port order, then the keys: a map's own order is serde_json's
        // feature choice, and the confirmation's token must not depend on it.
        dropped.sort_by(|a, b| match (a, b) {
            (Dropped::Port(a), Dropped::Port(b)) => (self.order)(a, b),
            (Dropped::Port(_), Dropped::Key(_)) => Ordering::Less,
            (Dropped::Key(_), Dropped::Port(_)) => Ordering::Greater,
            (Dropped::Key(a), Dropped::Key(b)) => a.cmp(b),
        });
        dropped
    }
}

/// Names a save would drop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dropped {
    /// The names of the camera on, or the entry for, this port.
    Port(String),
    /// The override under this key, whose camera the join placed nowhere.
    Key(String),
}

impl CustomSection for CamerasSection {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let (rows, mode) = match view.submitted {
            Some(form) if form.contains_key(MODE) => (
                Self::rows_from(form),
                form.get(MODE).unwrap_or(AUTO).to_string(),
            ),
            _ => (
                self.rows(view.config),
                if entries(view.config).is_some() {
                    LISTED
                } else {
                    AUTO
                }
                .to_string(),
            ),
        };
        let errors: Vec<&FieldError> = view
            .errors
            .iter()
            .filter(|e| self.shows_error(&e.path))
            .collect();
        let sdk = self.sdk;
        html! {
            fieldset.cameras.switched #cameras {
                legend { "Cameras" }
                @for e in &errors {
                    div.section-error {
                        @if !e.path.is_empty() { code { (e.path) } " " }
                        (e.msg)
                    }
                }
                @if let Err(error) = &self.bus {
                    div.section-error {
                        "The USB scan failed: " (error) ". The rows are the listed ports and what the service serves now."
                    }
                }
                div.switch-choice {
                    div.checkbox {
                        input.switch-a type="radio" id="usb_devices.mode.auto" name=(MODE) value=(AUTO)
                            checked[mode == AUTO];
                        label for="usb_devices.mode.auto" { "Serve every " (sdk) " camera the USB scan places, numbered in port order" }
                    }
                    div.hint { "No list: a camera added or removed moves the numbers after it." }
                }
                div.switch-choice {
                    div.checkbox {
                        input.switch-b type="radio" id="usb_devices.mode.list" name=(MODE) value=(LISTED)
                            checked[mode == LISTED];
                        label for="usb_devices.mode.list" { "Serve only the checked cameras, each at the number it keeps" }
                    }
                    div.hint { "A list: every other " (sdk) " camera is left to other applications, and a listed port with no camera is held by a placeholder." }
                }
                (self.rows_table(&rows))
                p.hint.only-b {
                    "The checked rows keep the order of the numbers they have, from 0; a newly checked row takes the next. "
                    "Unchecking a camera moves the numbers after it down, and a save that moves one asks to confirm first."
                }
                p.hint.only-a {
                    "Changing to a list keeps every number, and each camera's names, from its own page, fill its row."
                }
            }
        }
    }

    fn has_controls(&self) -> bool {
        true
    }

    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
        let rows = Self::rows_from(form);
        let before = config.clone();
        let mode = form.get(MODE).unwrap_or_default();
        if mode != AUTO && mode != LISTED {
            return vec![FieldError {
                path: LIST.to_string(),
                msg: "choose whether every camera is served or only the checked ones".to_string(),
            }];
        }
        let list = (mode == LISTED).then(|| self.list_of(&rows, &before));
        let Some(object) = config.as_object_mut() else {
            return vec![FieldError {
                path: String::new(),
                msg: "the configuration file is not a JSON object".to_string(),
            }];
        };
        if let Some(list) = list {
            // A list refuses a `devices` map beside it; the rows carry the
            // names it held.
            _ = object.insert(LIST.to_string(), Value::Array(list));
            _ = object.insert(OVERRIDES.to_string(), Value::Object(Map::new()));
            return Vec::new();
        }
        _ = object.remove(LIST);
        if entries(&before).is_some() {
            // Leaving a list: each listed camera's names become its
            // override.
            let Some(overrides) = overrides_mut(config) else {
                return Vec::new();
            };
            for row in &rows {
                let Some(key) = self.placed_key(&row.port) else {
                    continue;
                };
                let mut names = overrides
                    .get(key)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                set_text(&mut names, "name", &row.name);
                set_text(&mut names, "description", &row.description);
                if names.is_empty() {
                    _ = overrides.remove(key);
                } else {
                    _ = overrides.insert(key.to_string(), Value::Object(names));
                }
            }
        }
        Vec::new()
    }

    fn shows_error(&self, path: &str) -> bool {
        [LIST, OVERRIDES].iter().any(|key| {
            path == *key
                || path
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    fn confirmation(&self, before: &Value, after: &Value) -> Option<Confirmation> {
        let moves = self.moves(before, after);
        let retaken = self.retaken(before, after, &moves);
        let dropped = self.dropped(before, after);
        if moves.is_empty() && retaken.is_empty() && dropped.is_empty() {
            return None;
        }
        let mut tokens: Vec<String> = moves
            .iter()
            .map(|(port, from, to)| format!("move {port} {from}>{to}"))
            .collect();
        tokens.extend(
            retaken
                .iter()
                .map(|(number, old, new)| format!("number {number} {old}>{new}")),
        );
        tokens.extend(dropped.iter().map(|d| match d {
            Dropped::Port(port) => format!("drop {port}"),
            Dropped::Key(key) => format!("drop key {key}"),
        }));
        let markup = html! {
            ul {
                @for (port, from, to) in &moves {
                    li.move data-port=(port) data-from=(from) data-to=(to) {
                        "move the camera on " code { (port) } " from number " (from) " to " (to)
                    }
                }
                @for (number, old, new) in &retaken {
                    li.retaken data-number=(number) data-from=(old) data-to=(new) {
                        "give number " (number) ", which the camera on " code { (old) }
                        " held, to the camera on " code { (new) }
                    }
                }
                @for d in &dropped {
                    @match d {
                        Dropped::Port(port) => li.dropped data-port=(port) {
                            "drop the name and description of " code { (port) } ", which has no camera whose override the page could write"
                        },
                        Dropped::Key(key) => li.dropped data-key=(key) {
                            "drop the " code { "devices" } " override " code { (key) } ", whose camera is on no port this start placed"
                        },
                    }
                }
            }
            @if !moves.is_empty() || !retaken.is_empty() {
                p {
                    "rp's camera entries bound to the numbers that move must be renumbered to match: an entry without a "
                    code { "unique_id" } " pin would otherwise open the wrong camera, with no error. rp's own page shows which entries those are."
                }
            }
        };
        Some(Confirmation {
            token: tokens.join("; "),
            markup,
        })
    }
}

/// The facts a camera's page shows.
fn facts(rows: &[(&str, &str)]) -> Markup {
    html! {
        dl.facts {
            @for (term, value) in rows {
                dt { (term) }
                dd { (value) }
            }
        }
    }
}

/// A listed camera's page: what the driver knows, and where its names are
/// edited.
struct ListedPage {
    number: u32,
    port: String,
    model: String,
    unique_id: String,
}

impl CustomSection for ListedPage {
    fn render(&self, _: SectionView<'_>) -> Markup {
        html! {
            (facts(&[("Model", &self.model), ("USB port", &self.port), ("UniqueID", &self.unique_id)]))
            p {
                "Listed at number " (self.number) ". Its name and description are edited in its row of the server page's "
                a href=(CAMERAS_LINK) { "Cameras section" } "."
            }
        }
    }

    fn has_controls(&self) -> bool {
        false
    }

    fn apply(&self, _: &FormValues, _: &mut Value) -> Vec<FieldError> {
        Vec::new()
    }
}

/// A placeholder's page: the port it stands in for, and why.
struct PlaceholderPage {
    port: String,
    reason: String,
}

impl CustomSection for PlaceholderPage {
    fn render(&self, _: SectionView<'_>) -> Markup {
        html! {
            (facts(&[("Stands in for USB port", &self.port), ("Why no camera is served", &self.reason)]))
            p {
                "A placeholder holds a listed number whose camera cannot be served, so no other number moves. "
                "The list is edited in the server page's "
                a href=(CAMERAS_LINK) { "Cameras section" } "."
            }
        }
    }

    fn has_controls(&self) -> bool {
        false
    }

    fn apply(&self, _: &FormValues, _: &mut Value) -> Vec<FieldError> {
        Vec::new()
    }
}

/// The page of a camera served without a list: its `devices` override's
/// names.
struct OverridePage {
    port: String,
    model: String,
    unique_id: String,
    /// The key its override is read under.
    key: String,
}

impl CustomSection for OverridePage {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let stored = override_of(view.config, &self.key);
        let value = |control: &str, key: &str| {
            view.submitted
                .and_then(|form| form.get(control))
                .map_or_else(|| text_in(stored, key), str::to_string)
        };
        html! {
            (facts(&[("Model", &self.model), ("USB port", &self.port), ("UniqueID", &self.unique_id)]))
            fieldset {
                legend { "Override" }
                div.field {
                    label for=(OVERRIDE_NAME) { "Name" }
                    input type="text" id=(OVERRIDE_NAME) name=(OVERRIDE_NAME) value=(value(OVERRIDE_NAME, "name"));
                    div.hint { "Blank keeps the name the camera gives itself." }
                }
                div.field {
                    label for=(OVERRIDE_DESCRIPTION) { "Description" }
                    input type="text" id=(OVERRIDE_DESCRIPTION) name=(OVERRIDE_DESCRIPTION)
                        value=(value(OVERRIDE_DESCRIPTION, "description"));
                }
                div.hint {
                    "Saved under " code { (self.key) } " in the " code { "devices" } " map. Served without a list, "
                    "numbered in port order; to pin its number, list it in the server page's "
                    a href=(CAMERAS_LINK) { "Cameras section" } "."
                }
            }
        }
    }

    fn has_controls(&self) -> bool {
        true
    }

    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
        let Some(overrides) = overrides_mut(config) else {
            return vec![FieldError {
                path: String::new(),
                msg: "the configuration file is not a JSON object".to_string(),
            }];
        };
        let mut names = overrides
            .get(&self.key)
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
            _ = overrides.remove(&self.key);
        } else {
            _ = overrides.insert(self.key.clone(), Value::Object(names));
        }
        Vec::new()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn bus(port: &str, label: &str) -> BusCamera {
        BusCamera {
            port: port.to_string(),
            label: label.to_string(),
        }
    }

    fn camera(port: &str, key: &str, listed: bool) -> Served {
        Served::Camera {
            port: port.to_string(),
            model: "SV605CC".to_string(),
            unique_id: format!("SVBONY:SV605CC:{key}"),
            override_key: key.to_string(),
            listed,
        }
    }

    fn placeholder(port: &str) -> Served {
        Served::Placeholder {
            port: port.to_string(),
            reason: format!("no working camera is enumerated on {port}"),
        }
    }

    fn placed(port: &str, key: &str) -> PlacedCamera {
        PlacedCamera {
            port: port.to_string(),
            override_key: key.to_string(),
        }
    }

    /// The cameras on `a:1` (key `K1`) and `a:3` (key `K3`), the join's;
    /// `a:2` holds a record no SDK camera is placed on.
    fn section(served: Vec<(&str, Served)>) -> CamerasSection {
        CamerasSection {
            sdk: "SVBony",
            bus: Ok(vec![
                bus("a:1", "SVBONY SV605CC"),
                bus("a:2", "f266:9b00"),
                bus("a:3", "SVBONY SV605CC"),
            ]),
            registry: Registry {
                served: served
                    .into_iter()
                    .map(|(name, s)| (name.to_string(), s))
                    .collect(),
                placed: vec![placed("a:1", "K1"), placed("a:3", "K3")],
            },
            order: str::cmp,
        }
    }

    fn form(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    /// The form the rows `ports` submit, `checked` among them, in `mode`.
    fn rows_form(mode: &str, rows: &[(&str, bool, &str)]) -> FormValues {
        let mut pairs = vec![(MODE.to_string(), mode.to_string())];
        for (i, (port, checked, name)) in rows.iter().enumerate() {
            pairs.push((format!("usb_devices.{i}.port"), (*port).to_string()));
            if *checked {
                pairs.push((format!("usb_devices.{i}.serve"), "on".to_string()));
            }
            pairs.push((format!("usb_devices.{i}.name"), (*name).to_string()));
            pairs.push((format!("usb_devices.{i}.description"), String::new()));
        }
        FormValues::from(pairs)
    }

    fn view(config: &Value) -> SectionView<'_> {
        SectionView {
            config,
            submitted: None,
            errors: &[],
        }
    }

    #[test]
    fn without_a_list_the_rows_check_what_is_served_with_its_override_names() {
        let s = section(vec![
            ("Main", camera("a:1", "K1", false)),
            ("SVBony 2", camera("a:3", "K3", false)),
        ]);
        let config = json!({ "devices": { "K1": { "name": "Main" } } });
        let html = s.render(view(&config)).into_string();
        assert!(html.contains(r#"value="auto" checked"#), "{html}");
        let rows = s.rows(&config);
        assert_eq!(
            rows.iter()
                .map(|r| (r.port.as_str(), r.checked, r.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("a:1", true, "Main"), ("a:2", false, ""), ("a:3", true, "")]
        );
        assert!(html.contains("camera 0"), "{html}");
        assert!(html.contains("not served"), "{html}");
        assert!(html.contains("f266:9b00"), "{html}");
    }

    #[test]
    fn with_a_list_the_rows_are_its_entries_and_a_listed_port_without_a_record() {
        let s = section(vec![
            ("Main", camera("a:1", "K1", true)),
            ("Gone", placeholder("a:9")),
        ]);
        let config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "name": "Main" },
            { "device_number": 1, "usb_port": "a:9" }
        ]});
        let rows = s.rows(&config);
        assert_eq!(
            rows.iter()
                .map(|r| (r.port.as_str(), r.checked))
                .collect::<Vec<_>>(),
            vec![("a:1", true), ("a:2", false), ("a:3", false), ("a:9", true)]
        );
        let html = s.render(view(&config)).into_string();
        assert!(html.contains(r#"value="list" checked"#), "{html}");
        assert!(html.contains("placeholder 1"), "{html}");
        assert!(html.contains("nothing on this port now"), "{html}");
    }

    #[test]
    fn changing_to_a_list_keeps_the_numbers_and_carries_the_names() {
        let s = section(vec![
            ("Main", camera("a:1", "K1", false)),
            ("SVBony 2", camera("a:3", "K3", false)),
        ]);
        let mut config =
            json!({ "devices": { "K1": { "name": "Main" } }, "server": { "port": 1 } });
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[("a:1", true, "Main"), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config,
            json!({
                "usb_devices": [
                    { "device_number": 0, "usb_port": "a:1", "name": "Main" },
                    { "device_number": 1, "usb_port": "a:3" }
                ],
                "devices": {},
                "server": { "port": 1 }
            })
        );
        // Nothing moved and every name is carried: nothing to confirm.
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn a_newly_checked_row_takes_the_next_number() {
        let s = section(vec![("Main", camera("a:3", "K3", true))]);
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:3" } ] });
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[("a:1", true, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:3" },
                { "device_number": 1, "usb_port": "a:1" }
            ])
        );
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn unchecking_a_lower_number_moves_the_ones_after_it_and_asks_to_confirm() {
        let s = section(vec![
            ("Hold", placeholder("a:2")),
            ("Main", camera("a:1", "K1", true)),
        ]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:2" },
            { "device_number": 1, "usb_port": "a:1", "name": "Main" }
        ]});
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[
                ("a:1", true, "Main"),
                ("a:2", false, ""),
                ("a:3", false, ""),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([{ "device_number": 0, "usb_port": "a:1", "name": "Main" }])
        );
        let confirmation = s.confirmation(&before, &config).expect("a move to confirm");
        assert_eq!(confirmation.token, "move a:1 1>0");
        let html = confirmation.markup.into_string();
        assert!(
            html.contains(r#"data-port="a:1" data-from="1" data-to="0""#),
            "{html}"
        );
        assert!(html.contains("must be renumbered"), "{html}");
    }

    #[test]
    fn the_checked_rows_keep_the_order_of_their_numbers_not_of_their_ports() {
        let s = section(vec![
            ("Three", camera("a:3", "K3", true)),
            ("One", camera("a:1", "K1", true)),
        ]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:3" },
            { "device_number": 1, "usb_port": "a:1" }
        ]});
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[("a:1", true, ""), ("a:2", true, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:3" },
                { "device_number": 1, "usb_port": "a:1" },
                { "device_number": 2, "usb_port": "a:2" }
            ])
        );
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn a_dropped_cameras_number_given_to_another_asks_to_confirm() {
        let s = section(vec![("Main", camera("a:1", "K1", true))]);
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:1" } ] });
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[("a:1", false, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([{ "device_number": 0, "usb_port": "a:3" }])
        );
        let confirmation = s
            .confirmation(&before, &config)
            .expect("a number to confirm");
        assert_eq!(confirmation.token, "number 0 a:1>a:3");
        let html = confirmation.markup.into_string();
        assert!(
            html.contains(r#"data-number="0" data-from="a:1" data-to="a:3""#),
            "{html}"
        );
        assert!(html.contains("must be renumbered"), "{html}");
    }

    #[test]
    fn a_number_a_moved_camera_takes_is_named_once_as_its_move() {
        let s = section(vec![
            ("Hold", placeholder("a:2")),
            ("Main", camera("a:1", "K1", true)),
        ]);
        let before = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:2" },
            { "device_number": 1, "usb_port": "a:1" }
        ]});
        let after = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:1" } ] });
        assert_eq!(
            s.confirmation(&before, &after).expect("a move").token,
            "move a:1 1>0"
        );
    }

    #[test]
    fn unchecking_every_row_saves_an_empty_list() {
        let s = section(vec![("Main", camera("a:1", "K1", true))]);
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:1" } ] });
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[("a:1", false, ""), ("a:2", false, ""), ("a:3", false, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config["usb_devices"], json!([]));
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn a_kept_entry_keeps_the_keys_the_section_does_not_edit() {
        let s = section(vec![("Main", camera("a:1", "K1", true))]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0, "name": "Old" }
        ]});
        let submitted = rows_form(LISTED, &[("a:1", true, "  ")]);
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([{ "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0 }])
        );
    }

    #[test]
    fn changing_to_no_list_carries_each_cameras_names_and_names_what_it_drops() {
        let s = section(vec![
            ("Main", camera("a:1", "K1", true)),
            ("Old", placeholder("a:9")),
        ]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "name": "Main" },
            { "device_number": 1, "usb_port": "a:9", "name": "Old" }
        ], "devices": {} });
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[
                ("a:1", true, "Main"),
                ("a:2", false, ""),
                ("a:3", false, ""),
                ("a:9", true, "Old"),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config, json!({ "devices": { "K1": { "name": "Main" } } }));
        let confirmation = s.confirmation(&before, &config).expect("dropped names");
        // Without the list, the unlisted camera on a:3 is served at the
        // number the placeholder held.
        assert_eq!(confirmation.token, "number 1 a:9>a:3; drop a:9");
        assert!(confirmation
            .markup
            .into_string()
            .contains(r#"li class="dropped" data-port="a:9""#));
    }

    #[test]
    fn changing_to_a_list_names_an_override_whose_camera_is_not_listed_or_not_placed() {
        let s = section(vec![
            ("Main", camera("a:1", "K1", false)),
            ("Two", camera("a:3", "K3", false)),
        ]);
        let mut config = json!({ "devices": {
            "K1": { "name": "Main" },
            "K3": { "name": "Three" },
            "GONE": { "name": "Away" }
        }});
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[
                ("a:1", true, "Main"),
                ("a:2", false, ""),
                ("a:3", false, "Three"),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s.confirmation(&before, &config).expect("dropped names");
        assert_eq!(confirmation.token, "drop a:3; drop key GONE");
    }

    #[test]
    fn staying_without_a_list_leaves_the_overrides_to_the_camera_pages() {
        let s = section(vec![("Main", camera("a:1", "K1", false))]);
        let mut config = json!({ "devices": { "K1": { "name": "Main" } } });
        let submitted = rows_form(AUTO, &[("a:1", true, "Typed but hidden")]);
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config, json!({ "devices": { "K1": { "name": "Main" } } }));
    }

    #[test]
    fn a_submission_without_a_mode_is_refused_and_shown_by_the_section() {
        let s = section(Vec::new());
        let mut config = json!({});
        let errors = s.apply(&form(&[]), &mut config);
        assert_eq!(errors.len(), 1);
        assert!(s.shows_error(&errors[0].path));
        assert!(s.shows_error("usb_devices.1.usb_port"));
        assert!(s.shows_error("devices.K1"));
        assert!(!s.shows_error("devicesx"));
        assert!(!s.shows_error("server.port"));
    }

    #[test]
    fn a_re_render_shows_the_submission() {
        let s = section(vec![("Main", camera("a:1", "K1", true))]);
        let config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:1" } ] });
        let submitted = rows_form(LISTED, &[("a:2", true, "Typed")]);
        let html = s
            .render(SectionView {
                config: &config,
                submitted: Some(&submitted),
                errors: &[],
            })
            .into_string();
        assert!(html.contains(r#"value="Typed""#), "{html}");
        assert!(!html.contains(r#"data-port="a:1""#), "{html}");
    }

    #[test]
    fn a_failed_scan_is_said_and_the_rows_are_the_list_and_the_served() {
        let mut s = section(vec![("Main", camera("a:1", "K1", true))]);
        s.bus = Err("sysfs unreadable".to_string());
        let config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1" },
            { "device_number": 1, "usb_port": "a:7" }
        ]});
        let html = s.render(view(&config)).into_string();
        assert!(
            html.contains("The USB scan failed: sysfs unreadable"),
            "{html}"
        );
        assert_eq!(
            s.rows(&config)
                .iter()
                .map(|r| r.port.as_str())
                .collect::<Vec<_>>(),
            vec!["a:1", "a:7"]
        );
    }

    #[test]
    fn a_camera_served_without_a_list_creates_updates_and_deletes_its_override() {
        let page = OverridePage {
            port: "a:1".to_string(),
            model: "SV605CC".to_string(),
            unique_id: "SVBONY:SV605CC:K1".to_string(),
            key: "K1".to_string(),
        };
        let mut config = json!({ "server": {} });
        assert_eq!(
            page.apply(
                &form(&[(OVERRIDE_NAME, "Main"), (OVERRIDE_DESCRIPTION, "")]),
                &mut config
            ),
            Vec::<FieldError>::new()
        );
        assert_eq!(config["devices"], json!({ "K1": { "name": "Main" } }));
        assert_eq!(
            page.apply(
                &form(&[(OVERRIDE_NAME, "Main"), (OVERRIDE_DESCRIPTION, "Imaging")]),
                &mut config
            ),
            Vec::<FieldError>::new()
        );
        assert_eq!(
            config["devices"],
            json!({ "K1": { "name": "Main", "description": "Imaging" } })
        );
        let html = page.render(view(&config)).into_string();
        assert!(
            html.contains(r#"name="override.name" value="Main""#),
            "{html}"
        );
        assert_eq!(
            page.apply(
                &form(&[(OVERRIDE_NAME, " "), (OVERRIDE_DESCRIPTION, "")]),
                &mut config
            ),
            Vec::<FieldError>::new()
        );
        assert_eq!(config["devices"], json!({}));
        assert!(page.has_controls());
    }

    #[test]
    fn a_listed_camera_and_a_placeholder_only_show() {
        let listed = ListedPage {
            number: 2,
            port: "a:1".to_string(),
            model: "SV605CC".to_string(),
            unique_id: "SVBONY:SV605CC:K1".to_string(),
        };
        let held = PlaceholderPage {
            port: "a:9".to_string(),
            reason: "no working camera".to_string(),
        };
        let config = json!({});
        let pages: [(&dyn CustomSection, &str); 2] = [
            (&listed, "Listed at number 2"),
            (&held, "no working camera"),
        ];
        for (page, says) in pages {
            assert!(!page.has_controls());
            let html = page.render(view(&config)).into_string();
            assert!(html.contains(says), "{html}");
            assert!(html.contains(CAMERAS_LINK), "{html}");
            let mut untouched = json!({ "a": 1 });
            assert_eq!(
                page.apply(&form(&[]), &mut untouched),
                Vec::<FieldError>::new()
            );
            assert_eq!(untouched, json!({ "a": 1 }));
        }
    }

    fn pages() -> ClaimsPages {
        let pages = ClaimsPages::new(
            "SVBony",
            Arc::new(|| Ok(vec![bus("a:1", "SVBONY SV605CC")])),
            str::cmp,
        );
        assert_eq!(pages.register("Main", camera("a:1", "K1", false)), 0);
        assert_eq!(pages.register("Held", placeholder("a:9")), 1);
        pages.placed(vec![placed("a:1", "K1")]);
        pages
    }

    #[test]
    fn the_devices_are_what_the_build_registered_with_no_section_of_their_own() {
        let p = pages();
        let devices = p.devices().unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|d| (d.device_number, d.name.as_str(), d.pointer.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "Main", ""), (1, "Held", "")]
        );
        assert_eq!(p.owned_keys(), vec!["usb_devices", "devices"]);
    }

    #[tokio::test]
    async fn each_page_gets_its_section() {
        let p = pages();
        let server = p.sections(PageScope::Server).await;
        assert_eq!(server.len(), 1);
        assert!(server[0].has_controls());
        let html = server[0].render(view(&json!({}))).into_string();
        assert!(html.contains(r#"data-port="a:1""#), "{html}");

        let unlisted = p
            .sections(PageScope::Device {
                device_type: "camera",
                device_number: 0,
            })
            .await;
        assert!(unlisted[0].has_controls());
        let held = p
            .sections(PageScope::Device {
                device_type: "camera",
                device_number: 1,
            })
            .await;
        assert!(!held[0].has_controls());
        assert_eq!(
            p.sections(PageScope::Device {
                device_type: "camera",
                device_number: 2
            })
            .await
            .len(),
            0
        );
        assert_eq!(
            p.sections(PageScope::Device {
                device_type: "filterwheel",
                device_number: 0
            })
            .await
            .len(),
            0
        );
    }

    #[tokio::test]
    async fn a_scan_that_panics_is_a_failed_scan() {
        let p = ClaimsPages::new("SVBony", Arc::new(|| panic!("collector")), str::cmp);
        let server = p.sections(PageScope::Server).await;
        let html = server[0].render(view(&json!({}))).into_string();
        assert!(
            html.contains("The USB scan failed: the USB scan did not finish"),
            "{html}"
        );
    }
}

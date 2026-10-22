//! [`ClaimsPages`]: a camera driver's own part of its setup pages — the
//! Cameras section that edits `usb_devices` on the server page, and each
//! camera's page (the driver-setup-pages plan's D11, the device-claims
//! plan's C8; `docs/services/svbony-camera.md`, "Setup pages"). A driver
//! whose entries declare a camera's filter wheel (`qhy-camera`) also gets
//! a wheel column in the section and a page per wheel, which edits its
//! filter names.
//!
//! The driver records what its build registered — each number's camera or
//! placeholder, each wheel's, and every camera the join placed — and hands
//! a scan the pages run on each request for the server page, so its rows
//! are the bus as it is now. The scan is the passive one a start takes: no
//! SDK call, nothing opened.

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

/// The filter wheel device type in its Alpaca path spelling.
const WHEEL_TYPE: &str = "filterwheel";

/// The configuration keys the Cameras section edits.
const LIST: &str = "usb_devices";
const OVERRIDES: &str = "devices";

/// An entry's declared wheel, and the wheel's filter names, in an entry or
/// an override.
const WHEEL_NUMBER: &str = "filter_wheel_number";
const FILTER_NAMES: &str = "filter_names";

/// A wheel page's filter names, one per line.
const WHEEL_NAMES: &str = "wheel.filter_names";

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

/// What a filter wheel number serves, as the last start or reload
/// registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedWheel {
    /// The wheel on the camera the join placed on `port`.
    Wheel {
        port: String,
        unique_id: String,
        /// The key its camera's `devices` override is read under, which
        /// holds the wheel's filter names without a list.
        override_key: String,
    },
    /// A placeholder holding a declared wheel's number, for its camera's
    /// reason.
    Placeholder { port: String, reason: String },
}

impl ServedWheel {
    fn port(&self) -> &str {
        match self {
            Self::Wheel { port, .. } | Self::Placeholder { port, .. } => port,
        }
    }
}

/// A camera the join placed on a port, listed or not: what the no-list
/// default serves, numbered in port order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedCamera {
    pub port: String,
    /// The key its `devices` override is read under, `None` when the driver
    /// reads it only by opening the camera and the start did not open this
    /// one.
    pub override_key: Option<String>,
    /// Whether the no-list default serves it only if the reload's open of it
    /// succeeds, which the page cannot tell: the driver opens a camera to
    /// serve it, and the start did not serve this one — unlisted, or its
    /// open failed.
    pub served_if_opens: bool,
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
    /// Each filter wheel number's, likewise.
    wheels: Vec<(String, ServedWheel)>,
    placed: Vec<PlacedCamera>,
    /// Every port the build's scan saw a record on that can be one of the
    /// driver's cameras, placed or not; `None` when the build took no scan
    /// (an empty list) or the scan failed.
    seen: Option<Vec<String>>,
}

/// A camera driver's own part of its setup pages, built once per start or
/// reload.
pub struct ClaimsPages {
    /// The SDK's display name (`SVBony`).
    sdk: &'static str,
    scan: ScanCameras,
    /// Port order (`rusty-photon-doctor-checks`' `compare_ports`).
    order: fn(&str, &str) -> Ordering,
    /// Whether an entry declares its camera's filter wheel.
    wheels: bool,
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
            wheels: false,
            registry: Mutex::new(Registry::default()),
        }
    }

    /// The pages of a driver whose `usb_devices` entries declare the
    /// camera's filter wheel (`filter_wheel_number`, with its
    /// `filter_names`): the Cameras section numbers the wheels as it does
    /// the cameras, and each wheel gets a page that edits its filter names.
    #[must_use]
    pub const fn with_wheels(mut self) -> Self {
        self.wheels = true;
        self
    }

    /// Record the next device number the build registers — its name and
    /// what it serves — and answer the number, which its page is served at.
    pub fn register(&self, name: impl Into<String>, served: Served) -> u32 {
        let mut registry = self.lock();
        registry.served.push((name.into(), served));
        u32::try_from(registry.served.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    /// Record the next filter wheel number the build registers, and answer
    /// it, as [`Self::register`] does a camera's.
    pub fn register_wheel(&self, name: impl Into<String>, served: ServedWheel) -> u32 {
        let mut registry = self.lock();
        registry.wheels.push((name.into(), served));
        u32::try_from(registry.wheels.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    /// Record every camera the join placed, listed or not, in port order,
    /// and every port the scan saw a record on that can be a camera, placed
    /// or not: what the server page compares the bus with, to tell what the
    /// reload a save fires would number afresh.
    pub fn placed(&self, placed: Vec<PlacedCamera>, seen: Vec<String>) {
        let mut registry = self.lock();
        registry.placed = placed;
        registry.seen = Some(seen);
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
        let registry = self.lock();
        let section = |device_type: &str, name: &String, number| DeviceSection {
            device_type: device_type.to_string(),
            device_number: number,
            pointer: String::new(),
            name: name.clone(),
            enabled: true,
        };
        Some(
            registry
                .served
                .iter()
                .zip(0_u32..)
                .map(|((name, _), number)| section(DEVICE_TYPE, name, number))
                .chain(
                    registry
                        .wheels
                        .iter()
                        .zip(0_u32..)
                        .map(|((name, _), number)| section(WHEEL_TYPE, name, number)),
                )
                .collect(),
        )
    }

    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        Box::pin(async move {
            let registry = self.lock().clone();
            match scope {
                PageScope::Server => {
                    let section: Box<dyn CustomSection> =
                        Box::new(self.cameras_section(registry).await);
                    vec![section]
                }
                PageScope::Device {
                    device_type,
                    device_number,
                } => {
                    // An unlisted camera's page saves its override, and a
                    // wheel's its filter names; the reload either fires
                    // numbers the bus as it is now without a list, so a save
                    // needs the scan to tell what that would move. Showing a
                    // page needs none, so a `GET` takes none.
                    let index = usize::try_from(device_number).ok();
                    let edits = match device_type {
                        DEVICE_TYPE => index
                            .and_then(|index| registry.served.get(index))
                            .is_some_and(|(_, served)| {
                                matches!(served, Served::Camera { listed: false, .. })
                            }),
                        WHEEL_TYPE => index.is_some_and(|i| i < registry.wheels.len()),
                        _ => false,
                    };
                    let numbering = if saving && edits {
                        Some(self.cameras_section(registry.clone()).await)
                    } else {
                        None
                    };
                    device_page(&registry, device_type, device_number, numbering)
                        .into_iter()
                        .collect()
                }
            }
        })
    }
}

impl ClaimsPages {
    /// The Cameras section over `registry` and the bus as a scan finds it
    /// now. A scan that panics has failed to scan, like one that errors.
    async fn cameras_section(&self, registry: Registry) -> CamerasSection {
        let scan = Arc::clone(&self.scan);
        let bus = tokio::task::spawn_blocking(move || scan())
            .await
            .unwrap_or_else(|error| Err(format!("the USB scan did not finish: {error}")));
        if let Err(error) = &bus {
            debug!(%error, "the setup pages' USB scan failed");
        }
        CamerasSection {
            sdk: self.sdk,
            bus,
            registry,
            order: self.order,
            wheels: self.wheels,
        }
    }
}

/// The section a camera's or a wheel's page draws, by what its number
/// serves; an unlisted camera's and a wheel's check their saves against
/// `numbering`.
fn device_page(
    registry: &Registry,
    device_type: &str,
    device_number: u32,
    numbering: Option<CamerasSection>,
) -> Option<Box<dyn CustomSection>> {
    let index = usize::try_from(device_number).ok()?;
    if device_type == WHEEL_TYPE {
        let (_, served) = registry.wheels.get(index)?;
        let page: Box<dyn CustomSection> = Box::new(WheelPage {
            served: served.clone(),
            numbering,
        });
        return Some(page);
    }
    if device_type != DEVICE_TYPE {
        return None;
    }
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
            numbering,
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
    /// The filter wheel number it declares, if any.
    wheel: Option<u64>,
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
        !self.text("name").is_empty()
            || !self.text("description").is_empty()
            || has_filter_names(Some(&self.object))
    }
}

/// Whether `object` — an entry or an override — names any filter.
fn has_filter_names(object: Option<&Map<String, Value>>) -> bool {
    object
        .and_then(|o| o.get(FILTER_NAMES))
        .and_then(Value::as_array)
        .is_some_and(|names| !names.is_empty())
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
                    wheel: object.get(WHEEL_NUMBER).and_then(Value::as_u64),
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

/// A port whose camera a save moves: the port, the number it has, the
/// number it takes.
type Move = (String, u64, u64);

/// A number a save gives another camera: the number, the port that held
/// it, the port that takes it.
type Retake = (u64, String, String);

/// A row of the Cameras section.
#[derive(Debug, Clone)]
struct Row {
    port: String,
    checked: bool,
    name: String,
    description: String,
    /// Whether its entry declares the camera's filter wheel.
    wheel: bool,
}

/// The server page's Cameras section.
struct CamerasSection {
    sdk: &'static str,
    /// The cameras on the bus now, or why the scan failed.
    bus: Result<Vec<BusCamera>, String>,
    registry: Registry,
    order: fn(&str, &str) -> Ordering,
    /// Whether an entry declares its camera's filter wheel.
    wheels: bool,
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

    /// The filter wheel number the running service serves on `port`'s
    /// camera, and what it is.
    fn wheel_on(&self, port: &str) -> Option<(u32, &ServedWheel)> {
        self.registry
            .wheels
            .iter()
            .zip(0_u32..)
            .find(|((_, wheel), _)| wheel.port() == port)
            .map(|((_, wheel), number)| (number, wheel))
    }

    /// Whether the running service serves a wheel, not its placeholder, on
    /// `port`'s camera.
    fn serves_wheel_on(&self, port: &str) -> bool {
        matches!(self.wheel_on(port), Some((_, ServedWheel::Wheel { .. })))
    }

    /// The key the camera the join placed on `port` reads its override
    /// under, when the driver knows it.
    fn placed_key(&self, port: &str) -> Option<&str> {
        self.registry
            .placed
            .iter()
            .find(|p| p.port == port)
            .and_then(|p| p.override_key.as_deref())
    }

    /// The port of the placed camera whose override key is `key`.
    fn placed_port(&self, key: &str) -> Option<&str> {
        self.registry
            .placed
            .iter()
            .find(|p| p.override_key.as_deref() == Some(key))
            .map(|p| p.port.as_str())
    }

    /// The numbers the running service serves each port at under `config`:
    /// its list's, or with no list the port order of the cameras the last
    /// start placed and served — not one it must open and did not.
    fn numbers(&self, config: &Value) -> BTreeMap<String, u64> {
        entries(config).map_or_else(
            || {
                Self::in_order(
                    self.registry
                        .placed
                        .iter()
                        .filter(|p| !p.served_if_opens)
                        .map(|p| p.port.clone())
                        .collect(),
                )
            },
            |list| list.into_iter().map(|e| (e.port, e.number)).collect(),
        )
    }

    /// The filter wheel numbers the running service serves on each camera
    /// port under `config`: the wheels its list declares, or with no list
    /// the ones the last start served. Empty for a driver without wheels.
    fn wheel_numbers(&self, config: &Value) -> BTreeMap<String, u64> {
        if !self.wheels {
            return BTreeMap::new();
        }
        entries(config).map_or_else(
            || {
                self.registry
                    .wheels
                    .iter()
                    .zip(0_u64..)
                    .filter_map(|((_, wheel), number)| match wheel {
                        ServedWheel::Wheel { port, .. } => Some((port.clone(), number)),
                        ServedWheel::Placeholder { .. } => None,
                    })
                    .collect()
            },
            |list| {
                list.into_iter()
                    .filter_map(|e| Some((e.port, e.wheel?)))
                    .collect()
            },
        )
    }

    /// The filter wheel numbers the reload a save of `config` fires would
    /// serve: its list's declared wheels, or with no list a wheel on each
    /// camera the no-list forecast serves that has one now, in port order —
    /// the page opens nothing, so it takes no other camera for one with a
    /// wheel ([`Self::wheels_by_probe`]).
    fn wheel_numbers_after(&self, config: &Value) -> BTreeMap<String, u64> {
        if !self.wheels {
            return BTreeMap::new();
        }
        if entries(config).is_some() {
            return self.wheel_numbers(config);
        }
        Self::in_order(
            self.predicted(false)
                .into_iter()
                .filter(|port| self.serves_wheel_on(port))
                .collect(),
        )
    }

    /// Whether a save from `before` to `after` leaves a list that declares
    /// a wheel: without a list the reload serves the wheels its CFW probe
    /// finds, which the page, opening nothing, cannot tell — a declared one
    /// it misses, or one on a camera that declared none, moves the wheel
    /// numbers after it.
    fn wheels_by_probe(&self, before: &Value, after: &Value) -> bool {
        entries(after).is_none()
            && entries(before).is_some()
            && !self.wheel_numbers(before).is_empty()
    }

    /// The numbers the reload a save of `config` fires would serve each port
    /// at: its list's, or with no list the port order the no-list default
    /// gives the bus as it is now (`predicted`), `all_open` saying whether
    /// every placed camera the start did not open opens this time, or none.
    fn numbers_after(&self, config: &Value, all_open: bool) -> BTreeMap<String, u64> {
        entries(config).map_or_else(
            || Self::in_order(self.predicted(all_open)),
            |list| list.into_iter().map(|e| (e.port, e.number)).collect(),
        )
    }

    /// The ports of the cameras the start placed and did not open, which
    /// the driver must open to serve.
    fn unopened(&self) -> Vec<&str> {
        self.registry
            .placed
            .iter()
            .filter(|p| p.served_if_opens)
            .map(|p| p.port.as_str())
            .collect()
    }

    /// `ports`, already in port order, numbered from 0.
    fn in_order(ports: Vec<String>) -> BTreeMap<String, u64> {
        ports.into_iter().zip(0_u64..).collect()
    }

    /// The ports the no-list default would serve a camera on after a reload,
    /// in port order: each camera the last start placed whose port the bus
    /// still shows — one it did not open only when `all_open` — and each
    /// port that has gained a record since that start, which the page,
    /// making no SDK call, takes for a camera. A record the start saw and did
    /// not place stays unplaced; a failed scan predicts what the start
    /// placed.
    fn predicted(&self, all_open: bool) -> Vec<String> {
        let placed = self
            .registry
            .placed
            .iter()
            .filter(|p| all_open || !p.served_if_opens)
            .map(|p| p.port.clone());
        let Ok(bus) = &self.bus else {
            return placed.collect();
        };
        let on_bus = |port: &str| bus.iter().any(|c| c.port == port);
        let seen = |port: &str| {
            self.registry.seen.as_ref().map_or_else(
                || self.placed_key(port).is_some(),
                |seen| seen.iter().any(|p| p == port),
            )
        };
        let mut ports: Vec<String> = placed.filter(|port| on_bus(port)).collect();
        ports.extend(
            bus.iter()
                .filter(|c| !seen(&c.port))
                .map(|c| c.port.clone()),
        );
        ports.sort_by(|a, b| (self.order)(a, b));
        ports.dedup();
        ports
    }

    /// Whether the bus differs from what the last start's scan saw: a placed
    /// camera's port gone, or a port gained since.
    fn bus_changed(&self) -> bool {
        let predicted = self.predicted(true);
        let placed: Vec<&str> = self
            .registry
            .placed
            .iter()
            .map(|p| p.port.as_str())
            .collect();
        predicted.len() != placed.len() || predicted.iter().zip(&placed).any(|(a, b)| a != b)
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
            wheel: entry.is_some_and(|e| e.wheel.is_some()),
            port,
        }
    }

    /// `port`'s row without a list: checked when the running service serves
    /// a camera there, with that camera's override names and the wheel it
    /// serves on it.
    fn unlisted_row(&self, config: &Value, port: String) -> Row {
        let names = self
            .placed_key(&port)
            .and_then(|key| override_of(config, key));
        Row {
            checked: matches!(self.served_on(&port), Some((_, Served::Camera { .. }))),
            name: text_in(names, "name"),
            description: text_in(names, "description"),
            wheel: self.serves_wheel_on(&port),
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
                    wheel: form.contains_key(&format!("usb_devices.{i}.wheel")),
                })
            })
            .collect()
    }

    /// The list the checked `rows` make, numbered from what `before` serves
    /// them at: the rows with a number keep its order, and the rest follow
    /// in row order. A kept entry's other keys stay as `before` has them.
    /// With wheels, the checked rows' wheels are numbered the same way
    /// ([`Self::declare_wheels`]).
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
        let mut list: Vec<(&Row, Map<String, Value>)> = kept
            .into_iter()
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
                (row, object)
            })
            .collect();
        if self.wheels {
            self.declare_wheels(rows, &mut list, before);
        }
        list.into_iter()
            .map(|(_, object)| Value::Object(object))
            .collect()
    }

    /// Number the wheels of `list`'s rows that declare one as the cameras
    /// are: the ones with a wheel number in `before` keep its order, and
    /// the rest follow in row order. An entry whose wheel goes loses its
    /// filter names with it; one whose wheel is new takes its camera's
    /// override's, when `before` has one.
    fn declare_wheels(
        &self,
        rows: &[Row],
        list: &mut [(&Row, Map<String, Value>)],
        before: &Value,
    ) {
        let numbers = self.wheel_numbers(before);
        let declared = |port: &str| list.iter().any(|(row, _)| row.port == port && row.wheel);
        let mut kept: Vec<(u64, &str)> = numbers
            .iter()
            .filter(|(port, _)| declared(port))
            .map(|(port, number)| (*number, port.as_str()))
            .collect();
        kept.sort_by_key(|(number, _)| *number);
        let added = rows
            .iter()
            .filter(|r| declared(&r.port) && !numbers.contains_key(&r.port))
            .map(|r| r.port.as_str());
        let order: Vec<String> = kept
            .into_iter()
            .map(|(_, port)| port)
            .chain(added)
            .map(str::to_string)
            .collect();
        for (row, object) in list.iter_mut() {
            let Some(number) = order.iter().position(|port| *port == row.port) else {
                _ = object.remove(WHEEL_NUMBER);
                _ = object.remove(FILTER_NAMES);
                continue;
            };
            _ = object.insert(WHEEL_NUMBER.to_string(), Value::from(number));
            if !object.contains_key(FILTER_NAMES) {
                let carried = self
                    .placed_key(&row.port)
                    .and_then(|key| override_of(before, key))
                    .and_then(|names| names.get(FILTER_NAMES));
                if let Some(names) = carried {
                    _ = object.insert(FILTER_NAMES.to_string(), names.clone());
                }
            }
        }
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
                    @if self.wheels { th { "Filter wheel" } }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr { td colspan="7" { "No " (sdk) " camera is on the bus, and none is listed." } }
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
                        @if self.wheels {
                            td.wheel {
                                input.only-b type="checkbox" name=(format!("usb_devices.{i}.wheel"))
                                    aria-label=(format!("Serve a filter wheel on the camera on {}", row.port))
                                    checked[row.wheel];
                                " "
                                span.mono.wheel-serves { (self.wheel_label(&row.port)) }
                            }
                        }
                    }
                }
            }
        }
        }
    }

    /// What the Cameras section says the running service serves as the
    /// filter wheel on `port`'s camera.
    fn wheel_label(&self, port: &str) -> String {
        match self.wheel_on(port) {
            Some((number, ServedWheel::Wheel { .. })) => format!("filter wheel {number}"),
            Some((number, ServedWheel::Placeholder { .. })) => format!("placeholder {number}"),
            None => "no wheel".to_string(),
        }
    }

    /// Each port served at one number in `before` and another in `after`,
    /// `all_open` as for [`Self::numbers_after`].
    fn moves(&self, before: &Value, after: &Value, all_open: bool) -> Vec<Move> {
        self.moves_between(&self.numbers(before), &self.numbers_after(after, all_open))
    }

    /// Each port numbered `from` one number and `to` another.
    fn moves_between(&self, from: &BTreeMap<String, u64>, to: &BTreeMap<String, u64>) -> Vec<Move> {
        let mut moves: Vec<Move> = from
            .iter()
            .filter_map(|(port, from)| {
                let to = *to.get(port)?;
                (*from != to).then(|| (port.clone(), *from, to))
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
        moves: &[Move],
        all_open: bool,
    ) -> Vec<Retake> {
        Self::retaken_between(
            &self.numbers(before),
            &self.numbers_after(after, all_open),
            moves,
        )
    }

    /// Each number `from` and `to` both give, to different ports, where the
    /// new port's is none of `moves`.
    fn retaken_between(
        from: &BTreeMap<String, u64>,
        to: &BTreeMap<String, u64>,
        moves: &[Move],
    ) -> Vec<Retake> {
        let held: BTreeMap<u64, &String> =
            from.iter().map(|(port, number)| (*number, port)).collect();
        let mut retaken: Vec<Retake> = to
            .iter()
            .filter_map(|(port, number)| {
                let old = held.get(number)?;
                let moved_in = moves.iter().any(|(p, _, to)| p == port && to == number);
                (*old != port && !moved_in).then(|| (*number, (*old).clone(), port.clone()))
            })
            .collect();
        retaken.sort_by_key(|(number, _, _)| *number);
        retaken
    }

    /// The filter wheels a save from `before` to `after` moves, and the
    /// wheel numbers it gives another camera's wheel, as for the cameras.
    fn wheel_moves(&self, before: &Value, after: &Value) -> (Vec<Move>, Vec<Retake>) {
        let (from, to) = (self.wheel_numbers(before), self.wheel_numbers_after(after));
        let moves = self.moves_between(&from, &to);
        let retaken = Self::retaken_between(&from, &to, &moves);
        (moves, retaken)
    }

    /// What entering `list` drops of the override `names` under `key`: all
    /// of it when its camera is no placed one or the list does not serve
    /// it, its filter names when the list does not declare its wheel.
    fn override_dropped(&self, key: &str, names: &Value, list: &[Entry]) -> Option<Dropped> {
        let Some(port) = self.placed_port(key) else {
            return Some(Dropped::Key(key.to_string()));
        };
        let Some(entry) = list.iter().find(|e| e.port == port) else {
            return Some(Dropped::Unserved(port.to_string()));
        };
        (has_filter_names(names.as_object()) && !has_filter_names(Some(&entry.object)))
            .then(|| Dropped::FilterNames(port.to_string()))
    }

    /// The names `before` holds that `after` keeps nowhere: an entry's,
    /// leaving a list, whose camera has no override key the page knows; an
    /// entry's no longer listed, every row unchecked included; an
    /// override's, entering a list, whose camera's row is not listed; and
    /// a camera's filter names, where the list no longer declares its
    /// wheel. Each is named by its port, else by its override key.
    fn dropped(&self, before: &Value, after: &Value) -> Vec<Dropped> {
        let mut dropped: Vec<Dropped> = match (entries(before), entries(after)) {
            (Some(list), Some(kept)) => list
                .into_iter()
                .filter_map(|e| match kept.iter().find(|k| k.port == e.port) {
                    None => e.has_names().then_some(Dropped::Unlisted(e.port)),
                    Some(k) => (has_filter_names(Some(&e.object))
                        && !has_filter_names(Some(&k.object)))
                    .then_some(Dropped::FilterNames(e.port)),
                })
                .collect(),
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
                .filter_map(|(key, names)| self.override_dropped(key, names, &list))
                .collect(),
            _ => Vec::new(),
        };
        // In port order, then the keys: a map's own order is serde_json's
        // feature choice, and the confirmation's token must not depend on it.
        dropped.sort_by(|a, b| match (a.port(), b.port()) {
            (Some(a), Some(b)) => (self.order)(a, b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.key().cmp(&b.key()),
        });
        dropped
    }
}

/// Names a save would drop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dropped {
    /// The names of the camera on, or the entry for, this port, which no
    /// override can take.
    Port(String),
    /// The names of the entry for this port, which is no longer listed.
    Unlisted(String),
    /// The override of the camera on this port, whose row the new list
    /// leaves unchecked.
    Unserved(String),
    /// The override under this key, whose camera the last start did not
    /// serve.
    Key(String),
    /// The filter names of the camera on this port, whose wheel the list
    /// does not declare.
    FilterNames(String),
}

impl Dropped {
    fn port(&self) -> Option<&str> {
        match self {
            Self::Port(port)
            | Self::Unlisted(port)
            | Self::Unserved(port)
            | Self::FilterNames(port) => Some(port),
            Self::Key(_) => None,
        }
    }

    fn key(&self) -> Option<&str> {
        match self {
            Self::Key(key) => Some(key),
            Self::Port(_) | Self::Unlisted(_) | Self::Unserved(_) | Self::FilterNames(_) => None,
        }
    }

    /// Its token in a confirmation.
    fn token(&self) -> String {
        match self {
            Self::Port(port) | Self::Unlisted(port) | Self::Unserved(port) => {
                format!("drop {port}")
            }
            Self::Key(key) => format!("drop key {key}"),
            Self::FilterNames(port) => format!("drop filter names {port}"),
        }
    }
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
                @if self.wheels {
                    p.hint.only-b {
                        "A checked filter wheel is numbered the same way among the wheels, and goes with its camera when the camera is unchecked. "
                        "Its filter names are edited on the wheel's own page."
                    }
                    p.hint.only-a {
                        "Without a list, a wheel is served on each camera whose CFW probe finds one, numbered in their cameras' port order."
                    }
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
        if let Some(listed) = entries(&before) {
            // Leaving a list: each listed camera's names, and its wheel's
            // filter names, become its override.
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
                let filters = listed
                    .iter()
                    .find(|e| e.port == row.port)
                    .and_then(|e| e.object.get(FILTER_NAMES));
                if let Some(filters) = filters {
                    _ = names.insert(FILTER_NAMES.to_string(), filters.clone());
                }
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
        let moves = self.moves(before, after, false);
        let retaken = self.retaken(before, after, &moves, false);
        let dropped = self.dropped(before, after);
        let unopened = self.unopened();
        let all_open = self.if_all_open(after, &unopened, before, (&moves, &retaken));
        let (wheel_moves, wheel_retaken) = self.wheel_moves(before, after);
        let by_probe = self.wheels_by_probe(before, after);
        let wheels_move = !wheel_moves.is_empty() || !wheel_retaken.is_empty();
        if moves.is_empty()
            && retaken.is_empty()
            && dropped.is_empty()
            && all_open.is_none()
            && !wheels_move
            && !by_probe
        {
            return None;
        }
        let mut tokens = Self::move_tokens("", &moves, &retaken);
        if let Some((moves, retaken)) = &all_open {
            tokens.extend(Self::move_tokens("if all open: ", moves, retaken));
            if moves.is_empty() && retaken.is_empty() {
                tokens.push("if all open: nothing moves".to_string());
            }
        }
        tokens.extend(Self::move_tokens("wheel ", &wheel_moves, &wheel_retaken));
        if by_probe {
            tokens.push("wheels by probe".to_string());
        }
        tokens.extend(dropped.iter().map(Dropped::token));
        let afresh = entries(after).is_none() && self.bus_changed();
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
                (Self::wheel_items(&wheel_moves, &wheel_retaken))
                (self.dropped_items(&dropped))
            }
            @if let Some((if_moves, if_retaken)) = &all_open {
                (Self::if_all_open_markup(&unopened, moves.is_empty() && retaken.is_empty(), if_moves, if_retaken))
            }
            @if afresh && (!moves.is_empty() || !retaken.is_empty()) {
                p {
                    "The bus has changed since the service last started, so the reload numbers the cameras afresh, in port order. "
                    "The page opens nothing, so it takes a record new since then for a camera; the reload's own scan decides."
                }
            }
            @if by_probe {
                p.wheels-by-probe {
                    "Without a list, the reload serves a filter wheel on each camera whose CFW probe finds one, numbered in their cameras' port order. "
                    "The page opens nothing, so it cannot tell which those are: a declared wheel the probe misses, or one on a camera that declared none, "
                    "moves the wheel numbers after it. What is listed is for the probe finding each declared wheel whose camera the reload serves, and no other."
                }
            }
            @if !moves.is_empty() || !retaken.is_empty() || all_open.as_ref().is_some_and(|(m, r)| !m.is_empty() || !r.is_empty()) {
                p {
                    "rp's camera entries bound to the numbers that move must be renumbered to match: an entry without a "
                    code { "unique_id" } " pin would otherwise open the wrong camera, with no error. rp's own page shows which entries those are."
                }
            }
            @if wheels_move || by_probe {
                p {
                    "rp's filter wheel entries bound to the wheel numbers that move must be renumbered to match: an entry without a "
                    code { "unique_id" } " pin would otherwise open the wrong wheel, with no error."
                }
            }
        };
        Some(Confirmation {
            token: tokens.join("; "),
            markup,
        })
    }
}

impl CamerasSection {
    /// The confirmation's items for the filter wheels a save moves, and the
    /// wheel numbers it gives another camera's wheel.
    fn wheel_items(moves: &[Move], retaken: &[Retake]) -> Markup {
        html! {
            @for (port, from, to) in moves {
                li.wheel-move data-port=(port) data-from=(from) data-to=(to) {
                    "move the filter wheel on the camera on " code { (port) } " from wheel number " (from) " to " (to)
                }
            }
            @for (number, old, new) in retaken {
                li.wheel-retaken data-number=(number) data-from=(old) data-to=(new) {
                    "give wheel number " (number) ", which the wheel on the camera on " code { (old) }
                    " held, to the wheel on the camera on " code { (new) }
                }
            }
        }
    }

    /// What a camera's entry or override holds that a save can drop.
    const fn names(&self) -> &'static str {
        if self.wheels {
            "name, description and filter names"
        } else {
            "name and description"
        }
    }

    /// The confirmation's items for the names a save drops.
    fn dropped_items(&self, dropped: &[Dropped]) -> Markup {
        let names = self.names();
        html! {
            @for d in dropped {
                @match d {
                    Dropped::Port(port) => li.dropped data-port=(port) {
                        "drop the " (names) " of " code { (port) } ", which has no camera whose override the page could write"
                    },
                    Dropped::Unlisted(port) => li.dropped data-port=(port) {
                        "drop the " (names) " of the entry for " code { (port) } ", which is no longer listed"
                    },
                    Dropped::Unserved(port) => li.dropped data-port=(port) {
                        "drop the " code { "devices" } " override of the camera on " code { (port) } ", which the list does not serve"
                    },
                    Dropped::Key(key) => li.dropped data-key=(key) {
                        "drop the " code { "devices" } " override " code { (key) } ", whose camera the last start did not serve"
                    },
                    Dropped::FilterNames(port) => li.dropped-filter-names data-port=(port) {
                        "drop the filter names of the camera on " code { (port) } ", whose filter wheel the list does not declare"
                    },
                }
            }
        }
    }

    /// The moves and retaken numbers if every camera in `unopened`, which
    /// the start placed and did not open, opens on the reload a no-list save
    /// fires — when they differ from `if_none`'s, the ones for none of them
    /// opening. `None` when the outcome does not depend on those opens.
    fn if_all_open(
        &self,
        after: &Value,
        unopened: &[&str],
        before: &Value,
        if_none: (&[Move], &[Retake]),
    ) -> Option<(Vec<Move>, Vec<Retake>)> {
        if entries(after).is_some() || unopened.is_empty() {
            return None;
        }
        let moves = self.moves(before, after, true);
        let retaken = self.retaken(before, after, &moves, true);
        (moves.as_slice() != if_none.0 || retaken.as_slice() != if_none.1)
            .then_some((moves, retaken))
    }

    /// What the confirmation says of the cameras in `unopened`, which the
    /// start did not open: that the page cannot tell which the reload
    /// opens, the rule it numbers them by, and what it does if all of them
    /// open. `none_moves` when none of them opening moves no number.
    fn if_all_open_markup(
        unopened: &[&str],
        none_moves: bool,
        if_moves: &[Move],
        if_retaken: &[Retake],
    ) -> Markup {
        html! {
            p {
                "The last start did not open the "
                (if unopened.len() == 1 { "camera on " } else { "cameras on " })
                @for (i, port) in unopened.iter().enumerate() {
                    @if i > 0 { ", " }
                    code { (port) }
                }
                " — not listed, or its open failed — and the page opens nothing, so it cannot tell which the reload will open. "
                "The reload serves each camera it opens in its place in port order, moving the numbers after it up one. "
                @if none_moves {
                    "If none of them opens, no number moves. "
                } @else {
                    "What is listed above is for none of them opening. "
                }
                "If all of them open, the reload instead:"
            }
            ul.if-all-open {
                @for (port, from, to) in if_moves {
                    li.move-if-all-open data-port=(port) data-from=(from) data-to=(to) {
                        "moves the camera on " code { (port) } " from number " (from) " to " (to)
                    }
                }
                @for (number, old, new) in if_retaken {
                    li.retaken-if-all-open data-number=(number) data-from=(old) data-to=(new) {
                        "gives number " (number) ", which the camera on " code { (old) }
                        " held, to the camera on " code { (new) }
                    }
                }
                @if if_moves.is_empty() && if_retaken.is_empty() {
                    li { "moves no number" }
                }
            }
        }
    }

    /// The confirmation's tokens for `moves` and `retaken`, each after
    /// `prefix`.
    fn move_tokens(prefix: &str, moves: &[Move], retaken: &[Retake]) -> Vec<String> {
        moves
            .iter()
            .map(|(port, from, to)| format!("{prefix}move {port} {from}>{to}"))
            .chain(
                retaken
                    .iter()
                    .map(|(number, old, new)| format!("{prefix}number {number} {old}>{new}")),
            )
            .collect()
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
    /// The Cameras section's numbering, which a save of this page — a
    /// reload, like any — is checked against; `None` on a page that only
    /// shows (a `GET`), which needs no scan.
    numbering: Option<CamerasSection>,
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

    /// The reload this page's save fires numbers the bus as it is now, so a
    /// camera plugged in or pulled since the start is confirmed as the move
    /// it causes, as on the server page.
    fn confirmation(&self, before: &Value, after: &Value) -> Option<Confirmation> {
        self.numbering.as_ref()?.confirmation(before, after)
    }
}

/// A filter wheel's page: what the driver knows, and its filter names,
/// which it edits — in its camera's `usb_devices` entry with a list, in its
/// camera's `devices` override without one.
struct WheelPage {
    served: ServedWheel,
    /// The Cameras section's numbering, which a save of this page — a
    /// reload, like any — is checked against; `None` on a page that only
    /// shows (a `GET`), which needs no scan.
    numbering: Option<CamerasSection>,
}

/// Where a configuration keeps a wheel's filter names.
enum NamesAt {
    /// In the `usb_devices` entry at this index.
    Entry(usize),
    /// In the `devices` override under this key.
    Override(String),
}

impl WheelPage {
    /// Where `config` keeps this wheel's filter names: its camera's entry
    /// with a list, its camera's override without one — or why the page
    /// cannot write them, when the file has changed since the start.
    fn names_at(&self, config: &Value) -> Result<NamesAt, String> {
        let port = self.served.port();
        if let Some(list) = config.get(LIST).and_then(Value::as_array) {
            let index = list
                .iter()
                .position(|e| e.get("usb_port").and_then(Value::as_str) == Some(port))
                .ok_or_else(|| {
                    format!("the usb_devices list no longer lists the camera on {port}")
                })?;
            if list.get(index).and_then(|e| e.get(WHEEL_NUMBER)).is_none() {
                return Err(format!(
                    "the usb_devices entry for {port} no longer declares its filter wheel"
                ));
            }
            return Ok(NamesAt::Entry(index));
        }
        match &self.served {
            ServedWheel::Wheel { override_key, .. } => Ok(NamesAt::Override(override_key.clone())),
            ServedWheel::Placeholder { .. } => Err(format!(
                "without a list, no camera on {port} is served to hang this wheel off"
            )),
        }
    }

    /// The filter names `config` holds for this wheel.
    fn stored(&self, config: &Value) -> Vec<String> {
        let object = match self.names_at(config) {
            Ok(NamesAt::Entry(index)) => config
                .get(LIST)
                .and_then(|list| list.get(index))
                .and_then(Value::as_object),
            Ok(NamesAt::Override(key)) => override_of(config, &key),
            Err(_) => None,
        };
        object
            .and_then(|o| o.get(FILTER_NAMES))
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Set `object`'s filter names to `names`; none removes them.
fn set_filter_names(object: &mut Map<String, Value>, names: Vec<String>) {
    if names.is_empty() {
        _ = object.remove(FILTER_NAMES);
    } else {
        _ = object.insert(FILTER_NAMES.to_string(), Value::from(names));
    }
}

impl CustomSection for WheelPage {
    fn render(&self, view: SectionView<'_>) -> Markup {
        let value = view
            .submitted
            .and_then(|form| form.get(WHEEL_NAMES))
            .map_or_else(|| self.stored(view.config).join("\n"), str::to_string);
        let errors: Vec<&FieldError> = view
            .errors
            .iter()
            .filter(|e| self.shows_error(&e.path))
            .collect();
        let port = self.served.port();
        html! {
            @match &self.served {
                ServedWheel::Wheel { unique_id, .. } => {
                    (facts(&[("On the camera on USB port", port), ("UniqueID", unique_id)]))
                }
                ServedWheel::Placeholder { reason, .. } => {
                    (facts(&[("Stands in for the wheel on the camera on USB port", port), ("Why no wheel is served", reason)]))
                }
            }
            p {
                @if entries(view.config).is_some() {
                    "Declared in its camera's row of the server page's " a href=(CAMERAS_LINK) { "Cameras section" } ", which numbers it."
                } @else {
                    "Served without a list, on the camera whose CFW probe found it, numbered in their cameras' port order; "
                    "to pin its number, list its camera and check its wheel in the server page's " a href=(CAMERAS_LINK) { "Cameras section" } "."
                }
            }
            fieldset {
                legend { "Filter names" }
                div.field.invalid[!errors.is_empty()] {
                    label for=(WHEEL_NAMES) { "One name per line, from the first slot" }
                    // A browser drops the newline right after the tag, so a
                    // blank first slot needs one of its own before it.
                    textarea id=(WHEEL_NAMES) name=(WHEEL_NAMES) rows="8" { "\n" (value) }
                    @for e in &errors {
                        div.error { code { (e.path) } " " (e.msg) }
                    }
                    div.hint { "Blank uses the names the wheel reports (Filter0, Filter1, …)." }
                    @match self.names_at(view.config) {
                        Ok(NamesAt::Entry(_)) => div.hint { "Saved in the " code { "usb_devices" } " entry for " code { (port) } "." },
                        Ok(NamesAt::Override(key)) => div.hint { "Saved under " code { (key) } " in the " code { "devices" } " map." },
                        Err(why) => div.error { (why) "." },
                    }
                }
            }
        }
    }

    fn has_controls(&self) -> bool {
        true
    }

    /// One name per line, each trimmed; trailing blank lines are dropped,
    /// and a blank line among the names is kept, so the save's validation
    /// names its slot rather than the next name taking it.
    fn apply(&self, form: &FormValues, config: &mut Value) -> Vec<FieldError> {
        let mut names: Vec<String> = form
            .get(WHEEL_NAMES)
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().to_string())
            .collect();
        while names.last().is_some_and(String::is_empty) {
            _ = names.pop();
        }
        let not_an_object = || {
            vec![FieldError {
                path: String::new(),
                msg: "the configuration file is not a JSON object".to_string(),
            }]
        };
        match self.names_at(config) {
            Err(msg) => vec![FieldError {
                path: WHEEL_NAMES.to_string(),
                msg,
            }],
            Ok(NamesAt::Entry(index)) => {
                let Some(entry) = config
                    .get_mut(LIST)
                    .and_then(Value::as_array_mut)
                    .and_then(|list| list.get_mut(index))
                    .and_then(Value::as_object_mut)
                else {
                    return not_an_object();
                };
                set_filter_names(entry, names);
                Vec::new()
            }
            Ok(NamesAt::Override(key)) => {
                let Some(overrides) = overrides_mut(config) else {
                    return not_an_object();
                };
                let mut object = overrides
                    .get(&key)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                set_filter_names(&mut object, names);
                // An override left empty goes, as a camera page's does.
                if object.is_empty() {
                    _ = overrides.remove(&key);
                } else {
                    _ = overrides.insert(key, Value::Object(object));
                }
                Vec::new()
            }
        }
    }

    /// Its control's errors, and a filter name's wherever the file keeps
    /// it.
    fn shows_error(&self, path: &str) -> bool {
        path == WHEEL_NAMES || path.split('.').any(|segment| segment == FILTER_NAMES)
    }

    /// Without a list the reload this page's save fires numbers the bus as
    /// it is now, as a camera page's save does.
    fn confirmation(&self, before: &Value, after: &Value) -> Option<Confirmation> {
        self.numbering.as_ref()?.confirmation(before, after)
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
            override_key: Some(key.to_string()),
            served_if_opens: false,
        }
    }

    /// Mark `camera` as one the start placed and did not open, whose key a
    /// driver that reads it by opening does not know.
    fn unopen(camera: &mut PlacedCamera) {
        camera.override_key = None;
        camera.served_if_opens = true;
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
                wheels: Vec::new(),
                placed: vec![placed("a:1", "K1"), placed("a:3", "K3")],
                // The start saw all three; a:2 is no SDK camera's.
                seen: Some(vec![
                    "a:1".to_string(),
                    "a:2".to_string(),
                    "a:3".to_string(),
                ]),
            },
            order: str::cmp,
            wheels: false,
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

    fn with_bus(mut s: CamerasSection, ports: &[&str]) -> CamerasSection {
        s.bus = Ok(ports.iter().map(|p| bus(p, "SVBONY SV605CC")).collect());
        s
    }

    #[test]
    fn leaving_a_list_after_a_camera_was_plugged_in_ahead_asks_to_confirm_the_move() {
        // The start served a:3 from the list; a:0 has appeared since.
        let s = with_bus(
            section(vec![("Main", camera("a:3", "K3", true))]),
            &["a:0", "a:1", "a:2", "a:3"],
        );
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:3" } ] });
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[
                ("a:0", false, ""),
                ("a:1", false, ""),
                ("a:2", false, ""),
                ("a:3", true, ""),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s.confirmation(&before, &config).expect("a move to confirm");
        // a:1 was placed and is still there; a:0 is new; a:2 was seen and
        // not placed, so stays unplaced. a:3 moves, and its number goes to
        // the camera new on a:0.
        assert_eq!(confirmation.token, "move a:3 0>2; number 0 a:3>a:0");
        let html = confirmation.markup.into_string();
        assert!(html.contains("The bus has changed"), "{html}");
    }

    #[test]
    fn a_save_without_a_list_asks_when_the_bus_changed_the_port_order() {
        let s = with_bus(
            section(vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ]),
            &["a:1", "a:2", "a:3", "a:4"],
        );
        // a:1 is gone, so a:3 takes its number on the reload any save fires.
        let s = with_bus(s, &["a:2", "a:3"]);
        let mut config = json!({ "server": { "port": 1 } });
        let before = config.clone();
        let submitted = rows_form(AUTO, &[("a:2", false, ""), ("a:3", true, "")]);
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            s.confirmation(&before, &config).expect("a move").token,
            "move a:3 1>0"
        );
    }

    #[test]
    fn leaving_a_list_names_the_moves_either_way_a_failed_open_goes() {
        // a:1's listed camera failed its open, so a placeholder holds 0.
        let mut s = section(vec![
            ("One", placeholder("a:1")),
            ("Three", camera("a:3", "K3", true)),
        ]);
        unopen(&mut s.registry.placed[0]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1" },
            { "device_number": 1, "usb_port": "a:3" }
        ]});
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[("a:1", true, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s.confirmation(&before, &config).expect("a move");
        // Failing again, a:1 is served nowhere and a:3 takes 0; opening,
        // it keeps 0 and nothing moves.
        assert_eq!(
            confirmation.token,
            "move a:3 1>0; if all open: nothing moves"
        );
        let html = confirmation.markup.into_string();
        assert!(html.contains("did not open the camera on"), "{html}");
        assert!(
            html.contains("What is listed above is for none of them opening"),
            "{html}"
        );
        assert!(html.contains("moves no number"), "{html}");
    }

    #[test]
    fn without_a_list_a_camera_whose_open_failed_is_not_numbered_and_asks_if_it_opens() {
        // The start placed a:1 but could not open it; a:3 is served at 0.
        let mut s = section(vec![("Three", camera("a:3", "K3", false))]);
        unopen(&mut s.registry.placed[0]);
        let mut config = json!({ "server": { "port": 1 } });
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[("a:1", false, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s
            .confirmation(&before, &config)
            .expect("a move if a:1 opens");
        assert_eq!(
            confirmation.token,
            "if all open: move a:3 0>1; if all open: number 0 a:3>a:1"
        );
        let html = confirmation.markup.into_string();
        assert!(
            html.contains(
                r#"li class="move-if-all-open" data-port="a:3" data-from="0" data-to="1""#
            ),
            "{html}"
        );
        assert!(html.contains("must be renumbered"), "{html}");
        assert!(
            html.contains("If none of them opens, no number moves."),
            "{html}"
        );
    }

    #[test]
    fn leaving_a_list_asks_both_ways_for_an_unlisted_camera_the_start_never_opened() {
        // The list serves a:3; the camera on a:1 is unlisted, so the start
        // never opened it, and another application may hold it.
        let mut s = section(vec![("Three", camera("a:3", "K3", true))]);
        unopen(&mut s.registry.placed[0]);
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:3" } ] });
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[("a:1", false, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s
            .confirmation(&before, &config)
            .expect("a move if a:1 opens");
        assert_eq!(
            confirmation.token,
            "if all open: move a:3 0>1; if all open: number 0 a:3>a:1"
        );
    }

    #[test]
    fn several_unopened_cameras_are_named_with_the_rule_for_those_that_open() {
        let mut s = section(vec![("Three", camera("a:3", "K3", false))]);
        s.registry.placed = vec![placed("a:1", ""), placed("a:2", ""), placed("a:3", "K3")];
        unopen(&mut s.registry.placed[0]);
        unopen(&mut s.registry.placed[1]);
        let mut config = json!({ "server": { "port": 1 } });
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[("a:1", false, ""), ("a:2", false, ""), ("a:3", true, "")],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s
            .confirmation(&before, &config)
            .expect("a move if they open");
        assert_eq!(
            confirmation.token,
            "if all open: move a:3 0>2; if all open: number 0 a:3>a:1"
        );
        let html = confirmation.markup.into_string();
        assert!(html.contains("did not open the cameras on"), "{html}");
        assert!(
            html.contains("serves each camera it opens in its place in port order"),
            "{html}"
        );
    }

    #[test]
    fn an_unchanged_bus_or_a_failed_scan_predicts_what_the_start_placed() {
        let mut s = section(vec![
            ("One", camera("a:1", "K1", false)),
            ("Three", camera("a:3", "K3", false)),
        ]);
        let config = json!({});
        assert!(s.confirmation(&config, &config).is_none());
        assert!(!s.bus_changed());
        s.bus = Err("no".to_string());
        assert_eq!(
            s.predicted(false),
            vec!["a:1".to_string(), "a:3".to_string()]
        );
        assert!(s.confirmation(&config, &config).is_none());
    }

    #[test]
    fn unchecking_a_named_entry_names_the_names_it_drops() {
        let s = section(vec![("Main", camera("a:1", "K1", true))]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "name": "Main" }
        ]});
        let before = config.clone();
        let submitted = rows_form(
            LISTED,
            &[
                ("a:1", false, "Main"),
                ("a:2", false, ""),
                ("a:3", false, ""),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config["usb_devices"], json!([]));
        let confirmation = s.confirmation(&before, &config).expect("names to confirm");
        assert_eq!(confirmation.token, "drop a:1");
        assert!(
            confirmation
                .markup
                .into_string()
                .contains("no longer listed"),
            "the reason is the unlisting"
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
    fn leaving_a_list_drops_the_names_of_a_camera_whose_key_was_never_read() {
        // The join placed a camera on a:3 too, but its open failed, so a
        // placeholder holds its number and the driver never read its key.
        let mut s = section(vec![
            ("Main", camera("a:1", "K1", true)),
            ("Three", placeholder("a:3")),
        ]);
        unopen(&mut s.registry.placed[1]);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "name": "Main" },
            { "device_number": 1, "usb_port": "a:3", "name": "Three" }
        ]});
        let before = config.clone();
        let submitted = rows_form(
            AUTO,
            &[
                ("a:1", true, "Main"),
                ("a:2", false, ""),
                ("a:3", true, "Three"),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config, json!({ "devices": { "K1": { "name": "Main" } } }));
        // Its port still numbers in port order: nothing moves.
        let confirmation = s.confirmation(&before, &config).expect("dropped names");
        assert_eq!(confirmation.token, "drop a:3");
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
        let html = confirmation.markup.into_string();
        // a:3's camera is there; its row is only left unchecked.
        assert!(html.contains("which the list does not serve"), "{html}");
        assert!(!html.contains("no camera whose override"), "{html}");
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
            numbering: Some(section(vec![("Main", camera("a:1", "K1", false))])),
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
    fn a_camera_pages_save_asks_when_the_bus_changed_the_port_order() {
        let placed_two = || {
            section(vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ])
        };
        let page = |numbering| OverridePage {
            port: "a:3".to_string(),
            model: "SV605CC".to_string(),
            unique_id: "SVBONY:SV605CC:K3".to_string(),
            key: "K3".to_string(),
            numbering,
        };
        let before = json!({ "devices": {} });
        let after = json!({ "devices": { "K3": { "name": "Three" } } });
        // The bus as the start found it: nothing moves.
        assert!(page(Some(placed_two()))
            .confirmation(&before, &after)
            .is_none());
        // A page that only showed took no scan, and asks nothing.
        assert!(page(None).confirmation(&before, &after).is_none());
        // a:1 has gone since: a:3 takes its number on the reload.
        let page = page(Some(with_bus(placed_two(), &["a:2", "a:3"])));
        assert_eq!(
            page.confirmation(&before, &after).expect("a move").token,
            "move a:3 1>0"
        );
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

    fn wheel(port: &str, key: &str) -> ServedWheel {
        ServedWheel::Wheel {
            port: port.to_string(),
            unique_id: format!("CFW-{key}"),
            override_key: key.to_string(),
        }
    }

    /// `section`, for a driver whose entries declare wheels, serving
    /// `wheels` in wheel-number order.
    fn wheel_section(served: Vec<(&str, Served)>, wheels: Vec<ServedWheel>) -> CamerasSection {
        let mut s = section(served);
        s.wheels = true;
        s.registry.wheels = wheels
            .into_iter()
            .map(|w| ("Wheel".to_string(), w))
            .collect();
        s
    }

    /// The form the rows submit in `mode`: each row's port, whether it is
    /// checked, and whether its wheel is.
    fn wheel_rows_form(mode: &str, rows: &[(&str, bool, bool)]) -> FormValues {
        let mut pairs = vec![(MODE.to_string(), mode.to_string())];
        for (i, (port, checked, wheel)) in rows.iter().enumerate() {
            pairs.push((format!("usb_devices.{i}.port"), (*port).to_string()));
            if *checked {
                pairs.push((format!("usb_devices.{i}.serve"), "on".to_string()));
            }
            if *wheel {
                pairs.push((format!("usb_devices.{i}.wheel"), "on".to_string()));
            }
            pairs.push((format!("usb_devices.{i}.name"), String::new()));
            pairs.push((format!("usb_devices.{i}.description"), String::new()));
        }
        FormValues::from(pairs)
    }

    #[test]
    fn a_driver_without_wheels_draws_no_wheel_column() {
        let s = section(vec![("Main", camera("a:1", "K1", false))]);
        let html = s.render(view(&json!({}))).into_string();
        assert!(!html.contains("usb_devices.0.wheel"), "{html}");
        assert!(!html.contains("Filter wheel"), "{html}");
    }

    #[test]
    fn without_a_list_each_row_checks_the_wheel_served_on_its_camera() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ],
            vec![wheel("a:3", "K3")],
        );
        let config = json!({});
        assert_eq!(
            s.rows(&config)
                .iter()
                .map(|r| (r.port.as_str(), r.wheel))
                .collect::<Vec<_>>(),
            vec![("a:1", false), ("a:2", false), ("a:3", true)]
        );
        let html = s.render(view(&config)).into_string();
        assert!(html.contains(r#"name="usb_devices.2.wheel""#), "{html}");
        assert!(html.contains("filter wheel 0"), "{html}");
        assert!(html.contains("no wheel"), "{html}");
    }

    #[test]
    fn changing_to_a_list_keeps_the_wheel_numbers_and_carries_the_filter_names() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ],
            vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
        );
        let mut config = json!({ "devices": { "K3": { "filter_names": ["L", "R"] } } });
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config,
            json!({
                "usb_devices": [
                    { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0 },
                    { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 1,
                      "filter_names": ["L", "R"] }
                ],
                "devices": {}
            })
        );
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn a_newly_declared_wheel_takes_the_next_wheel_number() {
        let s = wheel_section(
            vec![
                ("Three", camera("a:3", "K3", true)),
                ("One", camera("a:1", "K1", true)),
            ],
            vec![wheel("a:3", "K3")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 0 },
            { "device_number": 1, "usb_port": "a:1" }
        ]});
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 0 },
                { "device_number": 1, "usb_port": "a:1", "filter_wheel_number": 1 }
            ])
        );
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn newly_declared_wheels_take_the_next_wheel_numbers_in_port_order() {
        let s = wheel_section(
            vec![
                ("Three", camera("a:3", "K3", true)),
                ("One", camera("a:1", "K1", true)),
            ],
            Vec::new(),
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:3" },
            { "device_number": 1, "usb_port": "a:1" }
        ]});
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 1 },
                { "device_number": 1, "usb_port": "a:1", "filter_wheel_number": 0 }
            ])
        );
    }

    #[test]
    fn the_declared_wheels_keep_the_order_of_their_numbers_not_of_their_ports() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", true)),
                ("Three", camera("a:3", "K3", true)),
            ],
            vec![wheel("a:3", "K3"), wheel("a:1", "K1")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 1 },
            { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 0 }
        ]});
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(config["usb_devices"], before["usb_devices"]);
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn without_a_list_a_camera_with_no_wheel_now_is_forecast_with_none() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ],
            vec![wheel("a:3", "K3")],
        );
        // a:1's camera has no wheel, so a:3's keeps wheel number 0.
        assert!(s
            .confirmation(&json!({}), &json!({ "server": { "port": 2 } }))
            .is_none());
    }

    #[test]
    fn undeclaring_a_lower_wheel_moves_the_ones_after_it_and_drops_its_filter_names() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", true)),
                ("Three", camera("a:3", "K3", true)),
            ],
            vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0, "filter_names": ["L"] },
            { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 1 }
        ]});
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, false),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:1" },
                { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 0 }
            ])
        );
        let confirmation = s.confirmation(&before, &config).expect("a wheel move");
        assert_eq!(
            confirmation.token,
            "wheel move a:3 1>0; drop filter names a:1"
        );
        let html = confirmation.markup.into_string();
        assert!(
            html.contains(r#"li class="wheel-move" data-port="a:3" data-from="1" data-to="0""#),
            "{html}"
        );
        assert!(
            html.contains(r#"li class="dropped-filter-names" data-port="a:1""#),
            "{html}"
        );
        assert!(html.contains("filter wheel entries"), "{html}");
        assert!(!html.contains("camera entries"), "no camera moves: {html}");
    }

    #[test]
    fn unchecking_a_camera_takes_its_wheel_with_it() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", true)),
                ("Three", camera("a:3", "K3", true)),
            ],
            vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0 },
            { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 1 }
        ]});
        let before = config.clone();
        // a:1's wheel box stays checked: unchecking the camera is enough.
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", false, true),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([{ "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 0 }])
        );
        assert_eq!(
            s.confirmation(&before, &config).expect("moves").token,
            "move a:3 1>0; wheel move a:3 1>0"
        );
    }

    #[test]
    fn a_wheel_number_given_to_another_cameras_wheel_asks_to_confirm() {
        let s = wheel_section(
            vec![("One", camera("a:1", "K1", true))],
            vec![wheel("a:1", "K1")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0 }
        ]});
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", false, false),
                ("a:2", false, false),
                ("a:3", true, true),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        let confirmation = s.confirmation(&before, &config).expect("numbers");
        assert_eq!(
            confirmation.token,
            "number 0 a:1>a:3; wheel number 0 a:1>a:3"
        );
        assert!(confirmation
            .markup
            .into_string()
            .contains(r#"li class="wheel-retaken" data-number="0" data-from="a:1" data-to="a:3""#));
    }

    #[test]
    fn leaving_a_list_that_declares_a_wheel_asks_and_carries_its_filter_names() {
        let s = wheel_section(
            vec![("One", camera("a:1", "K1", true))],
            vec![wheel("a:1", "K1")],
        );
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0, "filter_names": ["L"] }
        ]});
        let before = config.clone();
        let submitted = wheel_rows_form(
            AUTO,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", false, false),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config,
            json!({ "devices": { "K1": { "filter_names": ["L"] } } })
        );
        let confirmation = s
            .confirmation(&before, &config)
            .expect("the probe numbers the wheels");
        assert_eq!(confirmation.token, "wheels by probe");
        let html = confirmation.markup.into_string();
        assert!(html.contains(r#"p class="wheels-by-probe""#), "{html}");
        assert!(html.contains("filter wheel entries"), "{html}");
    }

    #[test]
    fn leaving_a_list_that_declares_no_wheel_asks_nothing_of_wheels() {
        let s = wheel_section(vec![("One", camera("a:1", "K1", true))], Vec::new());
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:1" } ] });
        let before = config.clone();
        let submitted = wheel_rows_form(
            AUTO,
            &[
                ("a:1", true, false),
                ("a:2", false, false),
                ("a:3", false, false),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert!(s.confirmation(&before, &config).is_none());
    }

    #[test]
    fn without_a_list_a_wheel_whose_camera_left_the_bus_moves_the_ones_after_it() {
        let s = with_bus(
            wheel_section(
                vec![
                    ("One", camera("a:1", "K1", false)),
                    ("Three", camera("a:3", "K3", false)),
                ],
                vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
            ),
            &["a:2", "a:3"],
        );
        let config = json!({ "server": { "port": 1 } });
        assert_eq!(
            s.confirmation(&config, &json!({ "server": { "port": 2 } }))
                .expect("moves")
                .token,
            "move a:3 1>0; wheel move a:3 1>0"
        );
    }

    #[test]
    fn entering_a_list_that_leaves_a_wheel_undeclared_names_its_filter_names() {
        let s = wheel_section(
            vec![
                ("One", camera("a:1", "K1", false)),
                ("Three", camera("a:3", "K3", false)),
            ],
            vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
        );
        let mut config = json!({ "devices": { "K3": { "filter_names": ["L"] } } });
        let before = config.clone();
        let submitted = wheel_rows_form(
            LISTED,
            &[
                ("a:1", true, true),
                ("a:2", false, false),
                ("a:3", true, false),
            ],
        );
        assert_eq!(s.apply(&submitted, &mut config), Vec::<FieldError>::new());
        assert_eq!(
            config["usb_devices"],
            json!([
                { "device_number": 0, "usb_port": "a:1", "filter_wheel_number": 0 },
                { "device_number": 1, "usb_port": "a:3" }
            ])
        );
        assert_eq!(
            s.confirmation(&before, &config).expect("names").token,
            "drop filter names a:3"
        );
    }

    #[test]
    fn a_dropped_entrys_filter_names_are_named_with_its_names() {
        let s = wheel_section(vec![("Held", placeholder("a:9"))], Vec::new());
        let before = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:9", "filter_wheel_number": 0, "filter_names": ["L"] }
        ]});
        let confirmation = s
            .confirmation(&before, &json!({ "devices": {} }))
            .expect("names");
        assert!(
            confirmation.token.contains("drop a:9"),
            "{}",
            confirmation.token
        );
        assert!(confirmation
            .markup
            .into_string()
            .contains("name, description and filter names"));
    }

    fn wheel_page(served: ServedWheel, numbering: Option<CamerasSection>) -> WheelPage {
        WheelPage { served, numbering }
    }

    fn names(text: &str) -> FormValues {
        form(&[(WHEEL_NAMES, text)])
    }

    #[test]
    fn with_a_list_a_wheels_page_edits_the_filter_names_in_its_entry() {
        let page = wheel_page(wheel("a:3", "K3"), None);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:1" },
            { "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 0 }
        ]});
        assert_eq!(
            page.apply(&names("L\r\n R \n\n"), &mut config),
            Vec::<FieldError>::new()
        );
        assert_eq!(config["usb_devices"][1]["filter_names"], json!(["L", "R"]));
        let html = page.render(view(&config)).into_string();
        assert!(html.contains(">\nL\nR</textarea>"), "{html}");
        assert!(html.contains("Declared in its camera's row"), "{html}");
        assert!(html.contains("CFW-K3"), "{html}");
        assert_eq!(
            page.apply(&names(" \n"), &mut config),
            Vec::<FieldError>::new()
        );
        assert_eq!(
            config["usb_devices"][1],
            json!({ "device_number": 1, "usb_port": "a:3", "filter_wheel_number": 0 })
        );
        assert!(page.has_controls());
    }

    #[test]
    fn a_blank_line_among_the_filter_names_keeps_its_slot() {
        let page = wheel_page(wheel("a:3", "K3"), None);
        let mut config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 0 }
        ]});
        assert_eq!(
            page.apply(&names("L\n\nR"), &mut config),
            Vec::<FieldError>::new()
        );
        assert_eq!(
            config["usb_devices"][0]["filter_names"],
            json!(["L", "", "R"])
        );
    }

    #[test]
    fn without_a_list_a_wheels_page_edits_its_cameras_override() {
        let page = wheel_page(wheel("a:3", "K3"), None);
        let mut config = json!({ "devices": { "K3": { "name": "Main" } } });
        assert_eq!(
            page.apply(&names("Ha\nOIII"), &mut config),
            Vec::<FieldError>::new()
        );
        assert_eq!(
            config["devices"]["K3"],
            json!({ "name": "Main", "filter_names": ["Ha", "OIII"] })
        );
        let html = page.render(view(&config)).into_string();
        assert!(html.contains("Served without a list"), "{html}");
        assert!(html.contains("Saved under"), "{html}");
        // Its names cleared, the override keeps the camera's; cleared too,
        // it goes.
        assert_eq!(
            page.apply(&names(""), &mut config),
            Vec::<FieldError>::new()
        );
        assert_eq!(config["devices"]["K3"], json!({ "name": "Main" }));
        let mut bare = json!({ "devices": { "K3": { "filter_names": ["L"] } } });
        assert_eq!(page.apply(&names(""), &mut bare), Vec::<FieldError>::new());
        assert_eq!(bare["devices"], json!({}));
    }

    #[test]
    fn a_wheels_page_says_when_the_file_no_longer_declares_it() {
        let page = wheel_page(wheel("a:3", "K3"), None);
        let mut config = json!({ "usb_devices": [ { "device_number": 0, "usb_port": "a:3" } ] });
        let errors = page.apply(&names("L"), &mut config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, WHEEL_NAMES);
        assert!(
            errors[0].msg.contains("no longer declares"),
            "{}",
            errors[0].msg
        );
        let placeholder = wheel_page(
            ServedWheel::Placeholder {
                port: "a:9".to_string(),
                reason: "no working camera".to_string(),
            },
            None,
        );
        let mut no_list = json!({});
        assert_eq!(placeholder.apply(&names("L"), &mut no_list).len(), 1);
        let html = placeholder.render(view(&no_list)).into_string();
        assert!(html.contains("no working camera"), "{html}");
    }

    #[test]
    fn a_wheels_page_shows_a_filter_names_error_wherever_the_file_keeps_them() {
        let page = wheel_page(wheel("a:3", "K3"), None);
        assert!(page.shows_error(WHEEL_NAMES));
        assert!(page.shows_error("usb_devices.1.filter_names"));
        assert!(page.shows_error("devices.K3.filter_names.0"));
        assert!(!page.shows_error("server.port"));
        let errors = [FieldError {
            path: "usb_devices.0.filter_names".to_string(),
            msg: "filter_names[1] is empty".to_string(),
        }];
        let config = json!({ "usb_devices": [
            { "device_number": 0, "usb_port": "a:3", "filter_wheel_number": 0 }
        ]});
        let submitted = names("L\n\nR");
        let html = page
            .render(SectionView {
                config: &config,
                submitted: Some(&submitted),
                errors: &errors,
            })
            .into_string();
        assert!(html.contains("filter_names[1] is empty"), "{html}");
        assert!(html.contains("L\n\nR</textarea>"), "{html}");
        assert!(html.contains(r#"class="field invalid""#), "{html}");
    }

    #[test]
    fn without_a_list_a_wheels_pages_save_asks_when_the_bus_changed() {
        let numbering = with_bus(
            wheel_section(
                vec![
                    ("One", camera("a:1", "K1", false)),
                    ("Three", camera("a:3", "K3", false)),
                ],
                vec![wheel("a:1", "K1"), wheel("a:3", "K3")],
            ),
            &["a:2", "a:3"],
        );
        let page = wheel_page(wheel("a:3", "K3"), Some(numbering));
        let before = json!({ "devices": {} });
        let after = json!({ "devices": { "K3": { "filter_names": ["L"] } } });
        assert_eq!(
            page.confirmation(&before, &after).expect("moves").token,
            "move a:3 1>0; wheel move a:3 1>0"
        );
        assert!(wheel_page(wheel("a:3", "K3"), None)
            .confirmation(&before, &after)
            .is_none());
    }

    #[tokio::test]
    async fn each_wheel_has_a_page_beside_the_cameras() {
        let p = ClaimsPages::new(
            "QHY",
            Arc::new(|| Ok(vec![bus("a:1", "QHY600M")])),
            str::cmp,
        )
        .with_wheels();
        assert_eq!(p.register("Main", camera("a:1", "K1", false)), 0);
        assert_eq!(p.register_wheel("Wheel", wheel("a:1", "K1")), 0);
        p.placed(vec![placed("a:1", "K1")], vec!["a:1".to_string()]);
        assert_eq!(
            p.devices()
                .unwrap()
                .iter()
                .map(|d| (d.device_type.as_str(), d.device_number, d.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("camera", 0, "Main"), ("filterwheel", 0, "Wheel")]
        );
        let wheel_scope = PageScope::Device {
            device_type: "filterwheel",
            device_number: 0,
        };
        let shown = p.sections(wheel_scope, false).await;
        assert_eq!(shown.len(), 1);
        assert!(shown[0].has_controls());
        let html = shown[0].render(view(&json!({}))).into_string();
        assert!(html.contains("CFW-K1"), "{html}");
        let server = p.sections(PageScope::Server, false).await;
        let html = server[0].render(view(&json!({}))).into_string();
        assert!(html.contains("filter wheel 0"), "{html}");
        assert_eq!(
            p.sections(
                PageScope::Device {
                    device_type: "filterwheel",
                    device_number: 1,
                },
                false,
            )
            .await
            .len(),
            0
        );
    }

    fn pages() -> ClaimsPages {
        let pages = ClaimsPages::new(
            "SVBony",
            Arc::new(|| Ok(vec![bus("a:1", "SVBONY SV605CC")])),
            str::cmp,
        );
        assert_eq!(pages.register("Main", camera("a:1", "K1", false)), 0);
        assert_eq!(pages.register("Held", placeholder("a:9")), 1);
        pages.placed(vec![placed("a:1", "K1")], vec!["a:1".to_string()]);
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
        let server = p.sections(PageScope::Server, false).await;
        assert_eq!(server.len(), 1);
        assert!(server[0].has_controls());
        let html = server[0].render(view(&json!({}))).into_string();
        assert!(html.contains(r#"data-port="a:1""#), "{html}");

        let unlisted = p
            .sections(
                PageScope::Device {
                    device_type: "camera",
                    device_number: 0,
                },
                false,
            )
            .await;
        assert!(unlisted[0].has_controls());
        let held = p
            .sections(
                PageScope::Device {
                    device_type: "camera",
                    device_number: 1,
                },
                false,
            )
            .await;
        assert!(!held[0].has_controls());
        assert_eq!(
            p.sections(
                PageScope::Device {
                    device_type: "camera",
                    device_number: 2,
                },
                false,
            )
            .await
            .len(),
            0
        );
        assert_eq!(
            p.sections(
                PageScope::Device {
                    device_type: "filterwheel",
                    device_number: 0,
                },
                false,
            )
            .await
            .len(),
            0
        );
    }

    #[tokio::test]
    async fn an_unlisted_cameras_page_scans_only_for_a_save() {
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&scans);
        let p = ClaimsPages::new(
            "SVBony",
            Arc::new(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![bus("a:1", "SVBONY SV605CC")])
            }),
            str::cmp,
        );
        assert_eq!(p.register("Main", camera("a:1", "K1", false)), 0);
        p.placed(vec![placed("a:1", "K1")], vec!["a:1".to_string()]);
        let camera_page = PageScope::Device {
            device_type: "camera",
            device_number: 0,
        };
        let shown = p.sections(camera_page, false).await;
        assert!(shown[0].has_controls());
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 0);
        let _saved = p.sections(camera_page, true).await;
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The server page's rows are the bus now, shown or saved.
        let _server = p.sections(PageScope::Server, false).await;
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_scan_that_panics_is_a_failed_scan() {
        let p = ClaimsPages::new("SVBony", Arc::new(|| panic!("collector")), str::cmp);
        let server = p.sections(PageScope::Server, false).await;
        let html = server[0].render(view(&json!({}))).into_string();
        assert!(
            html.contains("The USB scan failed: the USB scan did not finish"),
            "{html}"
        );
    }
}

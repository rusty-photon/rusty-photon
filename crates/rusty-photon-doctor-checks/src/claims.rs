//! Device claims: the vendor-neutral half of a camera driver's `usb_devices`
//! list ([device-claims plan](../../../docs/plans/archive/device-claims-and-phd2-camera.md),
//! Part A).
//!
//! A list pins each USB port to an Alpaca device number. What is the same in
//! every camera driver lives here, beside the USB inventory it reads: the
//! list's validation, the passive join that places each SDK camera on a port,
//! the reason a listed number is held by a placeholder, the placeholder's
//! fixed error code and `UniqueID` form, the order ports are listed in, where
//! the scan comes from, the `doctor --devices` listing and its block, and the
//! list entry itself ([`UsbDeviceEntry`]) with the glue that validates a list
//! of them. What differs per driver — which SDK models it knows on the bus,
//! its simulated scan, what it can say about a camera without opening it, and
//! any field its entry adds (a QHY camera's declared filter wheel) — stays in
//! the driver. The placeholder device itself and the
//! failed scan's background re-scan are `rusty-photon-driver`'s `claims`
//! module: they are an ASCOM device and a task, and this crate is neither
//! ASCOM's nor async.
//!
//! Everything here but [`UsbSource::scan`] is pure: nothing scans, opens or
//! sleeps.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::facts::{UsbDevice, UsbScan};
use crate::report::{Check, Status};

/// The ASCOM error number a placeholder's `Connected = true` fails with.
///
/// Driver code `0x40` of the `0x500`–`0xFFF` driver range, the same in every
/// camera driver with device claims, so a client — rp's reconnect supervisor
/// — can tell "no camera here until the driver reloads" from a transient
/// failure without parsing the message.
pub const PLACEHOLDER_ERROR_CODE: u16 = 0x500 + PLACEHOLDER_DRIVER_CODE;

/// [`PLACEHOLDER_ERROR_CODE`] as an offset into the driver range, the form
/// `ascom-alpaca`'s `ASCOMErrorCode::new_for_driver` takes.
pub const PLACEHOLDER_DRIVER_CODE: u16 = 0x40;

/// The prefix of every placeholder's `UniqueID`; no camera's can start with it.
const PLACEHOLDER_ID_PREFIX: &str = "placeholder:";

/// A placeholder's `UniqueID`: `placeholder:<service>:<usb_port>`, a form no
/// camera's can take.
#[must_use]
pub fn placeholder_unique_id(service: &str, usb_port: &str) -> String {
    format!("{PLACEHOLDER_ID_PREFIX}{service}:{usb_port}")
}

/// A placeholder's `Name` when its entry gives none: `<sdk> camera on
/// <usb_port> (placeholder)`.
#[must_use]
pub fn placeholder_name(sdk: &str, usb_port: &str) -> String {
    format!("{sdk} camera on {usb_port} (placeholder)")
}

/// What a wheel placeholder's port carries in its `UniqueID`, so it is not its
/// camera's (`qhy-camera`'s declared filter wheel).
const PLACEHOLDER_WHEEL_PREFIX: &str = "CFW-";

/// A wheel placeholder's `UniqueID`: `placeholder:<service>:CFW-<usb_port>`,
/// which neither a device nor its camera's placeholder can take.
#[must_use]
pub fn placeholder_wheel_unique_id(service: &str, usb_port: &str) -> String {
    placeholder_unique_id(service, &format!("{PLACEHOLDER_WHEEL_PREFIX}{usb_port}"))
}

/// Whether a device is a device-claims placeholder, by its `UniqueID`.
///
/// The driver range is shared by every Alpaca driver, so a client tells a
/// placeholder's [`PLACEHOLDER_ERROR_CODE`] from another driver's use of the
/// same number by this, not by the code alone.
#[must_use]
pub fn is_placeholder_unique_id(unique_id: &str) -> bool {
    unique_id.starts_with(PLACEHOLDER_ID_PREFIX)
}

/// The USB port a placeholder stands in for, read back from its `UniqueID`.
///
/// A camera's or a wheel's; `None` for any other device's. A service name
/// holds no `:`, so the port is everything after the second field, `:`s and
/// all.
#[must_use]
pub fn placeholder_port(unique_id: &str) -> Option<&str> {
    let (_service, port) = unique_id
        .strip_prefix(PLACEHOLDER_ID_PREFIX)?
        .split_once(':')?;
    Some(port.strip_prefix(PLACEHOLDER_WHEEL_PREFIX).unwrap_or(port))
}

// --- the list -------------------------------------------------------------------

/// One `usb_devices` entry: the camera on `usb_port` is served at
/// `device_number`.
///
/// The entry every camera driver with device claims reads. A driver whose
/// entry carries more — `qhy-camera`'s declared filter wheel — spells its own
/// with these fields first and implements [`ListEntry`] for it, since a
/// flattened struct cannot refuse unknown keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbDeviceEntry {
    /// The Alpaca device number the camera on this port is served under.
    pub device_number: u32,
    /// The port, in the platform's native spelling, pasted from the driver's
    /// `doctor --devices`.
    pub usb_port: String,
    /// Display name override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What validation reads of one `usb_devices` entry.
#[derive(Debug, Clone, Copy)]
pub struct EntryView<'a> {
    pub device_number: u32,
    pub usb_port: &'a str,
}

/// A `usb_devices` entry, whichever driver's: what the shared validation and
/// the `doctor --devices` block read of it.
pub trait ListEntry {
    /// The fields every list rule reads.
    fn view(&self) -> EntryView<'_>;
    /// The entry as the paste-ready block reproduces it.
    fn block_entry(&self) -> BlockEntry;
}

impl ListEntry for UsbDeviceEntry {
    fn view(&self) -> EntryView<'_> {
        EntryView {
            device_number: self.device_number,
            usb_port: &self.usb_port,
        }
    }

    fn block_entry(&self) -> BlockEntry {
        BlockEntry {
            device_number: self.device_number,
            usb_port: self.usb_port.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            ..BlockEntry::default()
        }
    }
}

/// `list`'s entries as the paste-ready block reproduces them.
#[must_use]
pub fn block_entries<E: ListEntry>(list: &[E]) -> Vec<BlockEntry> {
    list.iter().map(ListEntry::block_entry).collect()
}

/// Every rule a configured `usb_devices` list breaks; empty without a list.
///
/// The shared rules, then an [`override_beside_list`] error for each key of
/// the `devices` map beside the list. A driver's load and its setup page's
/// save both refuse a config with any.
#[must_use]
pub fn list_errors<'a, E: ListEntry>(
    list: Option<&[E]>,
    override_keys: impl IntoIterator<Item = &'a String>,
) -> Vec<ListError> {
    let Some(list) = list else {
        return Vec::new();
    };
    let views: Vec<EntryView<'_>> = list.iter().map(ListEntry::view).collect();
    let mut errors = validate_list(&views);
    errors.extend(
        override_keys
            .into_iter()
            .map(|key| override_beside_list(key)),
    );
    errors
}

/// `errors` as one message, every rule broken named.
///
/// # Errors
/// The joined message, when there is any error.
pub fn check_list(errors: &[ListError]) -> Result<(), String> {
    if errors.is_empty() {
        return Ok(());
    }
    let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
    Err(errors.join("; "))
}

/// A rule a list (or the config around it) breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListError {
    /// The dotted config path a setup page reports it at
    /// (`usb_devices.1.usb_port`).
    pub path: String,
    /// What a load error names it by (`usb_devices[1]`).
    pub location: String,
    /// What is wrong.
    pub message: String,
}

impl ListError {
    /// An error in entry `index`'s `field`.
    #[must_use]
    pub fn entry(index: usize, field: &str, message: String) -> Self {
        Self {
            path: format!("usb_devices.{index}.{field}"),
            location: format!("usb_devices[{index}]"),
            message,
        }
    }

    /// An error in the list as a whole.
    #[must_use]
    pub fn list(message: String) -> Self {
        Self {
            path: "usb_devices".to_string(),
            location: "usb_devices".to_string(),
            message,
        }
    }
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Every rule a `usb_devices` list breaks, entry by entry.
///
/// Each `usb_port` non-blank, unpadded and unique, each `device_number`
/// unique, and the numbers exactly `0..N-1`. Empty means the list is valid. An
/// empty list is valid: it serves no camera.
#[must_use]
pub fn validate_list(entries: &[EntryView<'_>]) -> Vec<ListError> {
    let mut errors = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let earlier = entries.get(..index).unwrap_or_default();
        let port = entry.usb_port;
        if port.trim().is_empty() {
            errors.push(ListError::entry(
                index,
                "usb_port",
                "usb_port is blank".to_string(),
            ));
        } else if port != port.trim() {
            // Rejected, not trimmed: the scan never carries padding, so a
            // padded port could only ever be a placeholder for a port that
            // holds a camera.
            errors.push(ListError::entry(
                index,
                "usb_port",
                "usb_port has leading or trailing whitespace; paste it as \
                 `doctor --devices` prints it"
                    .to_string(),
            ));
        } else if let Some(first) = earlier.iter().position(|e| e.usb_port == port) {
            errors.push(ListError::entry(
                index,
                "usb_port",
                format!("usb_port {port} is also usb_devices[{first}]'s"),
            ));
        }
    }
    let numbers: Vec<Option<u32>> = entries.iter().map(|e| Some(e.device_number)).collect();
    errors.extend(numbering_errors(&numbers, DEVICE_NUMBERS));
    errors
}

/// A number the entries of a list that set it must keep exactly `0..N-1`:
/// what [`numbering_errors`] names it by.
#[derive(Debug, Clone, Copy)]
pub struct Numbering<'a> {
    /// The entry field that holds it (`device_number`).
    pub field: &'a str,
    /// The numbers, as an error names them (`device numbers`).
    pub numbers: &'a str,
    /// The letter the range's bound is written with (`N`).
    pub bound: &'a str,
    /// What a gap asks to renumber, above it (`entries`).
    pub renumber: &'a str,
}

/// Every list's `device_number`.
const DEVICE_NUMBERS: Numbering<'static> = Numbering {
    field: "device_number",
    numbers: "device numbers",
    bound: "N",
    renumber: "entries",
};

/// Every rule `numbers` break: each number set once, and the numbers set
/// exactly `0..count-1`.
///
/// `count` is how many are set, and `numbers` holds each entry's number, in
/// list order, `None` where an entry sets none. A gap is rejected rather than
/// filled, so every number served is one the operator wrote and a typo cannot
/// become a phantom device.
#[must_use]
pub fn numbering_errors(numbers: &[Option<u32>], numbering: Numbering<'_>) -> Vec<ListError> {
    let mut errors = Vec::new();
    for (index, number) in numbers.iter().enumerate() {
        let Some(number) = number else {
            continue;
        };
        let earlier = numbers.get(..index).unwrap_or_default();
        if let Some(first) = earlier.iter().position(|n| *n == Some(*number)) {
            errors.push(ListError::entry(
                index,
                numbering.field,
                format!(
                    "{} {number} is also usb_devices[{first}]'s",
                    numbering.field
                ),
            ));
        }
    }
    let set = numbers.iter().flatten().count();
    let missing: Vec<String> = (0..set)
        .filter_map(|n| u32::try_from(n).ok())
        .filter(|n| !numbers.contains(&Some(*n)))
        .map(|n| n.to_string())
        .collect();
    if !missing.is_empty() {
        let verb = if missing.len() == 1 { "is" } else { "are" };
        errors.push(ListError::list(format!(
            "{} must run 0..{}-1, and {} {verb} missing; a gap is rejected rather than \
             filled, so renumber the {} above it",
            numbering.numbers,
            numbering.bound,
            missing.join(", "),
            numbering.renumber
        )));
    }
    errors
}

/// The error for a `devices` override beside a list: a listed camera's
/// overrides live in its entry, and one the driver would silently ignore is
/// refused instead.
#[must_use]
pub fn override_beside_list(key: &str) -> ListError {
    ListError {
        path: format!("devices.{key}"),
        location: format!("devices.{key}"),
        message: "move its fields into the usb_devices entry for that camera's port, \
                  or delete it if the camera is not listed: a devices override is not \
                  read beside a usb_devices list"
            .to_string(),
    }
}

// --- the join -------------------------------------------------------------------

/// What one driver knows about its SDK's cameras on the bus.
#[derive(Debug, Clone, Copy)]
pub struct Normalizer<'a> {
    /// The SDK's display name, for messages (`SVBony`).
    pub sdk: &'a str,
    /// The vendor id every record of this SDK carries, as the inventory
    /// spells it (four lowercase hex digits).
    pub vendor: &'a str,
    /// Each SDK model this driver has observed, with the product id it
    /// enumerates under on the bus. Observed pairs only: a guess here is a
    /// wrong camera at the wrong number.
    pub models: &'a [(&'a str, &'a str)],
    /// Product ids of this vendor known not to be cameras, so they never
    /// count as a camera's record.
    pub not_cameras: &'a [&'a str],
    /// The SDK's own passive check of one of this vendor's product ids —
    /// whether the SDK supports it as a camera — for a product id `models`
    /// does not assign (ZWO's `ASICameraCheck`). It must open nothing. `None`
    /// when the SDK has no such check, and then every product id `models` and
    /// `not_cameras` leave open could be a camera.
    pub sdk_check: Option<fn(&str) -> bool>,
}

impl Normalizer<'_> {
    fn product_for(&self, model: &str) -> Option<&str> {
        self.models
            .iter()
            .find(|(m, _)| *m == model)
            .map(|(_, product)| *product)
    }

    fn assigns_product(&self, product: &str) -> bool {
        self.models.iter().any(|(_, p)| *p == product)
    }

    /// Whether one of this vendor's product ids can be a camera's: an
    /// observed model's, or — unless it is a known non-camera — one the SDK's
    /// own check accepts.
    fn is_camera_product(&self, product: &str) -> bool {
        if self.assigns_product(product) {
            return true;
        }
        !self.not_cameras.contains(&product) && self.sdk_check.is_none_or(|check| check(product))
    }

    fn is_own_record(&self, record: &UsbDevice) -> bool {
        record.vendor == self.vendor && self.is_camera_product(&record.product)
    }

    /// The working records in `scan` that can be one of this driver's
    /// cameras — its vendor's, under a product id that can be a camera's —
    /// with no SDK call: the rows of the camera drivers' Cameras section.
    #[must_use]
    pub fn camera_records<'s>(&self, scan: &'s UsbScan) -> Vec<&'s UsbDevice> {
        scan.devices
            .iter()
            .filter(|record| self.is_own_record(record))
            .collect()
    }

    /// Whether a fault could be this driver's camera: it carries no real
    /// vendor id — none could be read, or it is the `0000` Windows gives a
    /// device whose enumeration failed (doctor.md, "USB inventory") — or it
    /// carries this vendor's, with no product id or one that can be a
    /// camera's. Another vendor's fault is that vendor's device, and this
    /// vendor's non-camera (a ZWO focuser) is not the camera either.
    fn could_be_camera(&self, fault: &crate::facts::UsbFault) -> bool {
        match fault.vendor.as_deref() {
            None | Some("0000") => true,
            Some(vendor) if vendor == self.vendor => fault
                .product
                .as_deref()
                .is_none_or(|product| self.is_camera_product(product)),
            Some(_) => false,
        }
    }
}

/// One camera the SDK enumerated, as the join reads it: what the SDK reports
/// without opening anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkCamera {
    /// The SDK's model name.
    pub model: String,
    /// A serial the bus could publish too, canonicalized by the driver, when
    /// the SDK reports one before any open.
    pub serial: Option<String>,
}

/// Where the join put one SDK camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraPlace {
    /// On this port.
    Port(String),
    /// It matches records the join cannot pair one-to-one: the ports of the
    /// records it could be, and the other SDK cameras that match them too.
    LookAlike {
        ports: Vec<String>,
        rivals: Vec<usize>,
    },
    /// Its model is one the normalizer does not know, and elimination could
    /// not pair it.
    Unrecognised,
    /// Its model is known, but no working record matches it: its record is a
    /// fault, or it sits on no port the scan could read.
    NoRecord,
}

/// The join's result for a scan that ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// One place per SDK camera, in the order the SDK enumerated them.
    pub cameras: Vec<CameraPlace>,
}

impl Placement {
    /// The SDK camera placed on `port`, by its enumeration index.
    #[must_use]
    pub fn camera_on(&self, port: &str) -> Option<usize> {
        self.cameras
            .iter()
            .position(|place| matches!(place, CameraPlace::Port(p) if p == port))
    }
}

/// Whether `camera` and `record` are each other's on the strongest signal both
/// carry: the serial when each has one, otherwise the product id the
/// normalizer gives the camera's model.
fn keyed_match(normalizer: &Normalizer<'_>, camera: &SdkCamera, record: &UsbDevice) -> bool {
    match (&camera.serial, &record.serial) {
        (Some(sdk), Some(bus)) => sdk == bus,
        _ => normalizer.product_for(&camera.model) == Some(record.product.as_str()),
    }
}

/// Place each SDK camera on a port: a passive join of what the SDK
/// enumerated with the scan's working records (the plan's D4.2).
///
/// A camera is placed only on a **one-to-one** match: it matches exactly one
/// of this vendor's records, and that record matches no other camera. After
/// the keyed matches, a camera whose **model the normalizer does not know** is
/// paired **by elimination** with the one record left over when nothing else
/// could explain either: exactly one of each remains, no fault could be the
/// camera (see [`Normalizer`]'s rule for that), the record's product id is not one the normalizer gives any
/// model, and their serials, when both have one, agree. A known model is never
/// paired this way — its product id is known, and a record under another one
/// is some other device. Everything else is refused, never guessed (D4.3).
#[must_use]
pub fn place(normalizer: &Normalizer<'_>, sdk: &[SdkCamera], scan: &UsbScan) -> Placement {
    let records: Vec<&UsbDevice> = scan
        .devices
        .iter()
        .filter(|r| normalizer.is_own_record(r))
        .collect();
    let matches: Vec<Vec<usize>> = sdk
        .iter()
        .map(|camera| {
            (0..records.len())
                .filter(|&r| {
                    records
                        .get(r)
                        .is_some_and(|rec| keyed_match(normalizer, camera, rec))
                })
                .collect()
        })
        .collect();
    let matched_by = |r: usize| -> Vec<usize> {
        (0..sdk.len())
            .filter(|&c| matches.get(c).is_some_and(|m| m.contains(&r)))
            .collect()
    };

    let mut placed: Vec<Option<usize>> = matches
        .iter()
        .map(|m| match m.as_slice() {
            [r] if matched_by(*r).len() == 1 => Some(*r),
            _ => None,
        })
        .collect();

    let unpaired_cameras: Vec<usize> = (0..sdk.len())
        .filter(|&c| placed.get(c).is_some_and(Option::is_none))
        .collect();
    let unpaired_records: Vec<usize> = (0..records.len())
        .filter(|r| !placed.contains(&Some(*r)))
        .collect();
    if let ([camera], [record]) = (unpaired_cameras.as_slice(), unpaired_records.as_slice()) {
        if let (Some(sdk_camera), Some(rec)) = (sdk.get(*camera), records.get(*record)) {
            // A fault that could be the camera — its own record, which
            // failed — leaves the leftover record unexplained.
            let suspect_fault = scan.faults.iter().any(|f| normalizer.could_be_camera(f));
            // A known model has a known product id, so a record under another
            // one is evidence against the pair, not the absence of evidence
            // elimination needs: its own record may be missing, and the one
            // left over a different device of this vendor.
            let model_known = normalizer.product_for(&sdk_camera.model).is_some();
            let product_known = normalizer.assigns_product(&rec.product);
            let serials_disagree = matches!(
                (&sdk_camera.serial, &rec.serial),
                (Some(sdk), Some(bus)) if sdk != bus
            );
            if !suspect_fault && !model_known && !product_known && !serials_disagree {
                if let Some(slot) = placed.get_mut(*camera) {
                    *slot = Some(*record);
                }
            }
        }
    }

    let port_of = |r: usize| -> String {
        records
            .get(r)
            .and_then(|rec| rec.port.clone())
            .unwrap_or_default()
    };
    let cameras = (0..sdk.len())
        .map(|c| {
            if let Some(Some(r)) = placed.get(c) {
                return CameraPlace::Port(port_of(*r));
            }
            let own = matches.get(c).cloned().unwrap_or_default();
            if own.is_empty() {
                let known = sdk
                    .get(c)
                    .is_some_and(|cam| normalizer.product_for(&cam.model).is_some());
                return if known {
                    CameraPlace::NoRecord
                } else {
                    CameraPlace::Unrecognised
                };
            }
            let mut rivals: Vec<usize> = own
                .iter()
                .flat_map(|&r| matched_by(r))
                .filter(|&other| other != c)
                .collect();
            rivals.sort_unstable();
            rivals.dedup();
            let mut ports: Vec<String> = own.iter().map(|&r| port_of(r)).collect();
            ports.sort_by(|a, b| compare_ports(a, b));
            CameraPlace::LookAlike { ports, rivals }
        })
        .collect();
    Placement { cameras }
}

/// What a listed port resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The SDK camera placed on the port, by its enumeration index.
    Camera(usize),
    /// No camera can be served at the port's number tonight; a placeholder
    /// holds it, for this reason.
    Placeholder(String),
}

/// A scan that ran, and where the join put each camera on it.
#[derive(Debug, Clone)]
struct Joined {
    scan: UsbScan,
    placement: Placement,
}

/// One driver's claims for one start or reload: the scan, what the SDK
/// enumerated, and where the join put each camera.
#[derive(Debug, Clone)]
pub struct Claims<'a> {
    normalizer: Normalizer<'a>,
    service: &'a str,
    sdk: Vec<SdkCamera>,
    /// The scan and its placement, or why the scan could not run — which
    /// places nothing, since a failed scan has no opinion about the bus.
    joined: Result<Joined, String>,
}

impl<'a> Claims<'a> {
    /// Join `sdk` with `scan`, for `service`'s messages.
    #[must_use]
    pub fn new(
        normalizer: Normalizer<'a>,
        service: &'a str,
        scan: Result<UsbScan, String>,
        sdk: Vec<SdkCamera>,
    ) -> Self {
        let joined = scan.map(|scan| Joined {
            placement: place(&normalizer, &sdk, &scan),
            scan,
        });
        Self {
            normalizer,
            service,
            sdk,
            joined,
        }
    }

    /// The join's result, or `None` when the scan failed.
    #[must_use]
    pub fn placement(&self) -> Option<&Placement> {
        self.joined.as_ref().ok().map(|j| &j.placement)
    }

    /// Why the scan could not run, when it could not.
    #[must_use]
    pub fn scan_error(&self) -> Option<&str> {
        self.joined.as_ref().err().map(String::as_str)
    }

    /// The working record on `port`, when the scan has one there.
    #[must_use]
    pub fn record_on(&self, port: &str) -> Option<&UsbDevice> {
        self.joined
            .as_ref()
            .ok()?
            .scan
            .devices
            .iter()
            .find(|d| d.port.as_deref() == Some(port))
    }

    /// What a listed port resolves to: the camera placed there, or why a
    /// placeholder holds it — the first reason that applies (the plan's
    /// D4.5): the scan failed; a fault located at the port; a look-alike or an
    /// unrecognised camera whose record is there; a working record no SDK
    /// camera is placed on; otherwise no working camera on the port. Every
    /// reason ends with the way back.
    #[must_use]
    pub fn resolve(&self, port: &str) -> Resolution {
        let reason = match &self.joined {
            Ok(joined) => {
                if let Some(camera) = joined.placement.camera_on(port) {
                    return Resolution::Camera(camera);
                }
                self.reason_on(&joined.scan, &joined.placement, port)
            }
            Err(error) => {
                return Resolution::Placeholder(format!(
                    "the USB scan failed: {error}. {} re-scans on its own and rebuilds \
                     once a scan succeeds; nothing needs fixing or restarting for it",
                    self.service
                ));
            }
        };
        Resolution::Placeholder(self.with_the_way_back(&reason))
    }

    /// The reason a placeholder holds a listed port whose camera the join
    /// placed but the driver could not open (the plan's D4.4): the SDK's
    /// `error`, and the way back.
    #[must_use]
    pub fn open_failure(&self, port: &str, error: &str) -> String {
        self.with_the_way_back(&format!(
            "the {} SDK could not open the camera placed on {port}: {error}",
            self.normalizer.sdk
        ))
    }

    /// `reason`, ending with the way back every placeholder reason but a
    /// failed scan's ends with.
    fn with_the_way_back(&self, reason: &str) -> String {
        format!(
            "{reason}. Once the camera is fixed, reload or restart {}, which \
             re-opens every camera it serves",
            self.service
        )
    }

    fn reason_on(&self, scan: &UsbScan, placement: &Placement, port: &str) -> String {
        let sdk = self.normalizer.sdk;
        if let Some(fault) = scan
            .faults
            .iter()
            .find(|f| f.location.as_deref() == Some(port))
        {
            return format!(
                "{port} holds a USB record that is not a working device ({}): {}",
                fault.record, fault.reason
            );
        }
        for (index, place) in placement.cameras.iter().enumerate() {
            if let CameraPlace::LookAlike { ports, rivals } = place {
                if ports.iter().any(|p| p == port) {
                    let model = self.sdk.get(index).map_or("", |c| c.model.as_str());
                    return format!(
                        "the {sdk} SDK reports {} {model} camera(s) for the record(s) on {}, \
                         and no serial both the bus and the SDK carry pairs them: they cannot \
                         be told apart, so none of them is served while they are all \
                         connected to this host",
                        rivals.len().saturating_add(1),
                        ports.join(", ")
                    );
                }
            }
        }
        let record = scan
            .devices
            .iter()
            .find(|d| d.port.as_deref() == Some(port));
        if let Some(record) = record {
            let product = record.model.as_deref().unwrap_or("a device");
            let id = format!("{}:{}", record.vendor, record.product);
            // Each model once: two cameras of one model need one entry.
            let mut unrecognised: Vec<&str> = placement
                .cameras
                .iter()
                .zip(&self.sdk)
                .filter(|(place, _)| **place == CameraPlace::Unrecognised)
                .map(|(_, camera)| camera.model.as_str())
                .collect();
            unrecognised.sort_unstable();
            unrecognised.dedup();
            if self.normalizer.is_own_record(record) && !unrecognised.is_empty() {
                let models = unrecognised.join(", ");
                // A record no known model claims could be an unrecognised
                // camera's. One under a known model's product id is that
                // model — which the SDK either does not see, or names in a
                // way the normalizer does not know yet: both are said.
                if self.normalizer.assigns_product(&record.product) {
                    return format!(
                        "{port} holds {product} ({id}), a model this driver knows, but the \
                         {sdk} SDK reports none of that model; it reports camera(s) this driver \
                         knows no product id for ({models}). Either the SDK does not see this \
                         device, or it names it as one of those, and the driver's normalizer \
                         needs that name"
                    );
                }
                return format!(
                    "{port} holds {product} ({id}), and the {sdk} SDK reports camera(s) this \
                     driver knows no product id for ({models}), so nothing pairs them; the \
                     driver's normalizer needs an entry for each of {models}"
                );
            }
            return format!(
                "{port} holds {product} ({id}), which the {sdk} SDK does not report as a camera"
            );
        }
        let suspects = scan
            .faults
            .iter()
            .filter(|f| f.location.as_deref() != Some(port) && self.normalizer.could_be_camera(f))
            .count();
        if suspects == 0 {
            format!("no working camera is enumerated on {port}")
        } else {
            format!(
                "no working camera is enumerated on {port}; the scan also found {suspects} \
                 record(s) that are not working devices and could be the camera — {sdk} \
                 records, or ones whose vendor id could not be read — see doctor's \
                 hardware.usb-fault"
            )
        }
    }

    /// The SDK cameras the join placed, by enumeration index, with their
    /// ports, in port order.
    #[must_use]
    pub fn placed(&self) -> Vec<(usize, &str)> {
        let mut placed: Vec<(usize, &str)> = self
            .placement()
            .into_iter()
            .flat_map(|p| p.cameras.iter().enumerate())
            .filter_map(|(index, place)| match place {
                CameraPlace::Port(port) => Some((index, port.as_str())),
                _ => None,
            })
            .collect();
        placed.sort_by(|a, b| compare_ports(a.1, b.1));
        placed
    }

    /// One line per SDK camera the join could not place, and per record of
    /// this vendor no camera is placed on, for an operator: everything a
    /// normalizer entry or a re-cabling needs.
    #[must_use]
    pub fn unplaced_notes(&self) -> Vec<String> {
        let Ok(Joined { scan, placement }) = &self.joined else {
            return Vec::new();
        };
        let sdk = self.normalizer.sdk;
        let mut notes = Vec::new();
        for (index, place) in placement.cameras.iter().enumerate() {
            let model = self.sdk.get(index).map_or("", |c| c.model.as_str());
            match place {
                CameraPlace::Port(_) => {}
                CameraPlace::LookAlike { ports, .. } => notes.push(format!(
                    "{model} (SDK camera {index}) cannot be told apart from the other \
                     camera(s) on {}: none of them can be served while they are all \
                     connected to this host — cameras of different models, or one per host, \
                     resolve it",
                    ports.join(", ")
                )),
                CameraPlace::Unrecognised => notes.push(format!(
                    "{model} (SDK camera {index}) is a model this driver knows no product id \
                     for, and nothing else pairs it with a record: the {sdk} records on the \
                     bus are listed below"
                )),
                CameraPlace::NoRecord => notes.push(format!(
                    "{model} (SDK camera {index}) has no working record on the bus: its \
                     record may be one of the faults below, or on a port the scan could not \
                     read"
                )),
            }
        }
        for record in scan
            .devices
            .iter()
            .filter(|r| self.normalizer.is_own_record(r))
        {
            let port = record.port.as_deref().unwrap_or_default();
            // A look-alike's record is already named in its note above.
            let look_alike = placement.cameras.iter().any(|place| {
                matches!(place, CameraPlace::LookAlike { ports, .. } if ports.iter().any(|p| p == port))
            });
            if placement.camera_on(port).is_none() && !look_alike {
                notes.push(format!(
                    "{port} holds {} ({}:{}), which no {sdk} SDK camera is placed on",
                    record.model.as_deref().unwrap_or("a device"),
                    record.vendor,
                    record.product
                ));
            }
        }
        for fault in scan
            .faults
            .iter()
            .filter(|f| self.normalizer.could_be_camera(f))
        {
            notes.push(format!(
                "{} ({}) is not a working device: {}",
                fault.record,
                fault.location.as_deref().unwrap_or("location unknown"),
                fault.reason
            ));
        }
        notes
    }
}

// --- port order -----------------------------------------------------------------

/// Compare two ports hop by hop from the root (the plan's D4.7).
///
/// The port is cut into the runs of letters and digits between its
/// separators, and runs compare by width without their leading zeros, then as
/// text. For decimal runs that is numeric order, so `…-0:4.2` precedes
/// `…-0:4.10` and `USB(2)` precedes `USB(14)`; for the hex the platforms write
/// (`PCI(1400)`, a macOS `0x14200000`) it is hex order, padded with zeros or
/// not — a field that happens to be all digits included. Every run has one
/// key, so the order is total — whatever runs meet at a hop, `sort_by` gets a
/// consistent comparison — and equal runs fall back to the whole string.
#[must_use]
pub fn compare_ports(a: &str, b: &str) -> Ordering {
    let keys = |s: &'_ str| -> Vec<(usize, String)> {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|run| !run.is_empty())
            .map(run_key)
            .collect()
    };
    keys(a).cmp(&keys(b)).then_with(|| a.cmp(b))
}

/// A run's sort key: its width, then its text, with its leading zeros dropped
/// so it compares as the number it spells, decimal or hex.
fn run_key(run: &str) -> (usize, String) {
    let run = run.trim_start_matches('0');
    (run.len(), run.to_string())
}

// --- the scan source ------------------------------------------------------------

/// Where a driver's USB scan comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbSource {
    /// The host's collector — the only source a release build has.
    Host,
    /// A fixed inventory: the records of the cameras a driver's simulation
    /// fabricates, which no host scan can see.
    #[cfg(feature = "mock")]
    Simulated(UsbScan),
    /// The staged document in this file, read again at every scan, so a
    /// scenario can stage a failed scan and then repair it (doctor.md, "USB
    /// inventory").
    #[cfg(feature = "mock")]
    Staged(std::path::PathBuf),
}

impl UsbSource {
    /// The source a simulation build's hidden `--usb-inventory` flag selects:
    /// the staged document in `path`, read now so a file that cannot be read
    /// or stages a state no collector could produce refuses the start;
    /// without the flag, `simulated`.
    ///
    /// # Errors
    ///
    /// Why the staged document is unusable, naming the file.
    #[cfg(feature = "mock")]
    pub fn from_flag(path: Option<std::path::PathBuf>, simulated: UsbScan) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::Simulated(simulated));
        };
        crate::facts::StagedUsbInventory::load(&path)?;
        Ok(Self::Staged(path))
    }

    /// Take the scan. Blocking: the host collector reads sysfs or shells out
    /// under its deadline.
    ///
    /// # Errors
    ///
    /// Why the scan could not run — the collector's reason, or a staged
    /// document that cannot be read or stages a failure.
    pub fn scan(&self) -> Result<UsbScan, String> {
        match self {
            Self::Host => crate::facts::scan_usb(),
            #[cfg(feature = "mock")]
            Self::Simulated(scan) => Ok(scan.clone()),
            #[cfg(feature = "mock")]
            Self::Staged(path) => crate::facts::StagedUsbInventory::load(path)
                .and_then(crate::facts::StagedUsbInventory::into_scan),
        }
    }
}

// --- the listing ----------------------------------------------------------------

/// One row of `doctor --devices`: an SDK camera the join placed on a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingRow {
    pub port: String,
    /// The SDK's model name.
    pub model: String,
    /// The SDK's own id for the camera, when it yields one without an open.
    pub sdk_id: Option<String>,
    /// The serial the bus publishes, when it publishes one.
    pub usb_serial: Option<String>,
    /// The number the running configuration serves the camera at, or why it
    /// serves none (`not listed`).
    pub device: String,
}

/// One entry of the paste-ready `usb_devices` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockEntry {
    pub device_number: u32,
    pub usb_port: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// The `FilterWheel` number the entry declares its camera's wheel at
    /// (`qhy-camera` only).
    pub filter_wheel_number: Option<u32>,
    /// The declared wheel's slot names.
    pub filter_names: Option<Vec<String>>,
}

/// Everything `doctor --devices` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The SDK's display name (`SVBony`).
    pub sdk: String,
    pub vendor: String,
    /// The service whose config file the block is pasted into.
    pub service: String,
    /// The placed cameras, in port order.
    pub rows: Vec<ListingRow>,
    /// The paste-ready block; `None` prints none.
    pub block: Option<Vec<BlockEntry>>,
    /// Paragraphs printed after the table, on its rows: what the join could
    /// not place, the unplug procedure.
    pub notes: Vec<String>,
    /// Paragraphs printed after [`Self::notes`], on pasting the block: the
    /// `devices` overrides it cannot carry, deleting the `devices` map,
    /// checking the wheel numbers (`qhy-camera`). `usb-devices.implicit`
    /// carries them with the block.
    pub paste_notes: Vec<String>,
    /// The driver serves a filter wheel through its camera, which an entry
    /// declares (`qhy-camera`): the text above the block says `--devices`
    /// cannot see one and how to declare it.
    pub wheels: bool,
}

/// What `doctor --devices` can say about one SDK camera without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraFacts {
    /// The SDK's model name.
    pub model: String,
    /// The SDK's own id for the camera, when the SDK yields one before any
    /// open.
    pub sdk_id: Option<String>,
    /// The key the camera's `devices` override is read under, when that is
    /// knowable before any open; `None` when telling needs an open (a ZWO
    /// camera's serial), which `--devices` never makes.
    pub override_key: Option<String>,
}

/// A `devices` override's display names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayNames {
    pub name: Option<String>,
    pub description: Option<String>,
    /// The slot names of the camera's filter wheel (`qhy-camera`): an
    /// override that names them says the camera has a wheel, so the no-list
    /// block declares one.
    pub filter_names: Option<Vec<String>>,
}

impl Claims<'_> {
    /// What `doctor --devices` prints (the plan's D5): a row per SDK camera
    /// the join placed, in port order; the paste-ready block; and the notes
    /// after the table.
    ///
    /// `cameras` holds the SDK's cameras in enumeration order, `list` the
    /// configured `usb_devices` entries, and `overrides` the `devices` map.
    /// With a list the block reproduces it; without one it pins what the
    /// no-list default serves, numbered in port order, each camera carrying
    /// the names its override gives it — where the override can be matched
    /// without an open.
    #[must_use]
    pub fn listing(
        &self,
        cameras: &[CameraFacts],
        list: Option<&[BlockEntry]>,
        overrides: &BTreeMap<String, DisplayNames>,
    ) -> Listing {
        let placed = self.placed();
        let rows = placed
            .iter()
            .zip(0_usize..)
            .map(|(&(index, port), position)| {
                let facts = cameras.get(index);
                ListingRow {
                    port: port.to_string(),
                    model: facts.map_or_default(|f| f.model.clone()),
                    sdk_id: facts.and_then(|f| f.sdk_id.clone()),
                    usb_serial: self.record_on(port).and_then(|r| r.serial.clone()),
                    // Without a list a placed camera is served at its place in
                    // port order (the plan's D4.7).
                    device: list.map_or_else(
                        || position.to_string(),
                        |list| {
                            list.iter().find(|e| e.usb_port == port).map_or_else(
                                || "not listed".to_string(),
                                |e| e.device_number.to_string(),
                            )
                        },
                    ),
                }
            })
            .collect::<Vec<_>>();

        let mut notes = self.unplaced_notes();
        let mut paste_notes = Vec::new();
        let block = match list {
            Some(list) => {
                let mut entries = list.to_vec();
                entries.sort_by_key(|e| e.device_number);
                Some(entries)
            }
            None if placed.is_empty() => None,
            None => Some(self.no_list_block(&placed, cameras, overrides, &mut paste_notes)),
        };
        if rows.len() > 1 {
            notes.push(UNPLUG_NOTE.to_string());
        }
        Listing {
            sdk: self.normalizer.sdk.to_string(),
            vendor: self.normalizer.vendor.to_string(),
            service: self.service.to_string(),
            rows,
            block,
            notes,
            paste_notes,
            wheels: false,
        }
    }

    /// The block that pins what the no-list default serves: every placed
    /// camera, numbered in port order, with the names its `devices` override
    /// gives it, and in `paste_notes` each override that it cannot carry and
    /// the map's deletion. A camera whose override names its wheel's slots
    /// has a wheel, so its entry declares one, the wheels numbered in port
    /// order too.
    fn no_list_block(
        &self,
        placed: &[(usize, &str)],
        cameras: &[CameraFacts],
        overrides: &BTreeMap<String, DisplayNames>,
        paste_notes: &mut Vec<String>,
    ) -> Vec<BlockEntry> {
        let key_of = |index: usize| cameras.get(index).and_then(|f| f.override_key.as_ref());
        let mut matched_keys = Vec::new();
        let mut wheels = 0_u32..;
        let block: Vec<BlockEntry> = placed
            .iter()
            .zip(0_u32..)
            .map(|(&(index, port), device_number)| {
                let found = key_of(index).and_then(|key| overrides.get_key_value(key));
                if let Some((key, _)) = found {
                    matched_keys.push(key.as_str());
                }
                let names = found.map(|(_, names)| names);
                let filter_names = names.and_then(|n| n.filter_names.clone());
                BlockEntry {
                    device_number,
                    usb_port: port.to_string(),
                    name: names.and_then(|n| n.name.clone()),
                    description: names.and_then(|n| n.description.clone()),
                    filter_wheel_number: filter_names.as_ref().and_then(|_| wheels.next()),
                    filter_names,
                }
            })
            .collect();
        // A key no placed camera's matched is a stale override when every
        // placed camera's key was known; when some could not be known
        // without an open, it may well be one of theirs.
        let every_key_known = placed.iter().all(|&(index, _)| key_of(index).is_some());
        for key in overrides
            .keys()
            .filter(|k| !matched_keys.contains(&k.as_str()))
        {
            paste_notes.push(if every_key_known {
                format!(
                    "The devices override {key} matches no camera placed on a port; move its \
                     fields into the right entry by hand."
                )
            } else {
                format!(
                    "The devices override {key} cannot be matched to a camera without opening \
                     one, which doctor --devices never does; move its fields into the right \
                     entry by hand."
                )
            });
        }
        if !overrides.is_empty() {
            let carried = if every_key_known {
                "the block carries its names, and "
            } else {
                ""
            };
            paste_notes.push(format!(
                "Delete the devices map from {}.json when you paste this block: {carried}a \
                 devices map beside a usb_devices list refuses the start.",
                self.service
            ));
        }
        block
    }
}

// --- the doctor checks ----------------------------------------------------------

/// The listing's note for an operator who cannot tell its rows apart.
const UNPLUG_NOTE: &str = "Cannot tell which row is which camera? Unplug one and run this \
                           again: the port that disappears is the one you unplugged.";

const RESOLVE: &str = "usb-devices.resolve";
const UNLISTED: &str = "usb-devices.unlisted";
const IMPLICIT: &str = "usb-devices.implicit";

/// What a failed scan's check suggests: the host is what needs fixing, and the
/// service re-scans on its own.
fn failed_scan_suggestion(service: &str) -> String {
    format!(
        "fix what the error names on the host; a running {service} re-scans on its own and \
         rebuilds once a scan succeeds"
    )
}

impl Claims<'_> {
    /// The `usb-devices.*` checks of the driver's `doctor`
    /// ([doctor.md](../../../docs/services/doctor.md), "The per-service
    /// subcommand"), judged from this join — what a start would serve now.
    ///
    /// With `list`, `usb-devices.resolve` and `usb-devices.unlisted`; without
    /// one, `usb-devices.implicit`, whose `ok` names the numbers port order
    /// gives and, given the no-list `listing`, the block that pins them with
    /// the notes that go with pasting it. Only a failed scan fails: a dead
    /// device, an empty listed port, a look-alike or an unrecognised model
    /// only warns.
    #[must_use]
    pub fn checks(&self, list: Option<&[BlockEntry]>, listing: Option<&Listing>) -> Vec<Check> {
        list.map_or_else(
            || self.implicit_checks(listing),
            |list| self.list_checks(list),
        )
    }

    fn list_checks(&self, list: &[BlockEntry]) -> Vec<Check> {
        let mut checks = self.resolve_checks(list);
        if self.scan_error().is_some() {
            return checks;
        }
        for (index, port) in self.placed() {
            if !list.iter().any(|e| e.usb_port == port) {
                checks.push(Check::ok(
                    UNLISTED,
                    None,
                    format!(
                        "{port} ({}) is on the bus and not listed",
                        self.model_of(index)
                    ),
                ));
            }
        }
        checks
    }

    /// `usb-devices.resolve` over `list`: an empty list first, since it
    /// registers nothing and needs no scan, then a failed scan, then each
    /// entry's port.
    fn resolve_checks(&self, list: &[BlockEntry]) -> Vec<Check> {
        if list.is_empty() {
            return vec![Check::ok(
                RESOLVE,
                None,
                "the list is empty: no camera is registered",
            )];
        }
        if let Some(error) = self.scan_error() {
            return vec![Check::fail(
                RESOLVE,
                None,
                format!(
                    "the USB scan failed: {error}; every listed number would be a placeholder \
                     until a scan succeeds"
                ),
                Some(failed_scan_suggestion(self.service)),
            )];
        }
        let mut entries: Vec<&BlockEntry> = list.iter().collect();
        entries.sort_by_key(|e| e.device_number);
        let mut checks: Vec<Check> = entries
            .iter()
            .filter_map(|entry| match self.resolve(&entry.usb_port) {
                Resolution::Camera(_) => None,
                Resolution::Placeholder(reason) => Some(Check::warn(
                    RESOLVE,
                    None,
                    format!(
                        "device {} ({}) would be a placeholder: {reason}",
                        entry.device_number, entry.usb_port
                    ),
                    None,
                )),
            })
            .collect();
        if checks.is_empty() {
            checks.push(Check::ok(
                RESOLVE,
                None,
                match entries.len() {
                    1 => "1 listed port resolves, no placeholders".to_string(),
                    n => format!("{n} listed ports resolve, no placeholders"),
                },
            ));
        }
        checks
    }

    fn implicit_checks(&self, listing: Option<&Listing>) -> Vec<Check> {
        if let Some(error) = self.scan_error() {
            return vec![Check::fail(
                IMPLICIT,
                None,
                format!(
                    "the USB scan failed: {error}; without a list no camera is registered until \
                     a scan succeeds"
                ),
                Some(failed_scan_suggestion(self.service)),
            )];
        }
        let mut checks = Vec::new();
        let placed = self.placed();
        if placed.len() > 1 {
            let served: Vec<String> = placed
                .iter()
                .zip(0_usize..)
                .map(|(&(index, port), number)| {
                    format!("{number} {} on {port}", self.model_of(index))
                })
                .collect();
            let mut detail = format!(
                "without a list the service serves {} cameras, numbered in port order: {}",
                placed.len(),
                served.join(", ")
            );
            if let Some(listing) = listing {
                if let Some(block) = &listing.block {
                    let _ = write!(
                        detail,
                        ". To pin them, paste this into {}.json:\n{}",
                        self.service,
                        render_block(block)
                    );
                    // What the listing says about pasting it goes with the
                    // block. Its notes on the rows do not: what the join
                    // could not place is this check's warnings, below, and
                    // a check has no rows to unplug.
                    for note in &listing.paste_notes {
                        let _ = write!(detail, "\n{note}");
                    }
                }
            }
            checks.push(Check::ok(IMPLICIT, None, detail));
        }
        checks.extend(
            self.unplaced_notes()
                .into_iter()
                .map(|note| Check::warn(IMPLICIT, None, note, None)),
        );
        checks
    }

    /// The SDK's model name for the camera at `index`.
    fn model_of(&self, index: usize) -> &str {
        self.sdk.get(index).map_or("", |c| c.model.as_str())
    }
}

/// The `usb-devices.*` checks a camera driver's `doctor` report carries.
///
/// `scan` and the SDK's `cameras` are joined as a start would join them, and
/// [`Claims::checks`] judges the result, with the no-list block and notes
/// `listing` builds.
///
/// The report enumerates the SDK once, for `hardware.sdk-devices` and these
/// alike, so `cameras` arrives enumerated. A caller whose configuration does
/// not load, or whose SDK does not enumerate, makes none of these:
/// `config.full-shape` or `hardware.sdk-devices` says so.
pub fn usb_devices_checks(
    normalizer: Normalizer<'_>,
    service: &str,
    list: Option<&[BlockEntry]>,
    scan: Result<UsbScan, String>,
    cameras: Vec<SdkCamera>,
    listing: impl FnOnce(&Claims<'_>) -> Listing,
) -> Vec<Check> {
    let claims = Claims::new(normalizer, service, scan, cameras);
    let listing = list
        .is_none()
        .then(|| claims.placement().map(|_| listing(&claims)))
        .flatten();
    claims.checks(list, listing.as_ref())
}

/// `checks` as the lines that close `doctor --devices`: one per check, its
/// status first, lowercase. A status this build does not know, which it never
/// emits, reads as a failure, as the text report reads it.
fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let status = match check.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail | Status::Unknown => "fail",
        };
        let _ = writeln!(out, "{status:<4} {}: {}", check.name, check.detail);
    }
    out
}

const ABSENT: &str = "—";

/// Render `listing` as `doctor --devices` prints it. Columns are separated by
/// at least two spaces, which no cell contains.
#[must_use]
pub fn render_listing(listing: &Listing) -> String {
    let mut out = format!(
        "{} cameras on the bus (vendor {}), in port order:\n\n",
        listing.sdk, listing.vendor
    );
    if listing.rows.is_empty() {
        let _ = writeln!(out, "  No {} camera is placed on a port.", listing.sdk);
    } else {
        let header = ["Port", "Model", "SDK id", "USB serial", "Device"].map(str::to_string);
        let rows: Vec<[String; 5]> = std::iter::once(header)
            .chain(listing.rows.iter().map(|row| {
                [
                    row.port.clone(),
                    row.model.clone(),
                    row.sdk_id.clone().unwrap_or_else(|| ABSENT.to_string()),
                    row.usb_serial.clone().unwrap_or_else(|| ABSENT.to_string()),
                    row.device.clone(),
                ]
            }))
            .collect();
        let widths: Vec<usize> = (0..5)
            .map(|column| {
                rows.iter()
                    .map(|row| row.get(column).map_or(0, |cell| cell.chars().count()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for row in &rows {
            let mut line = String::from(" ");
            for (cell, width) in row.iter().zip(&widths) {
                line.push(' ');
                line.push_str(cell);
                let pad = width.saturating_sub(cell.chars().count());
                line.push_str(&" ".repeat(pad.saturating_add(1)));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    for note in listing.notes.iter().chain(&listing.paste_notes) {
        out.push('\n');
        out.push_str(note);
        out.push('\n');
    }
    if let Some(block) = &listing.block {
        let _ = write!(
            out,
            "\nTo pin the cameras this driver serves, paste this into {}.json.\n\
             Leave out any camera another application owns (PHD2's guide camera), and\n\
             keep the numbers running 0..N-1",
            listing.service
        );
        if listing.wheels {
            // One line for the first sentence, so it reads whole wherever it
            // is searched for.
            out.push_str(
                ".\n--devices opens no camera, so it cannot see a filter wheel:\n\
                 add \"filter_wheel_number\" to the entry of each camera that has one,\n\
                 numbering the wheels 0..M-1",
            );
        }
        out.push_str(":\n\n");
        out.push_str(&render_block(block));
    }
    out
}

/// What `<service> doctor --devices` prints, and its exit code.
///
/// The cameras by port, the block to paste and, closing it with a list,
/// `usb-devices.resolve`'s verdicts on the configured entries, exit 0 — or why
/// there is no listing, exit 1: the scan failed, or the SDK could not
/// enumerate. `list` is the configured
/// `usb_devices` (`None` without one), or the rule it breaks. A list in
/// `config_path` that breaks a rule is said first whatever else fails, so a
/// host fault fixed first does not hide it; with one the block and the checks
/// are withheld, since they would only reproduce or judge the list the
/// service refuses, and the exit code is 1.
///
/// `enumerate` runs only once the scan has, and returns the SDK's cameras as
/// the join takes them beside whatever `listing` builds the listing from.
pub fn devices_report<I>(
    normalizer: Normalizer<'_>,
    service: &str,
    config_path: &std::path::Path,
    list: Result<Option<&[BlockEntry]>, String>,
    scan: Result<UsbScan, String>,
    enumerate: impl FnOnce() -> Result<(Vec<SdkCamera>, I), String>,
    listing: impl FnOnce(&Claims<'_>, &I) -> Listing,
) -> (String, i32) {
    let (list, list_note) = match list {
        Ok(list) => (list, None),
        Err(error) => (None, Some(error)),
    };
    let list_note = list_note.map(|error| {
        format!(
            "The usb_devices list in {} breaks a rule, so the service refuses to start: \
             {error}.",
            config_path.display()
        )
    });
    let with_list_note = |text: String| match &list_note {
        Some(note) => format!("{note}\n\n{text}"),
        None => text,
    };
    if let Err(error) = &scan {
        return (
            with_list_note(format!(
                "{} cameras on the bus (vendor {}):\n\n  the USB scan failed: {error}\n",
                normalizer.sdk, normalizer.vendor
            )),
            1,
        );
    }
    let (cameras, facts) = match enumerate() {
        Ok(enumerated) => enumerated,
        Err(error) => {
            return (
                with_list_note(format!(
                    "{} SDK enumeration failed: {error}\n",
                    normalizer.sdk
                )),
                1,
            )
        }
    };
    let claims = Claims::new(normalizer, service, scan, cameras);
    let mut listing = listing(&claims, &facts);
    let Some(note) = list_note else {
        // The listing shows the placed cameras, the block, and what the join
        // could not place. What it does not show is which listed numbers a
        // placeholder would hold: `usb-devices.resolve`'s verdicts, which
        // close it. A failed scan, the one thing that fails them, returned
        // above.
        let resolve: Vec<Check> = list.map_or_default(|list| claims.resolve_checks(list));
        if resolve.is_empty() {
            return (render_listing(&listing), 0);
        }
        return (
            format!("{}\n{}", render_listing(&listing), render_checks(&resolve)),
            0,
        );
    };
    // A block reproducing a list the service refuses would only be pasted
    // back; the ports listed are what the operator fixes it from.
    listing.block = None;
    listing
        .notes
        .insert(0, format!("{note} Fix it from the ports above."));
    (render_listing(&listing), 1)
}

/// The `"usb_devices": [...]` member, one entry per line, indented to paste.
fn render_block(block: &[BlockEntry]) -> String {
    if block.is_empty() {
        return "    \"usb_devices\": []\n".to_string();
    }
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"));
    let entries: Vec<String> = block
        .iter()
        .map(|entry| {
            let mut fields = vec![
                format!("\"device_number\": {}", entry.device_number),
                format!("\"usb_port\": {}", quote(&entry.usb_port)),
            ];
            if let Some(name) = &entry.name {
                fields.push(format!("\"name\": {}", quote(name)));
            }
            if let Some(description) = &entry.description {
                fields.push(format!("\"description\": {}", quote(description)));
            }
            if let Some(number) = entry.filter_wheel_number {
                fields.push(format!("\"filter_wheel_number\": {number}"));
            }
            if let Some(names) = &entry.filter_names {
                let names: Vec<String> = names.iter().map(|n| quote(n)).collect();
                fields.push(format!("\"filter_names\": [{}]", names.join(", ")));
            }
            format!("      {{ {} }}", fields.join(", "))
        })
        .collect();
    format!("    \"usb_devices\": [\n{}\n    ]\n", entries.join(",\n"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::facts::UsbFault;

    const SVB: Normalizer<'static> = Normalizer {
        sdk: "SVBony",
        vendor: "f266",
        models: &[("SV605CC", "9a0a"), ("SV705C", "9a0b")],
        not_cameras: &["1001"],
        sdk_check: None,
    };

    fn record(product: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "f266".to_string(),
            product: product.to_string(),
            model: Some(format!("SVBONY {product}")),
            port: Some(port.to_string()),
            serial: None,
        }
    }

    fn with_serial(mut device: UsbDevice, serial: &str) -> UsbDevice {
        device.serial = Some(serial.to_string());
        device
    }

    #[test]
    fn camera_records_are_the_vendors_records_that_can_be_cameras() {
        let mut other_vendor = record("9a0a", "p:4");
        other_vendor.vendor = "03c3".to_string();
        let scan = UsbScan {
            devices: vec![
                record("9a0a", "p:1"),
                record("1001", "p:2"),
                record("77aa", "p:3"),
                other_vendor,
            ],
            faults: Vec::new(),
        };
        let ports: Vec<_> = SVB
            .camera_records(&scan)
            .into_iter()
            .filter_map(|r| r.port.as_deref())
            .collect();
        // A known non-camera and another vendor's record are not; an
        // unassigned product id with no SDK check could be.
        assert_eq!(ports, vec!["p:1", "p:3"]);
    }

    fn camera(model: &str) -> SdkCamera {
        SdkCamera {
            model: model.to_string(),
            serial: None,
        }
    }

    fn camera_with_serial(model: &str, serial: &str) -> SdkCamera {
        SdkCamera {
            model: model.to_string(),
            serial: Some(serial.to_string()),
        }
    }

    fn scan(devices: Vec<UsbDevice>) -> UsbScan {
        UsbScan {
            devices,
            faults: Vec::new(),
        }
    }

    fn fault(vendor: Option<&str>, location: Option<&str>) -> UsbFault {
        UsbFault {
            record: "2-1".to_string(),
            vendor: vendor.map(str::to_string),
            product: None,
            model: None,
            location: location.map(str::to_string),
            reason: "its idProduct could not be read".to_string(),
        }
    }

    fn port(p: &str) -> CameraPlace {
        CameraPlace::Port(p.to_string())
    }

    fn views(entries: &[(u32, &'static str)]) -> Vec<EntryView<'static>> {
        entries
            .iter()
            .map(|&(device_number, usb_port)| EntryView {
                device_number,
                usb_port,
            })
            .collect()
    }

    fn placeholder_reason(resolution: Resolution) -> String {
        match resolution {
            Resolution::Placeholder(reason) => reason,
            Resolution::Camera(index) => panic!("expected a placeholder, got camera {index}"),
        }
    }

    // --- the wire contract ---------------------------------------------------

    #[test]
    fn the_placeholder_error_code_is_driver_code_0x40() {
        assert_eq!(PLACEHOLDER_DRIVER_CODE, 0x40);
        assert_eq!(PLACEHOLDER_ERROR_CODE, 0x500 + PLACEHOLDER_DRIVER_CODE);
    }

    #[test]
    fn a_placeholder_with_no_name_of_its_own_names_the_sdk_and_the_port() {
        assert_eq!(
            placeholder_name("ZWO", "pci-0000:00:14.0-usbv3-0:4.2"),
            "ZWO camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder)"
        );
    }

    #[test]
    fn a_placeholder_unique_id_names_the_service_and_the_port() {
        let id = placeholder_unique_id("svbony-camera", "pci-0000:00:14.0-usbv3-0:4.2");
        assert_eq!(id, "placeholder:svbony-camera:pci-0000:00:14.0-usbv3-0:4.2");
        assert!(is_placeholder_unique_id(&id));
    }

    #[test]
    fn a_camera_unique_id_is_not_a_placeholder_one() {
        assert!(!is_placeholder_unique_id(
            "SVBONY:SVBONY-SV605CC:SVB0123456789AB"
        ));
    }

    // --- the list ------------------------------------------------------------

    #[test]
    fn a_dense_list_of_distinct_ports_is_valid() {
        assert_eq!(validate_list(&views(&[(1, "b"), (0, "a")])), Vec::new());
    }

    #[test]
    fn an_empty_list_is_valid_in_a_file() {
        assert_eq!(validate_list(&[]), Vec::new());
    }

    #[test]
    fn a_gap_in_the_numbers_names_the_missing_number() {
        let errors = validate_list(&views(&[(0, "a"), (2, "b")]));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices");
        assert!(
            errors[0]
                .to_string()
                .starts_with("usb_devices: device numbers must run 0..N-1, and 1 is missing"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn several_missing_numbers_are_named_together() {
        let errors = validate_list(&views(&[(3, "a"), (4, "b")]));
        assert!(
            errors[0].message.contains("0, 1 are missing"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn a_repeated_device_number_names_the_entry_and_the_first_holder() {
        let errors = validate_list(&views(&[(0, "a"), (0, "b")]));
        let repeat = errors
            .iter()
            .find(|e| e.path == "usb_devices.1.device_number")
            .unwrap();
        assert_eq!(
            repeat.to_string(),
            "usb_devices[1]: device_number 0 is also usb_devices[0]'s"
        );
    }

    #[test]
    fn a_repeated_port_names_the_entry_and_the_first_holder() {
        let errors = validate_list(&views(&[(0, "a"), (1, "a")]));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices.1.usb_port");
        assert_eq!(
            errors[0].to_string(),
            "usb_devices[1]: usb_port a is also usb_devices[0]'s"
        );
    }

    #[test]
    fn a_blank_port_is_rejected() {
        let errors = validate_list(&views(&[(0, " ")]));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].to_string(), "usb_devices[0]: usb_port is blank");
    }

    #[test]
    fn a_padded_port_is_rejected_not_trimmed() {
        let errors = validate_list(&views(&[(0, " a")]));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices.0.usb_port");
        assert!(
            errors[0]
                .to_string()
                .starts_with("usb_devices[0]: usb_port has leading or trailing whitespace"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn an_override_beside_a_list_names_its_key() {
        let error = override_beside_list("SVB0123456789AB");
        assert_eq!(error.path, "devices.SVB0123456789AB");
        assert!(error
            .to_string()
            .starts_with("devices.SVB0123456789AB: move its fields into the usb_devices entry"));
    }

    // --- the join ------------------------------------------------------------

    #[test]
    fn a_camera_is_placed_on_the_one_record_of_its_product_id() {
        let placement = place(
            &SVB,
            &[camera("SV605CC")],
            &scan(vec![record("9a0a", "p1")]),
        );
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn two_models_are_told_apart_by_their_product_ids() {
        let placement = place(
            &SVB,
            &[camera("SV705C"), camera("SV605CC")],
            &scan(vec![record("9a0a", "p1"), record("9a0b", "p2")]),
        );
        assert_eq!(placement.cameras, vec![port("p2"), port("p1")]);
    }

    #[test]
    fn another_vendors_device_is_never_a_camera_record() {
        let mut other = record("9a0a", "p2");
        other.vendor = "0403".to_string();
        let placement = place(
            &SVB,
            &[camera("SV605CC")],
            &scan(vec![other, record("9a0a", "p1")]),
        );
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn one_camera_beside_two_identical_records_is_a_look_alike() {
        let placement = place(
            &SVB,
            &[camera("SV605CC")],
            &scan(vec![record("9a0a", "p2"), record("9a0a", "p1")]),
        );
        assert_eq!(
            placement.cameras,
            vec![CameraPlace::LookAlike {
                ports: vec!["p1".to_string(), "p2".to_string()],
                rivals: Vec::new(),
            }]
        );
    }

    #[test]
    fn two_identical_cameras_beside_one_record_are_both_look_alikes() {
        let placement = place(
            &SVB,
            &[camera("SV605CC"), camera("SV605CC")],
            &scan(vec![record("9a0a", "p1")]),
        );
        assert_eq!(
            placement.cameras,
            vec![
                CameraPlace::LookAlike {
                    ports: vec!["p1".to_string()],
                    rivals: vec![1],
                },
                CameraPlace::LookAlike {
                    ports: vec!["p1".to_string()],
                    rivals: vec![0],
                },
            ]
        );
    }

    #[test]
    fn two_identical_cameras_on_two_records_are_look_alikes() {
        let placement = place(
            &SVB,
            &[camera("SV605CC"), camera("SV605CC")],
            &scan(vec![record("9a0a", "p1"), record("9a0a", "p2")]),
        );
        assert!(
            placement
                .cameras
                .iter()
                .all(|p| matches!(p, CameraPlace::LookAlike { .. })),
            "{:?}",
            placement.cameras
        );
    }

    #[test]
    fn serials_on_both_sides_tell_identical_cameras_apart() {
        let placement = place(
            &SVB,
            &[
                camera_with_serial("SV605CC", "B"),
                camera_with_serial("SV605CC", "A"),
            ],
            &scan(vec![
                with_serial(record("9a0a", "p1"), "A"),
                with_serial(record("9a0a", "p2"), "B"),
            ]),
        );
        assert_eq!(placement.cameras, vec![port("p2"), port("p1")]);
    }

    #[test]
    fn a_serial_on_both_sides_that_differs_is_no_match() {
        let placement = place(
            &SVB,
            &[camera_with_serial("SV605CC", "A")],
            &scan(vec![with_serial(record("9a0a", "p1"), "B")]),
        );
        assert_eq!(placement.cameras, vec![CameraPlace::NoRecord]);
    }

    #[test]
    fn a_serial_on_one_side_only_falls_back_to_the_product_id() {
        let placement = place(
            &SVB,
            &[camera_with_serial("SV605CC", "A")],
            &scan(vec![record("9a0a", "p1")]),
        );
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn an_unknown_model_is_paired_by_elimination() {
        let placement = place(&SVB, &[camera("SV905C")], &scan(vec![record("9a0f", "p1")]));
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn a_known_model_is_never_paired_by_elimination() {
        // Its own record (9a0a) is missing; the one left over is some other
        // device of this vendor.
        let placement = place(
            &SVB,
            &[camera("SV605CC")],
            &scan(vec![record("9a0f", "p2")]),
        );
        assert_eq!(placement.cameras, vec![CameraPlace::NoRecord]);
    }

    #[test]
    fn elimination_never_takes_a_record_the_normalizer_gives_another_model() {
        let placement = place(&SVB, &[camera("SV905C")], &scan(vec![record("9a0a", "p1")]));
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn elimination_waits_out_a_fault_of_this_vendor() {
        let mut bus = scan(vec![record("9a0f", "p1")]);
        bus.faults.push(fault(Some("f266"), None));
        let placement = place(&SVB, &[camera("SV905C")], &bus);
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn elimination_waits_out_a_fault_with_no_readable_vendor() {
        let mut bus = scan(vec![record("9a0f", "p1")]);
        bus.faults.push(fault(None, None));
        let placement = place(&SVB, &[camera("SV905C")], &bus);
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn elimination_waits_out_a_windows_enumeration_failure() {
        let mut bus = scan(vec![record("9a0f", "p1")]);
        bus.faults
            .push(fault(Some("0000"), Some("ACPI(_SB_)#ACPI(HS05)")));
        let placement = place(&SVB, &[camera("SV905C")], &bus);
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn elimination_ignores_another_vendors_fault() {
        let mut bus = scan(vec![record("9a0f", "p1")]);
        bus.faults.push(fault(Some("0403"), None));
        let placement = place(&SVB, &[camera("SV905C")], &bus);
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn elimination_needs_exactly_one_of_each_left_over() {
        let placement = place(
            &SVB,
            &[camera("SV905C")],
            &scan(vec![record("9a0f", "p1"), record("9a0e", "p2")]),
        );
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn a_known_non_camera_takes_no_part_in_elimination() {
        let placement = place(
            &SVB,
            &[camera("SV905C")],
            &scan(vec![record("9a0f", "p1"), record("1001", "p2")]),
        );
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn a_known_model_with_no_working_record_has_no_record() {
        let placement = place(&SVB, &[camera("SV605CC")], &scan(Vec::new()));
        assert_eq!(placement.cameras, vec![CameraPlace::NoRecord]);
    }

    // --- the SDK's own check ---------------------------------------------------

    /// The ASI SDK's table as SDK 1.41 answered it on the dev box: cameras
    /// 1603, 178c and 662b, and not the EAF's 1f10.
    fn asi_check(product: &str) -> bool {
        matches!(product, "1603" | "178c" | "662b")
    }

    const ZWO: Normalizer<'static> = Normalizer {
        sdk: "ZWO",
        vendor: "03c3",
        models: &[("ZWO ASI178MM", "178c")],
        not_cameras: &[],
        sdk_check: Some(asi_check),
    };

    fn zwo_record(product: &str, port: &str) -> UsbDevice {
        UsbDevice {
            vendor: "03c3".to_string(),
            ..record(product, port)
        }
    }

    #[test]
    fn a_record_the_sdk_check_rejects_is_never_a_candidate() {
        // The EAF beside an unknown camera leaves exactly one candidate, so
        // elimination still pairs the camera with its own record.
        let placement = place(
            &ZWO,
            &[camera("ZWO ASI662MC")],
            &scan(vec![zwo_record("1f10", "p1"), zwo_record("662b", "p2")]),
        );
        assert_eq!(placement.cameras, vec![port("p2")]);
    }

    #[test]
    fn without_the_sdk_check_a_focuser_blocks_elimination() {
        let placement = place(
            &Normalizer {
                sdk_check: None,
                ..ZWO
            },
            &[camera("ZWO ASI662MC")],
            &scan(vec![zwo_record("1f10", "p1"), zwo_record("662b", "p2")]),
        );
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn an_observed_models_product_counts_whatever_the_sdk_check_says() {
        let placement = place(
            &Normalizer {
                sdk_check: Some(|_| false),
                ..ZWO
            },
            &[camera("ZWO ASI178MM")],
            &scan(vec![zwo_record("178c", "p1")]),
        );
        assert_eq!(placement.cameras, vec![port("p1")]);
    }

    #[test]
    fn a_fault_of_this_vendor_the_sdk_check_rejects_is_not_the_camera() {
        // A faulted focuser record does not stand between the unknown camera
        // and its record.
        let mut bus = scan(vec![zwo_record("662b", "p2")]);
        let mut focuser = fault(Some("03c3"), Some("p1"));
        focuser.product = Some("1f10".to_string());
        bus.faults.push(focuser);
        let placement = place(&ZWO, &[camera("ZWO ASI662MC")], &bus);
        assert_eq!(placement.cameras, vec![port("p2")]);
    }

    #[test]
    fn a_fault_of_this_vendor_with_no_product_id_could_be_the_camera() {
        let mut bus = scan(vec![zwo_record("662b", "p2")]);
        bus.faults.push(fault(Some("03c3"), Some("p1")));
        let placement = place(&ZWO, &[camera("ZWO ASI662MC")], &bus);
        assert_eq!(placement.cameras, vec![CameraPlace::Unrecognised]);
    }

    #[test]
    fn a_focuser_on_a_listed_port_is_a_device_the_sdk_does_not_report_as_a_camera() {
        let mut focuser = zwo_record("1f10", "p1");
        focuser.model = Some("ZWO EAF".to_string());
        let claims = Claims::new(ZWO, "zwo-camera", Ok(scan(vec![focuser])), Vec::new());
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.starts_with(
                "p1 holds ZWO EAF (03c3:1f10), which the ZWO SDK does not report as a camera."
            ),
            "{reason}"
        );
        assert_eq!(claims.unplaced_notes(), Vec::<String>::new());
    }

    // --- resolving a listed port ---------------------------------------------

    fn claims(scan: Result<UsbScan, String>, sdk: Vec<SdkCamera>) -> Claims<'static> {
        Claims::new(SVB, "svbony-camera", scan, sdk)
    }

    #[test]
    fn a_port_holding_a_placed_camera_resolves_to_it() {
        let claims = claims(
            Ok(scan(vec![record("9a0a", "p1")])),
            vec![camera("SV605CC")],
        );
        assert_eq!(claims.resolve("p1"), Resolution::Camera(0));
    }

    #[test]
    fn every_reason_ends_with_the_way_back() {
        let reason = placeholder_reason(claims(Ok(scan(Vec::new())), Vec::new()).resolve("p1"));
        assert!(
            reason.ends_with(
                "Once the camera is fixed, reload or restart svbony-camera, which re-opens \
                 every camera it serves"
            ),
            "{reason}"
        );
    }

    #[test]
    fn a_failed_scan_holds_every_port_with_its_error() {
        let claims = claims(Err("powershell.exe timed out".to_string()), Vec::new());
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.starts_with("the USB scan failed: powershell.exe timed out."),
            "{reason}"
        );
        assert!(
            reason.contains("re-scans on its own") && !reason.contains("reload or restart"),
            "a failed scan heals itself, so its reason sends nobody to restart: {reason}"
        );
        assert_eq!(claims.placement(), None);
        assert_eq!(claims.scan_error(), Some("powershell.exe timed out"));
    }

    #[test]
    fn a_fault_at_the_port_gives_its_own_reason() {
        let mut bus = scan(Vec::new());
        bus.faults.push(fault(Some("f266"), Some("p1")));
        let reason = placeholder_reason(claims(Ok(bus), vec![camera("SV605CC")]).resolve("p1"));
        assert!(
            reason.starts_with(
                "p1 holds a USB record that is not a working device (2-1): its idProduct \
                 could not be read"
            ),
            "{reason}"
        );
    }

    #[test]
    fn a_look_alike_on_the_port_says_the_cameras_cannot_be_told_apart() {
        let claims = claims(
            Ok(scan(vec![record("9a0a", "p1"), record("9a0a", "p2")])),
            vec![camera("SV605CC")],
        );
        for listed in ["p1", "p2"] {
            let reason = placeholder_reason(claims.resolve(listed));
            assert!(reason.contains("cannot be told apart"), "{reason}");
            assert!(reason.contains("p1, p2"), "{reason}");
        }
    }

    #[test]
    fn a_record_the_sdk_does_not_report_is_named_on_the_port() {
        let reason = placeholder_reason(
            claims(Ok(scan(vec![record("9a0a", "p1")])), Vec::new()).resolve("p1"),
        );
        assert!(
            reason.starts_with(
                "p1 holds SVBONY 9a0a (f266:9a0a), which the SVBony SDK does not report as a \
                 camera."
            ),
            "{reason}"
        );
    }

    #[test]
    fn another_vendors_device_on_the_port_is_named_on_the_port() {
        let mut other = record("6015", "p1");
        other.vendor = "0403".to_string();
        other.model = Some("UPBv2 revA".to_string());
        let reason = placeholder_reason(
            claims(Ok(scan(vec![other])), vec![camera("SV605CC")]).resolve("p1"),
        );
        assert!(
            reason.starts_with(
                "p1 holds UPBv2 revA (0403:6015), which the SVBony SDK does not report as a \
                 camera."
            ),
            "{reason}"
        );
    }

    #[test]
    fn an_unrecognised_camera_beside_an_unclaimed_record_names_the_model() {
        // Two unclaimed records, so elimination cannot pair either.
        let claims = claims(
            Ok(scan(vec![record("9a0f", "p1"), record("9a0e", "p2")])),
            vec![camera("SV905C")],
        );
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.contains("this driver knows no product id for (SV905C)"),
            "{reason}"
        );
    }

    #[test]
    fn every_unrecognised_model_is_named() {
        let claims = claims(
            Ok(scan(vec![record("9a0f", "p1"), record("9a0e", "p2")])),
            vec![camera("SV705X"), camera("SV905C")],
        );
        for listed in ["p1", "p2"] {
            let reason = placeholder_reason(claims.resolve(listed));
            assert!(
                reason.contains("needs an entry for each of SV705X, SV905C"),
                "{reason}"
            );
        }
    }

    #[test]
    fn an_unrecognised_model_is_named_once_however_many_cameras_it_has() {
        let claims = claims(
            Ok(scan(vec![record("9a0f", "p1"), record("9a0e", "p2")])),
            vec![camera("SV905C"), camera("SV905C")],
        );
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.contains("needs an entry for each of SV905C."),
            "{reason}"
        );
    }

    #[test]
    fn a_known_models_record_beside_an_unrecognised_camera_names_both_causes() {
        // 9a0a is the SV605CC's: either the SDK does not see it, or the SDK's
        // SV905C is that camera under a name the normalizer does not know.
        let claims = claims(Ok(scan(vec![record("9a0a", "p1")])), vec![camera("SV905C")]);
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.starts_with(
                "p1 holds SVBONY 9a0a (f266:9a0a), a model this driver knows, but the SVBony \
                 SDK reports none of that model"
            ),
            "{reason}"
        );
        assert!(reason.contains("(SV905C)"), "{reason}");
        assert!(reason.contains("normalizer needs that name"), "{reason}");
    }

    #[test]
    fn an_empty_port_says_no_working_camera_is_enumerated_there() {
        let reason = placeholder_reason(claims(Ok(scan(Vec::new())), Vec::new()).resolve("p9"));
        assert!(
            reason.starts_with("no working camera is enumerated on p9."),
            "{reason}"
        );
    }

    #[test]
    fn an_empty_port_mentions_faults_elsewhere_that_could_be_the_camera() {
        let mut bus = scan(Vec::new());
        bus.faults.push(fault(None, Some("ACPI(_SB_)#ACPI(HS05)")));
        let reason = placeholder_reason(claims(Ok(bus), Vec::new()).resolve("p9"));
        assert!(
            reason.starts_with(
                "no working camera is enumerated on p9; the scan also found 1 record(s)"
            ),
            "{reason}"
        );
        assert!(reason.contains("hardware.usb-fault"), "{reason}");
    }

    #[test]
    fn a_failed_enumeration_or_this_vendors_fault_could_be_the_camera() {
        let mut bus = scan(Vec::new());
        bus.faults
            .push(fault(Some("0000"), Some("ACPI(_SB_)#ACPI(HS05)")));
        bus.faults.push(fault(Some("f266"), Some("p3")));
        let reason = placeholder_reason(claims(Ok(bus), Vec::new()).resolve("p9"));
        assert!(
            reason.contains("the scan also found 2 record(s)"),
            "{reason}"
        );
    }

    #[test]
    fn another_vendors_fault_is_not_offered_as_the_camera() {
        let mut bus = scan(Vec::new());
        bus.faults.push(fault(Some("046d"), Some("1-3")));
        let reason = placeholder_reason(claims(Ok(bus), Vec::new()).resolve("p9"));
        assert!(
            reason.starts_with("no working camera is enumerated on p9."),
            "{reason}"
        );
    }

    #[test]
    fn an_open_failure_names_the_port_and_the_sdk_error_and_the_way_back() {
        let claims = claims(Ok(scan(Vec::new())), Vec::new());
        let reason = claims.open_failure("p1", "general error");
        assert_eq!(
            reason,
            "the SVBony SDK could not open the camera placed on p1: general error. Once the \
             camera is fixed, reload or restart svbony-camera, which re-opens every camera it \
             serves"
        );
    }

    #[test]
    fn placed_cameras_are_listed_in_port_order() {
        let claims = claims(
            Ok(scan(vec![
                record("9a0a", "1-4.10"),
                record("9a0b", "1-4.2"),
            ])),
            vec![camera("SV605CC"), camera("SV705C")],
        );
        assert_eq!(claims.placed(), vec![(1, "1-4.2"), (0, "1-4.10")]);
    }

    #[test]
    fn unplaced_notes_name_look_alikes_stray_records_and_faults() {
        let mut bus = scan(vec![
            record("9a0a", "p1"),
            record("9a0a", "p2"),
            record("9a0f", "p4"),
        ]);
        bus.faults.push(fault(Some("f266"), Some("p3")));
        let notes = claims(Ok(bus), vec![camera("SV605CC")]).unplaced_notes();
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("cannot be told apart"), "{notes:?}");
        assert!(notes[1].starts_with("p4 holds"), "{notes:?}");
        assert!(notes[2].contains("is not a working device"), "{notes:?}");
    }

    #[test]
    fn unplaced_notes_list_a_fault_that_could_be_the_camera() {
        let mut bus = scan(Vec::new());
        bus.faults
            .push(fault(Some("0000"), Some("ACPI(_SB_)#ACPI(HS05)")));
        bus.faults.push(fault(Some("046d"), Some("1-3")));
        let notes = claims(Ok(bus), vec![camera("SV605CC")]).unplaced_notes();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes[0].contains("may be one of the faults below"),
            "{notes:?}"
        );
        assert!(notes[1].contains("ACPI(_SB_)#ACPI(HS05)"), "{notes:?}");
    }

    #[test]
    fn a_look_alikes_records_are_not_named_twice() {
        let bus = scan(vec![record("9a0a", "p1"), record("9a0a", "p2")]);
        let notes = claims(Ok(bus), vec![camera("SV605CC")]).unplaced_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    // --- port order ----------------------------------------------------------

    #[test]
    fn a_linux_port_chain_compares_each_hop_as_a_number() {
        assert_eq!(
            compare_ports(
                "pci-0000:00:14.0-usbv3-0:4.2",
                "pci-0000:00:14.0-usbv3-0:4.10"
            ),
            Ordering::Less
        );
    }

    #[test]
    fn a_windows_location_path_compares_usb_hops_as_numbers() {
        assert_eq!(
            compare_ports(
                "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)",
                "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)"
            ),
            Ordering::Less
        );
    }

    #[test]
    fn fixed_width_hex_compares_in_hex_order() {
        assert_eq!(
            compare_ports("PCIROOT(0)#PCI(0d00)", "PCIROOT(0)#PCI(1400)"),
            Ordering::Less
        );
        assert_eq!(compare_ports("0x14200000", "0x14a00000"), Ordering::Less);
    }

    #[test]
    fn a_hex_field_that_is_all_digits_still_compares_in_hex_order() {
        assert_eq!(compare_ports("X(00a0)", "X(0100)"), Ordering::Less);
        assert_eq!(compare_ports("PCI(00A0)", "PCI(0100)"), Ordering::Less);
    }

    #[test]
    fn a_port_precedes_the_ports_behind_it() {
        assert_eq!(compare_ports("1-4", "1-4.1"), Ordering::Less);
    }

    #[test]
    fn port_order_stays_consistent_when_decimal_and_mixed_runs_meet() {
        // Once a cycle: 9 < 10 (numbers), 10 < 1a and 1a < 9 (text).
        assert_eq!(compare_ports("p-9", "p-10"), Ordering::Less);
        assert_eq!(compare_ports("p-10", "p-1a"), Ordering::Less);
        assert_eq!(compare_ports("p-9", "p-1a"), Ordering::Less);
        let mut ports = vec!["p-1a", "p-10", "p-9", "p-a", "p-09", "p-0d00", "p-1400"];
        ports.sort_by(|a, b| compare_ports(a, b));
        assert_eq!(
            ports,
            vec!["p-09", "p-9", "p-a", "p-10", "p-1a", "p-0d00", "p-1400"]
        );
    }

    #[test]
    fn port_order_is_total() {
        assert_eq!(compare_ports("1-04", "1-4"), Ordering::Less);
        assert_eq!(compare_ports("1-4", "1-4"), Ordering::Equal);
    }

    // --- the listing ---------------------------------------------------------

    fn listing(rows: Vec<ListingRow>, block: Option<Vec<BlockEntry>>) -> Listing {
        Listing {
            sdk: "SVBony".to_string(),
            vendor: "f266".to_string(),
            service: "svbony-camera".to_string(),
            rows,
            block,
            notes: Vec::new(),
            paste_notes: Vec::new(),
            wheels: false,
        }
    }

    fn row(port: &str, device: &str) -> ListingRow {
        ListingRow {
            port: port.to_string(),
            model: "SV605CC".to_string(),
            sdk_id: Some("0123481353808C03EE2512150035".to_string()),
            usb_serial: None,
            device: device.to_string(),
        }
    }

    fn entry(device_number: u32, usb_port: &str, name: Option<&str>) -> BlockEntry {
        BlockEntry {
            device_number,
            usb_port: usb_port.to_string(),
            name: name.map(str::to_string),
            ..BlockEntry::default()
        }
    }

    #[test]
    fn the_listing_aligns_its_columns_and_marks_what_is_absent() {
        let text = render_listing(&listing(
            vec![
                row("platform-xhci-hcd.1-usbv3-0:1", "0"),
                row("p2", "not listed"),
            ],
            None,
        ));
        let table: Vec<&str> = text.lines().skip(2).take(3).collect();
        assert_eq!(
            table,
            vec![
                "  Port                           Model    SDK id                        USB serial  Device",
                "  platform-xhci-hcd.1-usbv3-0:1  SV605CC  0123481353808C03EE2512150035  —           0",
                "  p2                             SV605CC  0123481353808C03EE2512150035  —           not listed",
            ]
        );
    }

    #[test]
    fn the_notes_on_pasting_follow_the_notes_on_the_rows_and_precede_the_block() {
        let mut with_notes = listing(Vec::new(), Some(vec![entry(0, "p1", None)]));
        with_notes.notes = vec!["On the rows.".to_string()];
        with_notes.paste_notes = vec!["On pasting.".to_string()];
        let text = render_listing(&with_notes);
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {text}"))
        };
        assert!(
            at("On the rows.") < at("On pasting.") && at("On pasting.") < at("\"usb_devices\""),
            "{text}"
        );
    }

    #[test]
    fn a_listing_with_no_camera_prints_no_table() {
        let text = render_listing(&listing(Vec::new(), None));
        assert!(
            text.contains("No SVBony camera is placed on a port."),
            "{text}"
        );
        assert!(!text.contains("Port "), "{text}");
    }

    #[test]
    fn the_block_pastes_as_a_usb_devices_member() {
        let text = render_listing(&listing(
            Vec::new(),
            Some(vec![entry(0, "p1", None), entry(1, "p\"2", Some("Guide"))]),
        ));
        let block = text
            .split_once("    \"usb_devices\"")
            .map(|(_, rest)| format!("{{\"usb_devices\"{rest}}}"))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(
            parsed["usb_devices"],
            serde_json::json!([
                { "device_number": 0, "usb_port": "p1" },
                { "device_number": 1, "usb_port": "p\"2", "name": "Guide" }
            ])
        );
    }

    #[test]
    fn an_empty_block_pastes_as_an_empty_list() {
        let text = render_listing(&listing(Vec::new(), Some(Vec::new())));
        assert!(text.ends_with("    \"usb_devices\": []\n"), "{text}");
    }

    #[test]
    fn a_declared_wheel_pastes_with_its_number_and_names() {
        let mut wheel = entry(0, "p1", None);
        wheel.filter_wheel_number = Some(0);
        wheel.filter_names = Some(vec!["L".to_string(), "H\"a".to_string()]);
        let text = render_listing(&listing(Vec::new(), Some(vec![wheel])));
        let block = text
            .split_once("    \"usb_devices\"")
            .map(|(_, rest)| format!("{{\"usb_devices\"{rest}}}"))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&block).unwrap();
        assert_eq!(
            parsed["usb_devices"],
            serde_json::json!([{
                "device_number": 0,
                "usb_port": "p1",
                "filter_wheel_number": 0,
                "filter_names": ["L", "H\"a"]
            }])
        );
    }

    #[test]
    fn a_driver_with_wheels_says_above_the_block_how_to_declare_one() {
        let mut with_wheels = listing(Vec::new(), Some(vec![entry(0, "p1", None)]));
        with_wheels.wheels = true;
        let text = render_listing(&with_wheels);
        assert!(
            text.contains("--devices opens no camera, so it cannot see a filter wheel:\n"),
            "{text}"
        );
        assert!(text.contains("numbering the wheels 0..M-1:\n"), "{text}");
        let without = render_listing(&listing(Vec::new(), Some(vec![entry(0, "p1", None)])));
        assert!(!without.contains("filter wheel"), "{without}");
        assert!(
            without.contains("keep the numbers running 0..N-1:\n"),
            "{without}"
        );
    }

    // --- the entry and its glue ---------------------------------------------

    fn usb_entry(device_number: u32, usb_port: &str) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number,
            usb_port: usb_port.to_string(),
            name: None,
            description: None,
        }
    }

    #[test]
    fn without_a_list_nothing_is_checked_and_an_override_is_fine() {
        let keys = ["SN1".to_string()];
        assert_eq!(list_errors::<UsbDeviceEntry>(None, &keys), Vec::new());
    }

    #[test]
    fn a_list_is_checked_by_the_shared_rules_and_refuses_every_override_beside_it() {
        let keys = ["SN1".to_string(), "SN2".to_string()];
        let errors = list_errors(Some(&[usb_entry(0, "a"), usb_entry(2, "b")][..]), &keys);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["usb_devices", "devices.SN1", "devices.SN2"]);
    }

    #[test]
    fn check_list_names_every_rule_broken_in_one_message() {
        let errors = list_errors(Some(&[usb_entry(0, " a"), usb_entry(0, "b")][..]), &[]);
        let message = check_list(&errors).unwrap_err();
        assert!(
            message.contains("usb_devices[0]: usb_port has leading or trailing whitespace"),
            "{message}"
        );
        assert!(
            message.contains("; usb_devices[1]: device_number 0 is also usb_devices[0]'s"),
            "{message}"
        );
        assert_eq!(check_list(&[]), Ok(()));
    }

    #[test]
    fn an_entry_reads_its_optional_names_and_refuses_an_unknown_key() {
        let entry: UsbDeviceEntry =
            serde_json::from_str(r#"{"device_number": 1, "usb_port": "a", "name": "Main"}"#)
                .unwrap();
        assert_eq!(entry.name.as_deref(), Some("Main"));
        assert_eq!(entry.description, None);
        let error = serde_json::from_str::<UsbDeviceEntry>(
            r#"{"device_number": 0, "usb_port": "a", "serial": "x"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("serial"), "{error}");
    }

    const WHEELS: Numbering<'static> = Numbering {
        field: "wheel",
        numbers: "wheel numbers",
        bound: "M",
        renumber: "wheels",
    };

    #[test]
    fn a_numbering_counts_only_the_entries_that_set_it() {
        assert_eq!(
            numbering_errors(&[None, Some(0), None, Some(1)], WHEELS),
            Vec::new()
        );
    }

    #[test]
    fn a_numbering_names_a_repeat_by_the_later_entry_and_a_gap_by_the_list() {
        let errors: Vec<String> = numbering_errors(&[Some(0), None, Some(0)], WHEELS)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            errors,
            [
                "usb_devices[2]: wheel 0 is also usb_devices[0]'s",
                "usb_devices: wheel numbers must run 0..M-1, and 1 is missing; a gap is \
                 rejected rather than filled, so renumber the wheels above it",
            ]
        );
    }

    #[test]
    fn an_unset_name_is_left_out_of_the_entry_rather_than_written_null() {
        let text = serde_json::to_string(&usb_entry(0, "a")).unwrap();
        assert_eq!(text, r#"{"device_number":0,"usb_port":"a"}"#);
    }

    #[test]
    fn an_entry_pastes_as_itself_and_declares_no_wheel() {
        let mut entry = usb_entry(3, "p");
        entry.description = Some("d".to_string());
        assert_eq!(
            entry.block_entry(),
            BlockEntry {
                device_number: 3,
                usb_port: "p".to_string(),
                description: Some("d".to_string()),
                ..BlockEntry::default()
            }
        );
    }

    // --- building the listing ----------------------------------------------

    fn facts(model: &str, sdk_id: &str, override_key: Option<&str>) -> CameraFacts {
        CameraFacts {
            model: model.to_string(),
            sdk_id: Some(sdk_id.to_string()),
            override_key: override_key.map(str::to_string),
        }
    }

    fn named(name: &str) -> DisplayNames {
        DisplayNames {
            name: Some(name.to_string()),
            ..DisplayNames::default()
        }
    }

    /// Two cameras of two models, placed so that port order (1-4.2 first)
    /// reverses SDK order.
    fn two_cameras() -> Claims<'static> {
        claims(
            Ok(scan(vec![
                record("9a0a", "1-4.10"),
                record("9a0b", "1-4.2"),
            ])),
            vec![camera("SV605CC"), camera("SV705C")],
        )
    }

    fn has_note(listing: &Listing, start: &str) -> bool {
        listing.notes.iter().any(|n| n.starts_with(start))
    }

    fn has_paste_note(listing: &Listing, start: &str) -> bool {
        listing.paste_notes.iter().any(|n| n.starts_with(start))
    }

    #[test]
    fn with_no_list_the_block_carries_the_override_names_in_port_order() {
        let cameras = [
            facts("SV605CC", "A", Some("A")),
            facts("SV705C", "B", Some("B")),
        ];
        let overrides = BTreeMap::from([
            ("B".to_string(), named("Main")),
            ("stale".to_string(), DisplayNames::default()),
        ]);
        let listing = two_cameras().listing(&cameras, None, &overrides);
        let block = listing.block.clone().unwrap();
        assert_eq!(block[0].usb_port, "1-4.2");
        assert_eq!(block[0].name.as_deref(), Some("Main"));
        assert_eq!(block[1].usb_port, "1-4.10");
        assert_eq!(block[1].name, None);
        assert_eq!(
            (
                listing.rows[0].device.as_str(),
                listing.rows[1].device.as_str()
            ),
            ("0", "1"),
            "without a list each camera is served at its place in port order"
        );
        assert_eq!(listing.rows[0].sdk_id.as_deref(), Some("B"));
        assert!(
            !listing.notes.iter().any(|n| n.contains("renumber")),
            "the block pins the numbers served now: {:?}",
            listing.notes
        );
        assert!(
            has_paste_note(
                &listing,
                "The devices override stale matches no camera placed on a port"
            ),
            "{:?}",
            listing.paste_notes
        );
        assert!(
            has_paste_note(
                &listing,
                "Delete the devices map from svbony-camera.json when you paste this block: the \
                 block carries its names"
            ),
            "{:?}",
            listing.paste_notes
        );
        assert!(
            has_note(&listing, "Cannot tell which row is which camera?"),
            "{:?}",
            listing.notes
        );
        assert!(
            !has_paste_note(&listing, "Cannot tell which row is which camera?"),
            "the unplug note is about the rows: {:?}",
            listing.paste_notes
        );
    }

    #[test]
    fn with_no_list_an_override_naming_filters_declares_a_wheel_numbered_in_port_order() {
        let cameras = [
            facts("SV605CC", "A", Some("A")),
            facts("SV705C", "B", Some("B")),
        ];
        let wheel = |names: &[&str]| DisplayNames {
            filter_names: Some(names.iter().map(|n| (*n).to_string()).collect()),
            ..DisplayNames::default()
        };
        let overrides = BTreeMap::from([
            ("A".to_string(), wheel(&["L", "R"])),
            ("B".to_string(), wheel(&["Ha"])),
        ]);
        let listing = two_cameras().listing(&cameras, None, &overrides);
        let block = listing.block.clone().unwrap();
        assert_eq!(
            (block[0].usb_port.as_str(), block[0].filter_wheel_number),
            ("1-4.2", Some(0))
        );
        assert_eq!(block[0].filter_names, Some(vec!["Ha".to_string()]));
        assert_eq!(
            (block[1].usb_port.as_str(), block[1].filter_wheel_number),
            ("1-4.10", Some(1))
        );
        assert!(
            !listing.notes.iter().any(|n| n.contains("renumber")),
            "the cameras are served in port order already: {:?}",
            listing.notes
        );
    }

    #[test]
    fn with_no_list_only_a_camera_whose_override_names_filters_declares_a_wheel() {
        let cameras = [
            facts("SV605CC", "A", Some("A")),
            facts("SV705C", "B", Some("B")),
        ];
        let overrides = BTreeMap::from([
            ("A".to_string(), named("Main")),
            (
                "B".to_string(),
                DisplayNames {
                    filter_names: Some(vec!["L".to_string()]),
                    ..DisplayNames::default()
                },
            ),
        ]);
        let block = two_cameras()
            .listing(&cameras, None, &overrides)
            .block
            .unwrap();
        assert_eq!(block[0].filter_wheel_number, Some(0), "B, on 1-4.2");
        assert_eq!(block[1].filter_wheel_number, None, "A names no filters");
        assert_eq!(block[1].filter_names, None);
    }

    #[test]
    fn with_no_list_an_override_that_needs_an_open_to_match_is_named_and_not_carried() {
        let cameras = [CameraFacts {
            model: "SV605CC".to_string(),
            sdk_id: None,
            override_key: None,
        }];
        let overrides = BTreeMap::from([("0a1b2c".to_string(), named("Main"))]);
        let listing = claims(
            Ok(scan(vec![record("9a0a", "p1")])),
            vec![camera("SV605CC")],
        )
        .listing(&cameras, None, &overrides);
        assert_eq!(listing.block.as_ref().unwrap()[0].name, None);
        assert!(
            has_paste_note(
                &listing,
                "The devices override 0a1b2c cannot be matched to a camera without opening one"
            ),
            "{:?}",
            listing.paste_notes
        );
        assert!(
            has_paste_note(
                &listing,
                "Delete the devices map from svbony-camera.json when you paste this block: a \
                 devices map beside a usb_devices list refuses the start."
            ),
            "{:?}",
            listing.paste_notes
        );
        assert!(
            !has_note(&listing, "Pasting this block renumbers"),
            "{:?}",
            listing.notes
        );
    }

    #[test]
    fn with_a_list_the_block_reproduces_it_and_marks_an_unlisted_camera() {
        let list = [entry(1, "p9", None), entry(0, "p8", Some("Guide port"))];
        let listing = claims(
            Ok(scan(vec![record("9a0a", "p1")])),
            vec![camera("SV605CC")],
        )
        .listing(
            &[facts("SV605CC", "SN", Some("SN"))],
            Some(&list),
            &BTreeMap::new(),
        );
        assert_eq!(listing.rows[0].device, "not listed");
        let ports: Vec<String> = listing
            .block
            .unwrap()
            .into_iter()
            .map(|e| e.usb_port)
            .collect();
        assert_eq!(ports, vec!["p8", "p9"]);
    }

    #[test]
    fn a_listed_camera_shows_its_entrys_number() {
        let list = [entry(3, "p1", None)];
        let listing = claims(
            Ok(scan(vec![record("9a0a", "p1")])),
            vec![camera("SV605CC")],
        )
        .listing(
            &[facts("SV605CC", "SN", Some("SN"))],
            Some(&list),
            &BTreeMap::new(),
        );
        assert_eq!(listing.rows[0].device, "3");
    }

    #[test]
    fn with_no_list_and_no_placed_camera_there_is_nothing_to_paste() {
        let listing = claims(Ok(scan(Vec::new())), Vec::new()).listing(&[], None, &BTreeMap::new());
        assert_eq!(listing.block, None);
        assert_eq!(listing.service, "svbony-camera");
        assert_eq!(listing.vendor, "f266");
    }

    // --- the scan source ----------------------------------------------------

    #[cfg(feature = "mock")]
    #[test]
    fn a_simulated_source_scans_its_own_records() {
        let records = scan(vec![record("9a0a", "simulated-usbv3-0:1")]);
        let source = UsbSource::from_flag(None, records.clone()).unwrap();
        assert_eq!(source, UsbSource::Simulated(records.clone()));
        assert_eq!(source.scan().unwrap(), records);
    }

    #[cfg(feature = "mock")]
    #[test]
    fn a_staged_source_that_cannot_be_read_refuses_naming_the_file() {
        let error = UsbSource::from_flag(
            Some(std::path::PathBuf::from("/nonexistent/usb-inventory.json")),
            scan(Vec::new()),
        )
        .unwrap_err();
        assert!(error.contains("/nonexistent/usb-inventory.json"), "{error}");
    }

    #[cfg(feature = "mock")]
    #[test]
    fn a_staged_source_reads_its_file_again_at_every_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usb-inventory.json");
        std::fs::write(&path, r#"{"usb_unavailable": "timed out"}"#).unwrap();
        let source = UsbSource::from_flag(Some(path.clone()), scan(Vec::new())).unwrap();
        assert_eq!(source.scan().unwrap_err(), "timed out");
        std::fs::write(&path, r#"{"usb": []}"#).unwrap();
        assert_eq!(source.scan().unwrap(), scan(Vec::new()));
    }
    /// `doctor --devices` over one SV605CC on 1-4.2, with `list_check`, a
    /// scan that answers `scan`, and an SDK that answers `enumerate`.
    fn report(
        list_check: Result<(), String>,
        scan: Result<UsbScan, String>,
        enumerate: Result<(), String>,
    ) -> (String, i32) {
        devices_report(
            SVB,
            "svbony-camera",
            std::path::Path::new("/etc/svbony-camera.json"),
            list_check.map(|()| None),
            scan,
            || enumerate.map(|()| (vec![camera("SV605CC")], ())),
            |claims, ()| {
                claims.listing(&[facts("SV605CC", "A", Some("A"))], None, &BTreeMap::new())
            },
        )
    }

    fn one_camera() -> UsbScan {
        scan(vec![record("9a0a", "1-4.2")])
    }

    #[test]
    fn devices_lists_the_cameras_and_prints_the_block() {
        let (output, code) = report(Ok(()), Ok(one_camera()), Ok(()));
        assert_eq!(code, 0, "{output}");
        assert!(output.contains("1-4.2"), "{output}");
        assert!(output.contains("\"usb_devices\""), "{output}");
    }

    #[test]
    fn devices_fails_on_a_failed_scan_without_asking_the_sdk() {
        let (output, code) = report(
            Ok(()),
            Err("powershell.exe timed out".to_string()),
            Err("the SDK was asked".to_string()),
        );
        assert_eq!(code, 1);
        assert_eq!(
            output,
            "SVBony cameras on the bus (vendor f266):\n\n  the USB scan failed: powershell.exe \
             timed out\n"
        );
    }

    #[test]
    fn devices_fails_when_the_sdk_cannot_enumerate() {
        let (output, code) = report(Ok(()), Ok(one_camera()), Err("no SDK".to_string()));
        assert_eq!(code, 1);
        assert_eq!(output, "SVBony SDK enumeration failed: no SDK\n");
    }

    #[test]
    fn devices_names_a_broken_list_first_whatever_else_fails() {
        let (output, code) = report(
            Err("number 0 is missing".to_string()),
            Err("powershell.exe timed out".to_string()),
            Ok(()),
        );
        assert_eq!(code, 1);
        assert!(
            output.starts_with(
                "The usb_devices list in /etc/svbony-camera.json breaks a rule, so the service \
                 refuses to start: number 0 is missing.\n\nSVBony cameras on the bus"
            ),
            "{output}"
        );
    }

    #[test]
    fn devices_withholds_the_block_of_a_broken_list_and_lists_the_ports() {
        let (output, code) = report(
            Err("number 0 is missing".to_string()),
            Ok(one_camera()),
            Ok(()),
        );
        assert_eq!(code, 1);
        assert!(output.contains("1-4.2"), "{output}");
        assert!(!output.contains("\"usb_devices\""), "{output}");
        assert!(output.contains("Fix it from the ports above."), "{output}");
    }

    // --- C5: the placeholder's port, and the doctor checks ---------------------

    #[test]
    fn a_placeholder_unique_id_gives_back_the_port_it_stands_in_for() {
        let port = "pci-0000:00:14.0-usbv3-0:4.2";
        assert_eq!(
            placeholder_port(&placeholder_unique_id("zwo-camera", port)),
            Some(port)
        );
        let windows = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)";
        assert_eq!(
            placeholder_wheel_unique_id("qhy-camera", windows),
            format!("placeholder:qhy-camera:CFW-{windows}")
        );
        assert_eq!(
            placeholder_port(&placeholder_wheel_unique_id("qhy-camera", windows)),
            Some(windows),
            "a wheel's port is its camera's"
        );
        assert_eq!(
            placeholder_port("ZWO:ASI1600MM-Cool:noserial-pci-0000:00:14.0-usbv3-0:4.2"),
            None,
            "a camera's UniqueID is no placeholder's"
        );
    }

    /// Each check as (name, status, detail).
    fn verdicts(checks: &[Check]) -> Vec<(&str, Status, &str)> {
        checks
            .iter()
            .map(|c| (c.name.as_str(), c.status, c.detail.as_str()))
            .collect()
    }

    #[test]
    fn with_a_list_every_port_resolving_is_one_ok_and_an_unlisted_camera_is_named() {
        let checks = two_cameras().checks(Some(&[entry(0, "1-4.2", None)]), None);
        assert_eq!(
            verdicts(&checks),
            [
                (
                    RESOLVE,
                    Status::Ok,
                    "1 listed port resolves, no placeholders"
                ),
                (
                    UNLISTED,
                    Status::Ok,
                    "1-4.10 (SV605CC) is on the bus and not listed"
                ),
            ]
        );
        let both = two_cameras().checks(
            Some(&[entry(0, "1-4.2", None), entry(1, "1-4.10", None)]),
            None,
        );
        assert_eq!(
            verdicts(&both),
            [(
                RESOLVE,
                Status::Ok,
                "2 listed ports resolve, no placeholders"
            )]
        );
    }

    #[test]
    fn with_a_list_each_number_a_placeholder_would_hold_warns_with_its_reason() {
        let checks = two_cameras().checks(
            Some(&[entry(1, "1-4.9", None), entry(0, "1-4.2", None)]),
            None,
        );
        let resolve: Vec<_> = verdicts(&checks)
            .into_iter()
            .filter(|(name, ..)| *name == RESOLVE)
            .collect();
        assert_eq!(resolve.len(), 1, "no ok beside a warning: {resolve:?}");
        assert_eq!(resolve[0].1, Status::Warn);
        assert!(
            resolve[0].2.starts_with(
                "device 1 (1-4.9) would be a placeholder: no working camera is enumerated on 1-4.9"
            ),
            "{resolve:?}"
        );
    }

    #[test]
    fn an_empty_list_resolves_nothing_and_says_so() {
        let checks = two_cameras().checks(Some(&[]), None);
        assert_eq!(
            verdicts(&checks),
            [
                (
                    RESOLVE,
                    Status::Ok,
                    "the list is empty: no camera is registered"
                ),
                (
                    UNLISTED,
                    Status::Ok,
                    "1-4.2 (SV705C) is on the bus and not listed"
                ),
                (
                    UNLISTED,
                    Status::Ok,
                    "1-4.10 (SV605CC) is on the bus and not listed"
                ),
            ],
            "every camera on the bus is one no entry lists"
        );
        // An empty list needs no scan, so a failed one is no fault of it.
        let failed = claims(Err("timed out".to_string()), vec![camera("SV605CC")]);
        assert_eq!(
            verdicts(&failed.checks(Some(&[]), None)),
            [(
                RESOLVE,
                Status::Ok,
                "the list is empty: no camera is registered"
            )]
        );
    }

    #[test]
    fn a_failed_scan_fails_the_check_that_applies_and_nothing_else() {
        let failed = claims(Err("timed out".to_string()), vec![camera("SV605CC")]);
        let listed = failed.checks(Some(&[entry(0, "1-4.2", None)]), None);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].name.as_str(), listed[0].status),
            (RESOLVE, Status::Fail)
        );
        assert!(
            listed[0]
                .detail
                .starts_with("the USB scan failed: timed out;"),
            "{}",
            listed[0].detail
        );
        let implicit = failed.checks(None, None);
        assert_eq!(implicit.len(), 1);
        assert_eq!(
            (implicit[0].name.as_str(), implicit[0].status),
            (IMPLICIT, Status::Fail)
        );
        assert!(
            implicit[0]
                .detail
                .starts_with("the USB scan failed: timed out;"),
            "{}",
            implicit[0].detail
        );
    }

    #[test]
    fn without_a_list_one_placed_camera_is_not_reported() {
        let one = claims(
            Ok(scan(vec![record("9a0a", "1-4.2")])),
            vec![camera("SV605CC")],
        );
        assert_eq!(verdicts(&one.checks(None, None)), []);
    }

    #[test]
    fn without_a_list_several_cameras_are_named_with_their_numbers_and_the_block() {
        let claims = two_cameras();
        // A devices key no camera matches, so the listing says to delete
        // the map when pasting; the block must carry that with it.
        let overrides = BTreeMap::from([("stale".to_string(), DisplayNames::default())]);
        let listing = claims.listing(
            &[
                facts("SV605CC", "A", Some("A")),
                facts("SV705C", "B", Some("B")),
            ],
            None,
            &overrides,
        );
        let checks = claims.checks(None, Some(&listing));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(
            (checks[0].name.as_str(), checks[0].status),
            (IMPLICIT, Status::Ok)
        );
        assert!(
            checks[0].detail.starts_with(
                "without a list the service serves 2 cameras, numbered in port order: \
                 0 SV705C on 1-4.2, 1 SV605CC on 1-4.10. To pin them, paste this into \
                 svbony-camera.json:"
            ),
            "{}",
            checks[0].detail
        );
        assert!(
            checks[0]
                .detail
                .contains(r#"{ "device_number": 1, "usb_port": "1-4.10" }"#),
            "{}",
            checks[0].detail
        );
        assert!(
            checks[0]
                .detail
                .contains("Delete the devices map from svbony-camera.json when you paste"),
            "the block goes with what pasting it needs: {}",
            checks[0].detail
        );
        assert!(
            !checks[0].detail.contains(UNPLUG_NOTE),
            "the unplug note is about the listing's rows: {}",
            checks[0].detail
        );
    }

    #[test]
    fn without_a_list_each_refused_camera_warns() {
        let look_alike = claims(
            Ok(scan(vec![record("9a0a", "1-4.1"), record("9a0a", "1-4.2")])),
            vec![camera("SV605CC")],
        );
        let checks = look_alike.checks(None, None);
        assert!(
            checks.iter().any(|c| c.name == IMPLICIT
                && c.status == Status::Warn
                && c.detail.contains("cannot be told apart")),
            "{checks:?}"
        );
        assert!(
            checks.iter().all(|c| c.status != Status::Ok),
            "nothing is placed, so nothing is named as served: {checks:?}"
        );
    }

    #[test]
    fn the_report_checks_fail_on_a_failed_scan_and_list_nothing() {
        let failed = usb_devices_checks(
            SVB,
            "svbony-camera",
            None,
            Err("timed out".to_string()),
            vec![camera("SV605CC")],
            |_| panic!("a failed scan has no ports to list"),
        );
        assert_eq!(
            failed.iter().map(|c| c.status).collect::<Vec<_>>(),
            [Status::Fail]
        );
    }

    #[test]
    fn the_report_checks_carry_the_listings_block_without_a_list() {
        let checks = usb_devices_checks(
            SVB,
            "svbony-camera",
            None,
            Ok(scan(vec![
                record("9a0a", "1-4.10"),
                record("9a0b", "1-4.2"),
            ])),
            vec![camera("SV605CC"), camera("SV705C")],
            |claims| {
                claims.listing(
                    &[
                        facts("SV605CC", "A", Some("A")),
                        facts("SV705C", "B", Some("B")),
                    ],
                    None,
                    &BTreeMap::new(),
                )
            },
        );
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert!(
            checks[0].detail.contains(r#""usb_devices": ["#),
            "{}",
            checks[0].detail
        );
    }

    #[test]
    fn devices_closes_the_listing_with_the_checks() {
        let list = [entry(0, "1-4.9", None)];
        let (output, code) = devices_report(
            SVB,
            "svbony-camera",
            std::path::Path::new("/etc/svbony-camera.json"),
            Ok(Some(&list)),
            Ok(one_camera()),
            || Ok((vec![camera("SV605CC")], ())),
            |claims, ()| {
                claims.listing(
                    &[facts("SV605CC", "A", Some("A"))],
                    Some(&list),
                    &BTreeMap::new(),
                )
            },
        );
        assert_eq!(code, 0, "a warning fails nothing: {output}");
        assert!(
            output.contains(
                "\nwarn usb-devices.resolve: device 0 (1-4.9) would be a placeholder: no working \
                 camera is enumerated on 1-4.9"
            ),
            "{output}"
        );
        assert!(
            !output.contains("usb-devices.unlisted"),
            "the listing's rows already say the camera is not listed: {output}"
        );
    }

    #[test]
    fn devices_without_a_list_adds_no_check_to_the_listing() {
        let (output, code) = report(Ok(()), Ok(one_camera()), Ok(()));
        assert_eq!(code, 0);
        assert!(!output.contains("usb-devices."), "{output}");
    }

    /// Each status is spelled as `--json` spells it.
    #[test]
    fn a_check_line_leads_with_its_status() {
        let checks = [
            Check::ok(RESOLVE, None, "a"),
            Check::warn(RESOLVE, None, "b", None),
            Check::fail(RESOLVE, None, "c", None),
        ];
        assert_eq!(
            render_checks(&checks),
            "ok   usb-devices.resolve: a\nwarn usb-devices.resolve: b\nfail usb-devices.resolve: c\n"
        );
    }
}

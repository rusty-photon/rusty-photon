//! Device claims: the vendor-neutral half of a camera driver's `usb_devices`
//! list ([device-claims plan](../../../docs/plans/device-claims-and-phd2-camera.md),
//! Part A).
//!
//! A list pins each USB port to an Alpaca device number. What is the same in
//! every camera driver lives here, beside the USB inventory it reads: the
//! list's validation, the passive join that places each SDK camera on a port,
//! the reason a listed number is held by a placeholder, the placeholder's
//! fixed error code and `UniqueID` form, the order ports are listed in, the
//! background re-scan's schedule, and the `doctor --devices` listing. What
//! differs per driver — which SDK models it knows on the bus, how it scans,
//! its config's entry type, its placeholder device — stays in the driver.
//!
//! Everything here is pure: nothing scans, opens or sleeps.

use std::cmp::Ordering;
use std::fmt::{self, Write as _};
use std::time::Duration;

use crate::facts::{UsbDevice, UsbScan};

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

/// Whether a device is a device-claims placeholder, by its `UniqueID`.
///
/// The driver range is shared by every Alpaca driver, so a client tells a
/// placeholder's [`PLACEHOLDER_ERROR_CODE`] from another driver's use of the
/// same number by this, not by the code alone.
#[must_use]
pub fn is_placeholder_unique_id(unique_id: &str) -> bool {
    unique_id.starts_with(PLACEHOLDER_ID_PREFIX)
}

// --- the list -------------------------------------------------------------------

/// What validation reads of one `usb_devices` entry.
#[derive(Debug, Clone, Copy)]
pub struct EntryView<'a> {
    pub device_number: u32,
    pub usb_port: &'a str,
}

/// A rule a list (or the config around it) breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListError {
    /// The dotted config path `config.apply` reports it at
    /// (`usb_devices.1.usb_port`).
    pub path: String,
    /// What a load error names it by (`usb_devices[1]`).
    pub location: String,
    /// What is wrong.
    pub message: String,
}

impl ListError {
    fn entry(index: usize, field: &str, message: String) -> Self {
        Self {
            path: format!("usb_devices.{index}.{field}"),
            location: format!("usb_devices[{index}]"),
            message,
        }
    }

    fn list(message: String) -> Self {
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
/// empty list is valid here — it is legal in a file; [`empty_list_over_apply`]
/// is the one path that refuses it.
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
        if let Some(first) = earlier
            .iter()
            .position(|e| e.device_number == entry.device_number)
        {
            errors.push(ListError::entry(
                index,
                "device_number",
                format!(
                    "device_number {} is also usb_devices[{first}]'s",
                    entry.device_number
                ),
            ));
        }
    }
    let missing: Vec<String> = (0..entries.len())
        .filter_map(|n| u32::try_from(n).ok())
        .filter(|n| !entries.iter().any(|e| e.device_number == *n))
        .map(|n| n.to_string())
        .collect();
    if !missing.is_empty() {
        let verb = if missing.len() == 1 { "is" } else { "are" };
        errors.push(ListError::list(format!(
            "device numbers must run 0..N-1, and {} {verb} missing; a gap is \
             rejected rather than filled, so renumber the entries above it",
            missing.join(", ")
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

/// `config.apply`'s refusal of `"usb_devices": []`: an empty list registers
/// no device, so the apply would remove the device it arrived through.
#[must_use]
pub fn empty_list_over_apply() -> ListError {
    ListError::list(
        "an empty list registers no camera, so this apply would remove the device \
         it arrived through; to hand every camera to another application, write \
         \"usb_devices\": [] into the file and reload the service"
            .to_string(),
    )
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

    fn is_own_record(&self, record: &UsbDevice) -> bool {
        record.vendor == self.vendor && !self.not_cameras.contains(&record.product.as_str())
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
/// could explain either: exactly one of each remains, no fault carries this
/// vendor's id, the record's product id is not one the normalizer gives any
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
            let vendor_fault = scan
                .faults
                .iter()
                .any(|f| f.vendor.as_deref() == Some(normalizer.vendor));
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
            if !vendor_fault && !model_known && !product_known && !serials_disagree {
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
            Err(error) => format!("the USB scan failed: {error}"),
        };
        Resolution::Placeholder(format!(
            "{reason}. Once the camera is fixed, reload or restart {}, which \
             re-opens every camera it serves",
            self.service
        ))
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
            // Only a record no known model claims could be the unrecognised
            // camera's: one under a known model's product id is that model,
            // which the SDK does not report, and no new entry would pair it.
            if self.normalizer.is_own_record(record)
                && !self.normalizer.assigns_product(&record.product)
            {
                let unrecognised: Vec<&str> = placement
                    .cameras
                    .iter()
                    .zip(&self.sdk)
                    .filter(|(place, _)| **place == CameraPlace::Unrecognised)
                    .map(|(_, camera)| camera.model.as_str())
                    .collect();
                if !unrecognised.is_empty() {
                    let models = unrecognised.join(", ");
                    return format!(
                        "{port} holds {product} ({id}), and the {sdk} SDK reports camera(s) \
                         this driver knows no product id for ({models}), so nothing pairs \
                         them; the driver's normalizer needs an entry for each of {models}"
                    );
                }
            }
            return format!(
                "{port} holds {product} ({id}), which the {sdk} SDK does not report as a camera"
            );
        }
        let suspects = scan
            .faults
            .iter()
            .filter(|f| f.location.as_deref() != Some(port) && self.could_be_camera(f))
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

    /// Whether a fault could be this driver's camera: it carries this
    /// vendor's id, or no real one — none could be read, or it is the `0000`
    /// Windows gives a device whose enumeration failed (doctor.md, "USB
    /// inventory"). Another vendor's fault is that vendor's device.
    fn could_be_camera(&self, fault: &crate::facts::UsbFault) -> bool {
        fault
            .vendor
            .as_deref()
            .is_none_or(|vendor| vendor == self.normalizer.vendor || vendor == "0000")
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
            if placement.camera_on(port).is_none() {
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
            .filter(|f| f.vendor.as_deref() == Some(self.normalizer.vendor))
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
/// The port is cut into the runs of letters and digits between its separators, and two
/// runs that are both decimal compare as numbers, so `…-0:4.2` precedes
/// `…-0:4.10` and `USB(2)` precedes `USB(14)`. Any other run compares as
/// text, which is hex order for the fixed-width hex the platforms write
/// (`PCI(1400)`, a macOS `0x14200000`). Equal runs fall back to the whole
/// string, so the order is total.
#[must_use]
pub fn compare_ports(a: &str, b: &str) -> Ordering {
    let runs = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|run| !run.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (left, right) = (runs(a), runs(b));
    for (x, y) in left.iter().zip(&right) {
        let order = if is_decimal(x) && is_decimal(y) {
            compare_decimal(x, y)
        } else {
            x.cmp(y)
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len()).then_with(|| a.cmp(b))
}

fn is_decimal(run: &str) -> bool {
    run.bytes().all(|b| b.is_ascii_digit())
}

/// Two decimal runs compared as numbers of any length.
fn compare_decimal(x: &str, y: &str) -> Ordering {
    let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
}

// --- the re-scan ----------------------------------------------------------------

/// The waits between a failed scan's re-scans (the plan's D4.4): 10 s, 20 s
/// and 40 s, then 60 s for as long as the scan keeps failing.
pub fn rescan_waits() -> impl Iterator<Item = Duration> {
    [10, 20, 40]
        .into_iter()
        .map(Duration::from_secs)
        .chain(std::iter::repeat(Duration::from_secs(60)))
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEntry {
    pub device_number: u32,
    pub usb_port: String,
    pub name: Option<String>,
    pub description: Option<String>,
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
    /// Paragraphs printed after the table: what the join could not place,
    /// renumbering warnings, the unplug procedure.
    pub notes: Vec<String>,
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
    for note in &listing.notes {
        out.push('\n');
        out.push_str(note);
        out.push('\n');
    }
    if let Some(block) = &listing.block {
        let _ = write!(
            out,
            "\nTo pin the cameras this driver serves, paste this into {}.json.\n\
             Leave out any camera another application owns (PHD2's guide camera), and\n\
             keep the numbers running 0..N-1:\n\n",
            listing.service
        );
        out.push_str(&render_block(block));
    }
    out
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

    #[test]
    fn config_apply_refuses_an_empty_list_at_the_list() {
        let error = empty_list_over_apply();
        assert_eq!(error.path, "usb_devices");
        assert!(error
            .message
            .starts_with("an empty list registers no camera"));
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
        assert!(placement
            .cameras
            .iter()
            .all(|p| matches!(p, CameraPlace::LookAlike { .. })));
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
    fn a_known_models_record_beside_an_unrecognised_camera_is_the_known_model() {
        // No entry for SV905C could pair it with 9a0a: that record is an
        // SV605CC the SDK does not report.
        let claims = claims(Ok(scan(vec![record("9a0a", "p1")])), vec![camera("SV905C")]);
        let reason = placeholder_reason(claims.resolve("p1"));
        assert!(
            reason.starts_with(
                "p1 holds SVBONY 9a0a (f266:9a0a), which the SVBony SDK does not report as a \
                 camera."
            ),
            "{reason}"
        );
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
        let mut bus = scan(vec![record("9a0a", "p1"), record("9a0a", "p2")]);
        bus.faults.push(fault(Some("f266"), Some("p3")));
        let notes = claims(Ok(bus), vec![camera("SV605CC")]).unplaced_notes();
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert!(notes[0].contains("cannot be told apart"), "{notes:?}");
        assert!(notes[1].starts_with("p1 holds"), "{notes:?}");
        assert!(notes[3].contains("is not a working device"), "{notes:?}");
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
    fn a_port_precedes_the_ports_behind_it() {
        assert_eq!(compare_ports("1-4", "1-4.1"), Ordering::Less);
    }

    #[test]
    fn port_order_is_total() {
        assert_eq!(compare_ports("1-04", "1-4"), Ordering::Less);
        assert_eq!(compare_ports("1-4", "1-4"), Ordering::Equal);
    }

    // --- the re-scan ---------------------------------------------------------

    #[test]
    fn rescans_wait_10_20_40_then_60_seconds() {
        let waits: Vec<u64> = rescan_waits().take(6).map(|w| w.as_secs()).collect();
        assert_eq!(waits, vec![10, 20, 40, 60, 60, 60]);
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
            description: None,
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
}

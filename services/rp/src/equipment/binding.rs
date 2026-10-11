//! The connect routine every equipment kind shares.
//!
//! It locates a roster entry's device in its server's
//! `configureddevices` list, checks the entry's pinned `UniqueID`,
//! switches the device on, and logs what was bound (rp.md § Device
//! Identity Pin, § Device Session Recovery). The reconnect supervisor
//! also re-checks a pinned entry's live session against the listing on
//! every pass ([`check_pin`]).
//!
//! The identity checked is the one the listing gives for the very entry
//! whose device URL rp binds, read in the same response, so the pin costs
//! no extra request. The check runs before `Connected = true`, so a
//! refused device is never switched on. The listing read and the
//! `Connected = true` request are still two requests, though: a server
//! that renumbers its devices between them makes rp switch on and bind
//! whatever device is then at that number. Alpaca has no identity-checked
//! connect to close that window; the next pass's [`check_pin`] catches it.

use std::path::Path;
use std::sync::Arc;

use ascom_alpaca::api::{
    Camera, CoverCalibrator, Device, Dome, FilterWheel, Focuser, ObservingConditions, Rotator,
    SafetyMonitor, Switch, Telescope, TypedDevice,
};
use rp_auth::config::ClientAuthConfig;
use tracing::info;

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config::{self, UniqueIdPin};

/// What a roster entry says about the device it addresses.
pub(super) struct RosterAddress<'a> {
    /// The kind as log lines and errors name it (`"camera"`,
    /// `"filter wheel"`, …).
    pub kind: &'static str,
    /// The roster id; `None` for the singular mount, which has none.
    pub id: Option<&'a str>,
    pub alpaca_url: &'a str,
    /// The device's position among the server's devices of this kind.
    pub device_number: u32,
    pub unique_id: Option<&'a UniqueIdPin>,
    pub auth: Option<&'a ClientAuthConfig>,
}

impl RosterAddress<'_> {
    /// The retry helper's log label: `"camera main-cam"`, or the bare
    /// kind for the id-less mount.
    fn label(&self) -> String {
        self.id
            .map_or_else(|| self.kind.to_string(), |id| format!("{} {id}", self.kind))
    }

    /// Why the device at this entry's number was not bound, as the
    /// operator reads it.
    fn refusal(&self, refusal: &Refusal) -> EstablishError {
        let kind = self.kind;
        let number = self.device_number;
        let pin = self.unique_id.map_or("", UniqueIdPin::as_str);
        match refusal {
            Refusal::NotListed => EstablishError::Failed(format!(
                "{kind} at index {number} not found on Alpaca server"
            )),
            Refusal::Unverifiable { found_name } => EstablishError::IdentityRefused(format!(
                "{kind} at device_number {number} ({found_name:?}) reports an empty UniqueID, \
                 so the pinned unique_id {pin:?} cannot be verified; refusing to connect it"
            )),
            Refusal::Mismatch { found, expected_at } => {
                let found_name = &found.name;
                let found_id = &found.unique_id;
                let elsewhere = expected_at.map_or_else(
                    || format!("no {kind} on this server reports {pin:?}"),
                    |at| {
                        format!(
                            "{pin:?} is listed at device_number {at} on this server; \
                             set this entry's device_number to {at} if the device moved"
                        )
                    },
                );
                EstablishError::IdentityRefused(format!(
                    "{kind} at device_number {number} is {found_name:?} with UniqueID \
                     {found_id:?}, not the pinned unique_id {pin:?}; refusing to connect it: \
                     {elsewhere}"
                ))
            }
        }
    }
}

/// A roster entry kind: how its config addresses the device, and which
/// entries of a server's listing are of its kind.
pub(super) trait RosterEntry: Sync {
    /// The Alpaca device trait object the kind binds (`dyn Camera`, …).
    type Device: Device + ?Sized;

    /// What the entry says about the device it addresses.
    fn address(&self) -> RosterAddress<'_>;

    /// This kind's device, if `device` is one.
    fn of_kind(device: TypedDevice) -> Option<Arc<Self::Device>>;
}

/// [`RosterEntry`] for the kinds whose entries carry an `id`. Every
/// address is built here from the same field names, so no kind can bind
/// without forwarding its `unique_id`.
macro_rules! roster_entry {
    ($($config:ty => $variant:ident as $device:ty, $kind:literal;)+) => {
        $(
            impl RosterEntry for $config {
                type Device = $device;

                fn address(&self) -> RosterAddress<'_> {
                    RosterAddress {
                        kind: $kind,
                        id: Some(&self.id),
                        alpaca_url: &self.alpaca_url,
                        device_number: self.device_number,
                        unique_id: self.unique_id.as_ref(),
                        auth: self.auth.as_ref(),
                    }
                }

                fn of_kind(device: TypedDevice) -> Option<Arc<$device>> {
                    match device {
                        TypedDevice::$variant(device) => Some(device),
                        _ => None,
                    }
                }
            }
        )+
    };
}

roster_entry! {
    config::CameraConfig => Camera as dyn Camera, "camera";
    config::FilterWheelConfig => FilterWheel as dyn FilterWheel, "filter wheel";
    config::CoverCalibratorConfig => CoverCalibrator as dyn CoverCalibrator, "cover calibrator";
    config::FocuserConfig => Focuser as dyn Focuser, "focuser";
    config::SafetyMonitorConfig => SafetyMonitor as dyn SafetyMonitor, "safety monitor";
    config::SwitchConfig => Switch as dyn Switch, "switch";
    config::RotatorConfig => Rotator as dyn Rotator, "rotator";
    config::ObservingConditionsConfig => ObservingConditions as dyn ObservingConditions, "observing conditions";
    config::DomeConfig => Dome as dyn Dome, "dome";
}

/// The singular mount has no `id`; otherwise the same as every kind.
impl RosterEntry for config::MountConfig {
    type Device = dyn Telescope;

    fn address(&self) -> RosterAddress<'_> {
        RosterAddress {
            kind: "mount",
            id: None,
            alpaca_url: &self.alpaca_url,
            device_number: self.device_number,
            unique_id: self.unique_id.as_ref(),
            auth: self.auth.as_ref(),
        }
    }

    fn of_kind(device: TypedDevice) -> Option<Arc<dyn Telescope>> {
        match device {
            TypedDevice::Telescope(device) => Some(device),
            _ => None,
        }
    }
}

/// Why an establish routine produced no session.
#[derive(Debug, PartialEq, Eq)]
pub enum EstablishError {
    /// The device at the entry's number is not the one its pin names, or
    /// cannot be checked against it. Any handle the entry still holds
    /// addresses that other device.
    IdentityRefused(String),
    /// Anything else: the client could not be built, the server could
    /// not be read or does not list the device, or `Connected = true`
    /// failed.
    Failed(String),
}

impl EstablishError {
    /// Whether the pin refused the device at the entry's number.
    #[must_use]
    pub const fn is_identity_refusal(&self) -> bool {
        matches!(self, Self::IdentityRefused(_))
    }
}

impl std::fmt::Display for EstablishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IdentityRefused(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl From<String> for EstablishError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// One device's identity as its server lists it in
/// `configureddevices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedIdentity {
    /// The listed `DeviceName`.
    pub name: String,
    /// The listed `UniqueID`.
    pub unique_id: String,
}

impl ListedIdentity {
    /// The identity a client handle carries from the listing it was
    /// built from.
    #[must_use]
    pub fn of<D: Device + ?Sized>(device: &D) -> Self {
        Self {
            name: device.static_name().to_owned(),
            unique_id: device.unique_id().to_owned(),
        }
    }
}

/// Why the device at a roster entry's `device_number` was not bound.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// The server lists no device of the kind at that position.
    NotListed,
    /// The device there lists an empty `UniqueID`, so a pin cannot be
    /// checked against it.
    Unverifiable { found_name: String },
    /// The device there lists a different `UniqueID`. `expected_at` is
    /// the position the pinned one is listed at instead, if any.
    Mismatch {
        found: ListedIdentity,
        expected_at: Option<usize>,
    },
}

/// Pick the device at `device_number` among `listed` — the server's
/// devices of one kind, in its listing order — and hold it to `pin`.
///
/// Without a pin, any listed device binds, whatever its `UniqueID`.
/// With one, only a device listing that `UniqueID` binds. The listed
/// side is compared with any whitespace its driver padded it with
/// ignored; the pin itself cannot carry any (config load rejects it).
/// The pin never moves the entry to another position, it only names
/// where the pinned device is listed.
fn select(
    listed: &[ListedIdentity],
    device_number: u32,
    pin: Option<&str>,
) -> Result<usize, Refusal> {
    let index = usize::try_from(device_number).map_err(|_| Refusal::NotListed)?;
    let found = listed.get(index).ok_or(Refusal::NotListed)?;
    let Some(pin) = pin else {
        return Ok(index);
    };
    let found_id = found.unique_id.trim();
    if found_id.is_empty() {
        return Err(Refusal::Unverifiable {
            found_name: found.name.clone(),
        });
    }
    if found_id == pin {
        return Ok(index);
    }
    Err(Refusal::Mismatch {
        found: found.clone(),
        expected_at: listed
            .iter()
            .position(|other| other.unique_id.trim() == pin),
    })
}

/// Locate the roster entry's device on its Alpaca server, hold it to the
/// entry's identity pin, and switch it on: the shared routine behind
/// every kind's startup connect and the reconnect supervisor's
/// re-establish.
///
/// A device that is not listed, or whose identity the pin refuses, is a
/// permanent outcome for this routine: no in-routine retries, and the
/// supervisor re-checks it on its next pass. Nothing here actuates: the
/// listing is a read, and `Connected = true` is non-actuating by driver
/// contract.
pub(super) async fn establish_listed<C: RosterEntry>(
    config: &C,
    ca_cert_path: Option<&Path>,
) -> Result<Arc<C::Device>, EstablishError> {
    let address = config.address();
    let client = build_alpaca_client(address.alpaca_url, address.auth, ca_cert_path)
        .map_err(|e| EstablishError::Failed(format!("failed to create Alpaca client: {e}")))?;
    let pin = address.unique_id.map(UniqueIdPin::as_str);

    let label = address.label();
    let (device, identity) = retry_connect_attempt(&label, |_attempt| async {
        let devices = match tokio::time::timeout(GET_DEVICES_TIMEOUT, client.get_devices()).await {
            Ok(Ok(devices)) => devices,
            Ok(Err(e)) => return AttemptOutcome::Transient(format!("get_devices: {e}")),
            Err(_) => {
                return AttemptOutcome::Transient(format!(
                    "get_devices: timeout after {GET_DEVICES_TIMEOUT:?}"
                ));
            }
        };

        let of_this_kind: Vec<Arc<C::Device>> = devices.filter_map(C::of_kind).collect();
        let listed: Vec<ListedIdentity> = of_this_kind
            .iter()
            .map(|device| ListedIdentity::of(device.as_ref()))
            .collect();
        let bound = select(&listed, address.device_number, pin).and_then(|index| {
            of_this_kind
                .into_iter()
                .zip(listed)
                .nth(index)
                .ok_or(Refusal::NotListed)
        });
        let (device, identity) = match bound {
            Ok(pair) => pair,
            Err(refusal) => return AttemptOutcome::Permanent(address.refusal(&refusal)),
        };

        match device.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok((device, identity)),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await?;

    // Once per established session, and the line that shows which
    // physical device an entry is on, so it is worth default verbosity.
    let roster_id = address.id.unwrap_or("mount");
    let pinned = pin.is_some();
    info!(
        kind = address.kind,
        id = roster_id,
        alpaca_url = address.alpaca_url,
        device_number = address.device_number,
        device_name = %identity.name,
        unique_id = %identity.unique_id,
        pinned,
        "bound the roster entry to the device its Alpaca server lists at that number"
    );
    Ok(device)
}

/// What a reconnect pass found when it re-checked a live session's pin.
#[derive(Debug, PartialEq, Eq)]
pub enum PinCheck {
    /// The entry has no pin, or the device at its number still reports
    /// it.
    Holds,
    /// The device at its number no longer reports the pin: the live
    /// session's handle addresses another device.
    Refused(String),
    /// The listing could not be read. The session was checked when it
    /// was established, and a failed read is no evidence of a swap.
    Unchecked(String),
}

/// Re-check a pinned entry's live session against its server's
/// listing: the reconnect supervisor's healthy path, which otherwise
/// reads only `Connected`. A driver that restarts with its devices
/// reordered, followed by another client switching on the device now at
/// the entry's number, leaves `Connected` reading true through rp's
/// handle; only the listing shows the device changed.
///
/// An unpinned entry is not checked and costs no request. A read,
/// never a command.
pub(super) async fn check_pin<C: RosterEntry>(config: &C, ca_cert_path: Option<&Path>) -> PinCheck {
    let address = config.address();
    let Some(pin) = address.unique_id.map(UniqueIdPin::as_str) else {
        return PinCheck::Holds;
    };
    let client = match build_alpaca_client(address.alpaca_url, address.auth, ca_cert_path) {
        Ok(client) => client,
        Err(e) => return PinCheck::Unchecked(format!("failed to create Alpaca client: {e}")),
    };
    let devices = match tokio::time::timeout(GET_DEVICES_TIMEOUT, client.get_devices()).await {
        Ok(Ok(devices)) => devices,
        Ok(Err(e)) => return PinCheck::Unchecked(format!("get_devices: {e}")),
        Err(_) => {
            return PinCheck::Unchecked(format!(
                "get_devices: timeout after {GET_DEVICES_TIMEOUT:?}"
            ));
        }
    };
    let listed: Vec<ListedIdentity> = devices
        .filter_map(C::of_kind)
        .map(|device| ListedIdentity::of(device.as_ref()))
        .collect();
    match select(&listed, address.device_number, Some(pin)) {
        Ok(_) => PinCheck::Holds,
        Err(refusal) => PinCheck::Refused(address.refusal(&refusal).to_string()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::routing::{get, put};
    use axum::{Json, Router};

    use super::*;
    use crate::equipment::test_support::spawn_stub;

    fn listed(entries: &[(&str, &str)]) -> Vec<ListedIdentity> {
        entries
            .iter()
            .map(|(name, unique_id)| ListedIdentity {
                name: (*name).to_string(),
                unique_id: (*unique_id).to_string(),
            })
            .collect()
    }

    /// The `rig2` pair after its power cycle: the guide camera now
    /// listed first.
    fn swapped_pair() -> Vec<ListedIdentity> {
        listed(&[
            ("QHY5III678M", "QHY5III678M-guide"),
            ("QHY600M", "QHY600M-imaging"),
        ])
    }

    // ----- select: the decision, without HTTP ------------------------

    #[test]
    fn select_without_a_pin_binds_whatever_is_listed_there() {
        assert_eq!(select(&swapped_pair(), 0, None).unwrap(), 0);
    }

    #[test]
    fn select_binds_a_pin_the_listed_device_reports() {
        assert_eq!(
            select(&swapped_pair(), 1, Some("QHY600M-imaging")).unwrap(),
            1
        );
    }

    #[test]
    fn select_refuses_a_pin_the_listed_device_does_not_report_and_names_where_it_is() {
        let refusal = select(&swapped_pair(), 0, Some("QHY600M-imaging")).unwrap_err();
        assert_eq!(
            refusal,
            Refusal::Mismatch {
                found: ListedIdentity {
                    name: "QHY5III678M".to_string(),
                    unique_id: "QHY5III678M-guide".to_string(),
                },
                expected_at: Some(1),
            }
        );
    }

    #[test]
    fn select_refuses_a_pin_no_listed_device_reports() {
        let refusal = select(&swapped_pair(), 0, Some("QHY268M-gone")).unwrap_err();
        assert!(
            matches!(
                refusal,
                Refusal::Mismatch {
                    expected_at: None,
                    ..
                }
            ),
            "{refusal:?}"
        );
    }

    #[test]
    fn select_refuses_a_pin_against_an_empty_unique_id() {
        let devices = listed(&[("Anonymous", "")]);
        assert_eq!(
            select(&devices, 0, Some("QHY600M-imaging")).unwrap_err(),
            Refusal::Unverifiable {
                found_name: "Anonymous".to_string()
            }
        );
    }

    #[test]
    fn select_refuses_a_pin_against_a_blank_unique_id() {
        let devices = listed(&[("Anonymous", "  ")]);
        assert!(matches!(
            select(&devices, 0, Some("QHY600M-imaging")).unwrap_err(),
            Refusal::Unverifiable { .. }
        ));
    }

    #[test]
    fn select_without_a_pin_binds_an_empty_unique_id() {
        let devices = listed(&[("Anonymous", "")]);
        assert_eq!(select(&devices, 0, None).unwrap(), 0);
    }

    #[test]
    fn select_reports_a_missing_position_as_not_listed_pinned_or_not() {
        assert_eq!(
            select(&swapped_pair(), 2, None).unwrap_err(),
            Refusal::NotListed
        );
        assert_eq!(
            select(&swapped_pair(), 2, Some("QHY600M-imaging")).unwrap_err(),
            Refusal::NotListed
        );
    }

    /// A `UniqueID` differing only in case is a different device.
    #[test]
    fn select_compares_the_pin_case_sensitively() {
        let refusal = select(&swapped_pair(), 1, Some("qhy600m-imaging")).unwrap_err();
        assert!(matches!(refusal, Refusal::Mismatch { .. }), "{refusal:?}");
    }

    /// A driver that pads its listed `UniqueID` can still be pinned: the
    /// pin cannot carry the padding, so the listed side drops it.
    #[test]
    fn select_ignores_padding_on_the_listed_unique_id() {
        let devices = listed(&[
            ("QHY5III678M", "QHY5III678M-guide"),
            ("QHY600M", " QHY600M-imaging "),
        ]);
        assert_eq!(select(&devices, 1, Some("QHY600M-imaging")).unwrap(), 1);
        assert!(matches!(
            select(&devices, 0, Some("QHY600M-imaging")).unwrap_err(),
            Refusal::Mismatch {
                expected_at: Some(1),
                ..
            }
        ));
    }

    // ----- refusal messages and their class --------------------------

    fn camera_address(pin: Option<&UniqueIdPin>) -> RosterAddress<'_> {
        RosterAddress {
            kind: "camera",
            id: Some("qhy600m"),
            alpaca_url: "http://127.0.0.1:1",
            device_number: 0,
            unique_id: pin,
            auth: None,
        }
    }

    fn pin(value: &str) -> UniqueIdPin {
        UniqueIdPin::try_new(value.to_string()).unwrap()
    }

    #[test]
    fn a_mismatch_names_both_identities_and_where_the_pinned_one_is_listed() {
        let expected = pin("QHY600M-imaging");
        let refusal = select(&swapped_pair(), 0, Some(expected.as_str())).unwrap_err();
        let error = camera_address(Some(&expected)).refusal(&refusal);
        assert_eq!(
            error,
            EstablishError::IdentityRefused(
                "camera at device_number 0 is \"QHY5III678M\" with UniqueID \
                 \"QHY5III678M-guide\", not the pinned unique_id \"QHY600M-imaging\"; \
                 refusing to connect it: \"QHY600M-imaging\" is listed at device_number 1 \
                 on this server; set this entry's device_number to 1 if the device moved"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_mismatch_with_the_pin_listed_nowhere_says_so() {
        let expected = pin("QHY268M-gone");
        let refusal = select(&swapped_pair(), 0, Some(expected.as_str())).unwrap_err();
        let msg = camera_address(Some(&expected))
            .refusal(&refusal)
            .to_string();
        assert!(
            msg.ends_with("no camera on this server reports \"QHY268M-gone\""),
            "{msg}"
        );
    }

    #[test]
    fn an_unverifiable_pin_is_an_identity_refusal_that_says_why() {
        let expected = pin("QHY600M-imaging");
        let refusal =
            select(&listed(&[("Anonymous", "")]), 0, Some(expected.as_str())).unwrap_err();
        assert_eq!(
            camera_address(Some(&expected)).refusal(&refusal),
            EstablishError::IdentityRefused(
                "camera at device_number 0 (\"Anonymous\") reports an empty UniqueID, so the \
                 pinned unique_id \"QHY600M-imaging\" cannot be verified; refusing to connect it"
                    .to_string()
            )
        );
    }

    /// "Not listed" keeps its old class and wording: it is no identity
    /// refusal, so an unpinned entry's outcome is unchanged.
    #[test]
    fn a_missing_position_is_a_plain_failure_with_the_not_found_wording() {
        assert_eq!(
            camera_address(None).refusal(&Refusal::NotListed),
            EstablishError::Failed("camera at index 0 not found on Alpaca server".to_string())
        );
    }

    #[test]
    fn the_label_names_the_kind_and_the_roster_id() {
        assert_eq!(camera_address(None).label(), "camera qhy600m");
        let mount = RosterAddress {
            kind: "mount",
            id: None,
            ..camera_address(None)
        };
        assert_eq!(mount.label(), "mount");
    }

    // ----- establish_listed and check_pin against a stub server ------

    fn ok_envelope() -> Json<serde_json::Value> {
        Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""}))
    }

    /// The `rig2` pair after its power cycle, served by one stub: the
    /// guide camera listed first, the imaging camera second. Every
    /// `Connected = true` either camera receives is counted.
    async fn swapped_pair_server(
        connects: Arc<AtomicU32>,
    ) -> crate::equipment::test_support::AlpacaStub {
        let connect0 = Arc::clone(&connects);
        let connect1 = connects;
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {"DeviceName": "QHY5III678M", "DeviceType": "Camera",
                             "DeviceNumber": 0, "UniqueID": "QHY5III678M-guide"},
                            {"DeviceName": "QHY600M", "DeviceType": "Camera",
                             "DeviceNumber": 1, "UniqueID": "QHY600M-imaging"}
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(move || {
                    let connects = Arc::clone(&connect0);
                    async move {
                        connects.fetch_add(1, Ordering::SeqCst);
                        ok_envelope()
                    }
                }),
            )
            .route(
                "/api/v1/camera/1/connected",
                put(move || {
                    let connects = Arc::clone(&connect1);
                    async move {
                        connects.fetch_add(1, Ordering::SeqCst);
                        ok_envelope()
                    }
                }),
            );
        spawn_stub(app).await
    }

    fn camera_config(
        id: &str,
        url: &str,
        device_number: u32,
        pin: Option<&str>,
    ) -> config::CameraConfig {
        config::CameraConfig {
            id: id.to_string(),
            name: String::new(),
            alpaca_url: url.to_string(),
            device_type: String::new(),
            device_number,
            unique_id: pin.map(|p| UniqueIdPin::try_new(p.to_string()).unwrap()),
            cooler_targets_c: Vec::new(),
            gain: None,
            offset: None,
            readout_time_estimate: None,
            auth: None,
        }
    }

    /// The `rig2` failure itself: the pinned imaging camera's number
    /// now lists the guide camera. The connect is refused as an identity
    /// refusal before `Connected = true` reaches either camera, and the
    /// error names where the pinned camera is listed now.
    #[tokio::test]
    async fn a_swapped_camera_is_refused_without_being_switched_on() {
        let connects = Arc::new(AtomicU32::new(0));
        let stub = swapped_pair_server(Arc::clone(&connects)).await;
        let config = camera_config("qhy600m", &stub.url(), 0, Some("QHY600M-imaging"));

        let err = establish_listed(&config, None).await.unwrap_err();

        assert!(err.is_identity_refusal(), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("\"QHY5III678M-guide\""), "{msg}");
        assert!(
            msg.contains("set this entry's device_number to 1"),
            "the error must name where the pinned camera is listed: {msg}"
        );
        assert_eq!(
            connects.load(Ordering::SeqCst),
            0,
            "a refused device must never be switched on"
        );
    }

    #[tokio::test]
    async fn a_pinned_camera_at_its_own_number_binds() {
        let connects = Arc::new(AtomicU32::new(0));
        let stub = swapped_pair_server(Arc::clone(&connects)).await;
        let config = camera_config("qhy600m", &stub.url(), 1, Some("QHY600M-imaging"));

        let camera = establish_listed(&config, None).await.unwrap();

        assert_eq!(
            ListedIdentity::of(camera.as_ref()).unique_id,
            "QHY600M-imaging"
        );
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    /// Without a pin the entry binds whatever its number lists — the
    /// behaviour the pin exists to guard against, kept for unpinned
    /// entries.
    #[tokio::test]
    async fn an_unpinned_camera_binds_whatever_its_number_lists() {
        let connects = Arc::new(AtomicU32::new(0));
        let stub = swapped_pair_server(Arc::clone(&connects)).await;
        let config = camera_config("qhy600m", &stub.url(), 0, None);

        let camera = establish_listed(&config, None).await.unwrap();

        assert_eq!(ListedIdentity::of(camera.as_ref()).name, "QHY5III678M");
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    /// The equipment status names the device each live entry is bound
    /// to, and nothing for an entry that is not — here one refused by
    /// its pin.
    #[tokio::test]
    async fn the_status_names_the_device_each_entry_is_bound_to() {
        let stub = swapped_pair_server(Arc::new(AtomicU32::new(0))).await;
        let url = stub.url();
        let equipment = config::EquipmentConfig {
            cameras: vec![
                camera_config("guide", &url, 0, None),
                camera_config("imaging", &url, 0, Some("QHY600M-imaging")),
            ],
            ..Default::default()
        };
        let registry = crate::equipment::EquipmentRegistry::new(&equipment, None).await;

        let status = serde_json::to_value(registry.status()).unwrap();
        assert_eq!(
            status["cameras"],
            serde_json::json!([
                {"id": "guide", "connected": true,
                 "device_name": "QHY5III678M", "unique_id": "QHY5III678M-guide"},
                {"id": "imaging", "connected": false,
                 "device_name": null, "unique_id": null}
            ])
        );
    }

    /// A dead session's stale handle still names the device it was
    /// built for, but rp is no longer bound to it, so it reports none.
    #[tokio::test]
    async fn a_dead_session_reports_no_bound_identity() {
        let stub = swapped_pair_server(Arc::new(AtomicU32::new(0))).await;
        let config = camera_config("qhy600m", &stub.url(), 1, None);
        let camera = establish_listed(&config, None).await.unwrap();
        let session: crate::equipment::DeviceSession<dyn Camera> =
            crate::equipment::DeviceSession::connected(camera);
        assert_eq!(
            session.bound_identity().unwrap().unique_id,
            "QHY600M-imaging"
        );

        session.mark_disconnected();
        assert_eq!(session.bound_identity(), None);
    }

    /// A refusal is permanent for the routine: one listing read, no
    /// in-routine retries with backoff (the supervisor's next pass is
    /// the retry).
    #[tokio::test]
    async fn a_refusal_is_not_retried_within_the_routine() {
        let reads = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&reads);
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(move || {
                let reads = Arc::clone(&counted);
                async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "Value": [{"DeviceName": "QHY5III678M", "DeviceType": "Camera",
                                   "DeviceNumber": 0, "UniqueID": "QHY5III678M-guide"}],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }
            }),
        );
        let stub = spawn_stub(app).await;
        let config = camera_config("qhy600m", &stub.url(), 0, Some("QHY600M-imaging"));

        let err = establish_listed(&config, None).await.unwrap_err();

        assert!(err.is_identity_refusal(), "{err:?}");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn check_pin_holds_while_the_pinned_device_is_at_its_number() {
        let stub = swapped_pair_server(Arc::new(AtomicU32::new(0))).await;
        let config = camera_config("qhy600m", &stub.url(), 1, Some("QHY600M-imaging"));
        assert_eq!(check_pin(&config, None).await, PinCheck::Holds);
    }

    #[tokio::test]
    async fn check_pin_refuses_once_another_device_is_at_its_number() {
        let stub = swapped_pair_server(Arc::new(AtomicU32::new(0))).await;
        let config = camera_config("qhy600m", &stub.url(), 0, Some("QHY600M-imaging"));
        let PinCheck::Refused(msg) = check_pin(&config, None).await else {
            panic!("a swapped device must refuse the pin");
        };
        assert!(msg.contains("set this entry's device_number to 1"), "{msg}");
    }

    /// An unpinned entry is never checked: pointed at a server that
    /// cannot be reached, it still holds, because no request is made.
    #[tokio::test]
    async fn check_pin_makes_no_request_for_an_unpinned_entry() {
        let config = camera_config("qhy600m", "http://127.0.0.1:1", 0, None);
        assert_eq!(check_pin(&config, None).await, PinCheck::Holds);
    }

    /// A listing that cannot be read is no evidence of a swap.
    #[tokio::test]
    async fn check_pin_leaves_an_unreadable_listing_unchecked() {
        let config = camera_config("qhy600m", "http://127.0.0.1:1", 0, Some("QHY600M-imaging"));
        assert!(
            matches!(check_pin(&config, None).await, PinCheck::Unchecked(_)),
            "an unreachable server must leave the pin unchecked"
        );
    }
}

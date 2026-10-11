//! The connect routine every equipment kind shares.
//!
//! It locates a roster entry's device in its server's
//! `configureddevices` list, checks the entry's pinned `UniqueID`,
//! switches the device on, and logs what was bound (rp.md § Device
//! Identity Pin, § Device Session Recovery).
//!
//! The identity checked is read from the same listing the device is
//! located in, so the check and the bind cannot disagree about which
//! device they mean, and the pin costs no extra request. The check runs
//! before `Connected = true`, so a refused device is never switched on.

use std::path::Path;
use std::sync::Arc;

use ascom_alpaca::api::{Device, TypedDevice};
use rp_auth::config::ClientAuthConfig;
use tracing::info;

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config::UniqueIdPin;

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

    /// The operator-facing reason a device was not bound.
    fn refusal_message(&self, refusal: &Refusal) -> String {
        let kind = self.kind;
        let number = self.device_number;
        let pin = self.unique_id.map_or("", UniqueIdPin::as_str);
        match refusal {
            Refusal::NotListed => {
                format!("{kind} at index {number} not found on Alpaca server")
            }
            Refusal::Unverifiable { found_name } => format!(
                "{kind} at device_number {number} ({found_name:?}) reports an empty UniqueID, \
                 so the pinned unique_id {pin:?} cannot be verified; refusing to connect it"
            ),
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
                format!(
                    "{kind} at device_number {number} is {found_name:?} with UniqueID \
                     {found_id:?}, not the pinned unique_id {pin:?}; refusing to connect it: \
                     {elsewhere}"
                )
            }
        }
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
/// With one, only a device listing exactly that `UniqueID` binds; the
/// pin never moves the entry to another position, it only names where
/// the pinned device is listed.
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
    if found.unique_id.is_empty() {
        return Err(Refusal::Unverifiable {
            found_name: found.name.clone(),
        });
    }
    if found.unique_id == pin {
        return Ok(index);
    }
    Err(Refusal::Mismatch {
        found: found.clone(),
        expected_at: listed.iter().position(|other| other.unique_id == pin),
    })
}

/// Locate the roster entry's device on its Alpaca server, hold it to the
/// entry's identity pin, and switch it on: the shared routine behind
/// every kind's startup connect and the reconnect supervisor's
/// re-establish.
///
/// `of_kind` picks the entry's kind out of the server's listing. A
/// device that is not listed, or whose identity the pin refuses, is a
/// permanent outcome for this routine: no in-routine retries, and the
/// supervisor re-checks it on its next pass. Nothing here actuates:
/// the listing is a read, and `Connected = true` is non-actuating by
/// driver contract.
pub(super) async fn establish_listed<D>(
    address: &RosterAddress<'_>,
    ca_cert_path: Option<&Path>,
    of_kind: fn(TypedDevice) -> Option<Arc<D>>,
) -> Result<Arc<D>, String>
where
    D: Device + ?Sized,
{
    let client = build_alpaca_client(address.alpaca_url, address.auth, ca_cert_path)
        .map_err(|e| format!("failed to create Alpaca client: {e}"))?;
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

        let of_this_kind: Vec<Arc<D>> = devices.filter_map(of_kind).collect();
        let listed: Vec<ListedIdentity> = of_this_kind
            .iter()
            .map(|device| ListedIdentity::of(device.as_ref()))
            .collect();
        let index = match select(&listed, address.device_number, pin) {
            Ok(index) => index,
            Err(refusal) => {
                return AttemptOutcome::Permanent(address.refusal_message(&refusal));
            }
        };
        let Some((device, identity)) = of_this_kind.into_iter().zip(listed).nth(index) else {
            return AttemptOutcome::Permanent(address.refusal_message(&Refusal::NotListed));
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use ascom_alpaca::api::Camera;
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

    /// The comparison is verbatim: a `UniqueID` differing only in case
    /// is a different device.
    #[test]
    fn select_compares_the_pin_verbatim() {
        let refusal = select(&swapped_pair(), 1, Some("qhy600m-imaging")).unwrap_err();
        assert!(matches!(refusal, Refusal::Mismatch { .. }), "{refusal:?}");
    }

    // ----- refusal messages ------------------------------------------

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
        let msg = camera_address(Some(&expected)).refusal_message(&refusal);
        assert_eq!(
            msg,
            "camera at device_number 0 is \"QHY5III678M\" with UniqueID \"QHY5III678M-guide\", \
             not the pinned unique_id \"QHY600M-imaging\"; refusing to connect it: \
             \"QHY600M-imaging\" is listed at device_number 1 on this server; \
             set this entry's device_number to 1 if the device moved"
        );
    }

    #[test]
    fn a_mismatch_with_the_pin_listed_nowhere_says_so() {
        let expected = pin("QHY268M-gone");
        let refusal = select(&swapped_pair(), 0, Some(expected.as_str())).unwrap_err();
        let msg = camera_address(Some(&expected)).refusal_message(&refusal);
        assert!(
            msg.ends_with("no camera on this server reports \"QHY268M-gone\""),
            "{msg}"
        );
    }

    #[test]
    fn an_unverifiable_pin_says_why() {
        let expected = pin("QHY600M-imaging");
        let refusal =
            select(&listed(&[("Anonymous", "")]), 0, Some(expected.as_str())).unwrap_err();
        let msg = camera_address(Some(&expected)).refusal_message(&refusal);
        assert_eq!(
            msg,
            "camera at device_number 0 (\"Anonymous\") reports an empty UniqueID, so the \
             pinned unique_id \"QHY600M-imaging\" cannot be verified; refusing to connect it"
        );
    }

    #[test]
    fn a_missing_position_keeps_the_not_found_wording() {
        let msg = camera_address(None).refusal_message(&Refusal::NotListed);
        assert_eq!(msg, "camera at index 0 not found on Alpaca server");
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

    // ----- establish_listed against a stub server --------------------

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

    fn camera_of(device: TypedDevice) -> Option<Arc<dyn Camera>> {
        match device {
            TypedDevice::Camera(camera) => Some(camera),
            _ => None,
        }
    }

    fn stub_address<'a>(
        url: &'a str,
        device_number: u32,
        pin: Option<&'a UniqueIdPin>,
    ) -> RosterAddress<'a> {
        RosterAddress {
            kind: "camera",
            id: Some("qhy600m"),
            alpaca_url: url,
            device_number,
            unique_id: pin,
            auth: None,
        }
    }

    /// The `rig2` failure itself: the pinned imaging camera's number
    /// now lists the guide camera. The connect is refused before
    /// `Connected = true` reaches either camera, and the error names
    /// where the pinned camera is listed now.
    #[tokio::test]
    async fn a_swapped_camera_is_refused_without_being_switched_on() {
        let connects = Arc::new(AtomicU32::new(0));
        let stub = swapped_pair_server(Arc::clone(&connects)).await;
        let url = stub.url();
        let expected = pin("QHY600M-imaging");

        let err = establish_listed(&stub_address(&url, 0, Some(&expected)), None, camera_of)
            .await
            .unwrap_err();

        assert!(err.contains("\"QHY5III678M-guide\""), "{err}");
        assert!(
            err.contains("set this entry's device_number to 1"),
            "the error must name where the pinned camera is listed: {err}"
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
        let url = stub.url();
        let expected = pin("QHY600M-imaging");

        let camera = establish_listed(&stub_address(&url, 1, Some(&expected)), None, camera_of)
            .await
            .unwrap();

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
        let url = stub.url();

        let camera = establish_listed(&stub_address(&url, 0, None), None, camera_of)
            .await
            .unwrap();

        assert_eq!(ListedIdentity::of(camera.as_ref()).name, "QHY5III678M");
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    fn camera_config(
        id: &str,
        url: &str,
        device_number: u32,
        pin: Option<&str>,
    ) -> crate::config::CameraConfig {
        crate::config::CameraConfig {
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

    /// The equipment status names the device each live entry is bound
    /// to, and nothing for an entry that is not — here one refused by
    /// its pin.
    #[tokio::test]
    async fn the_status_names_the_device_each_entry_is_bound_to() {
        let stub = swapped_pair_server(Arc::new(AtomicU32::new(0))).await;
        let url = stub.url();
        let equipment = crate::config::EquipmentConfig {
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
        let url = stub.url();
        let camera = establish_listed(&stub_address(&url, 1, None), None, camera_of)
            .await
            .unwrap();
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
        let url = stub.url();
        let expected = pin("QHY600M-imaging");

        let err = establish_listed(&stub_address(&url, 0, Some(&expected)), None, camera_of)
            .await
            .unwrap_err();

        assert!(err.contains("refusing to connect it"), "{err}");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }
}

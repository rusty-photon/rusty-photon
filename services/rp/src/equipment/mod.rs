//! Equipment registry and per-device-type Alpaca connect logic.
//!
//! [`EquipmentRegistry`] is the gateway's runtime equipment surface: a
//! flat collection of per-device entries plus a singular `Option<MountEntry>`.
//! It is built from a [`crate::config::EquipmentConfig`] at startup; each
//! per-device-type connect routine lives in its own submodule
//! ([`camera`], [`filter_wheel`], [`cover_calibrator`], [`focuser`],
//! [`mount`], [`safety_monitor`], [`switch`], [`rotator`],
//! [`observing_conditions`], [`dome`]). Generic Alpaca-client glue (HTTP
//! basic-auth header, retry/backoff with `Permanent`/`Transient` outcomes)
//! lives in [`alpaca`]; the per-entry session slot in [`session`]; the
//! reconnect supervisor that heals dead sessions (rp.md § Device
//! Session Recovery) in [`supervisor`].
//!
//! `rotator` has its own tools, and `switch` its power tools through the
//! power map ([`power`]); `observing_conditions` and `dome` cover roster
//! membership and connectivity status only (rp.md § Equipment
//! Integration).
//!
//! [`EntryRef`] names one entry of any kind, for the code that acts on
//! an entry by its id alone: the reconnect supervisor's walk,
//! `disconnect_device` and `connect_device`, and the power map.
//!
//! Every kind's connect routine is the shared one in [`binding`]:
//! locate the device in the server's `configureddevices` list, hold it
//! to the entry's identity pin (rp.md § Device Identity Pin), switch it
//! on, and log what was bound.
//!
//! The submodules' `*Entry` types and shared status types are
//! re-exported here so existing `crate::equipment::CameraEntry` etc.
//! callsites keep working unchanged.

pub mod alpaca;
pub mod binding;
pub mod camera;
pub mod cover_calibrator;
pub mod dome;
pub mod filter_wheel;
pub mod focuser;
pub mod mount;
pub mod observing_conditions;
pub mod power;
pub mod rotator;
pub mod safety_monitor;
pub mod session;
pub mod slot;
pub mod supervisor;
pub mod switch;
pub mod trains;

#[cfg(test)]
pub(crate) mod test_support;

pub use camera::{CameraEntry, CameraInvariants};
pub use cover_calibrator::CoverCalibratorEntry;
pub use dome::DomeEntry;
pub use filter_wheel::{FilterWheelEntry, FilterWheelInvariants};
pub use focuser::{FocuserEntry, FocuserInvariants};
pub use mount::MountEntry;
pub use observing_conditions::ObservingConditionsEntry;
pub use rotator::{RotatorEntry, RotatorInvariants};
pub use safety_monitor::SafetyMonitorEntry;
pub use session::DeviceSession;
pub use slot::{DeviceSlot, SlotGuard};
pub use supervisor::ReconnectSupervisor;
pub use switch::SwitchEntry;

use ascom_alpaca::api::Device;
use serde::Serialize;
use tracing::debug;

use crate::config;
use crate::error::RpError;

#[derive(Default)]
pub struct EquipmentRegistry {
    /// The observatory CA every device's Alpaca client trusts, for a
    /// connect made after startup (`connect_device`).
    pub ca_cert_path: Option<std::path::PathBuf>,
    /// Taken by `set_switch` across its hold-out check and its write,
    /// and by `connect_device` while it releases a hold-out, so no
    /// device is connected between the check and the cut (rp.md § Power
    /// Switching).
    pub power: tokio::sync::Mutex<()>,
    pub cameras: Vec<CameraEntry>,
    pub filter_wheels: Vec<FilterWheelEntry>,
    pub cover_calibrators: Vec<CoverCalibratorEntry>,
    pub focusers: Vec<FocuserEntry>,
    pub safety_monitors: Vec<SafetyMonitorEntry>,
    pub switches: Vec<SwitchEntry>,
    pub rotators: Vec<RotatorEntry>,
    pub observing_conditions: Vec<ObservingConditionsEntry>,
    pub domes: Vec<DomeEntry>,
    pub mount: Option<MountEntry>,
}

#[derive(Serialize)]
pub struct EquipmentStatus {
    pub cameras: Vec<DeviceStatus>,
    pub filter_wheels: Vec<DeviceStatus>,
    pub cover_calibrators: Vec<DeviceStatus>,
    pub focusers: Vec<DeviceStatus>,
    pub safety_monitors: Vec<DeviceStatus>,
    pub switches: Vec<DeviceStatus>,
    pub rotators: Vec<DeviceStatus>,
    pub observing_conditions: Vec<DeviceStatus>,
    pub domes: Vec<DeviceStatus>,
    pub mount: Option<MountStatus>,
}

/// One roster entry's live state (rp.md § Equipment).
#[derive(Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub connected: bool,
    /// Whether `disconnect_device` holds the entry out of the reconnect
    /// supervisor (rp.md § Power Switching).
    pub held_out: bool,
    /// The bound device's `DeviceName` as listed when the session was
    /// established; `None` while disconnected. A pinned entry's is
    /// re-checked every reconnect pass, an unpinned one's is not.
    pub device_name: Option<String>,
    /// The bound device's `UniqueID` as listed when the session was
    /// established; `None` while disconnected. Same freshness as
    /// `device_name`.
    pub unique_id: Option<String>,
}

impl DeviceStatus {
    /// The entry's id beside its session's state: the mount's shape
    /// plus the id.
    fn of<T: Device + ?Sized, M>(id: &str, session: &DeviceSession<T, M>) -> Self {
        let MountStatus {
            connected,
            held_out,
            device_name,
            unique_id,
        } = MountStatus::of(session);
        Self {
            id: id.to_string(),
            connected,
            held_out,
            device_name,
            unique_id,
        }
    }
}

/// Singular wire-format counterpart to [`MountEntry`] — no `id`.
#[derive(Serialize)]
pub struct MountStatus {
    pub connected: bool,
    /// Whether `disconnect_device` holds the mount out of the reconnect
    /// supervisor.
    pub held_out: bool,
    /// The bound device's listed `DeviceName`; `None` while disconnected.
    pub device_name: Option<String>,
    /// The bound device's listed `UniqueID`; `None` while disconnected.
    pub unique_id: Option<String>,
}

impl MountStatus {
    /// The connected flag and the bound identity, from one read of the
    /// session (a live session always holds its handle).
    fn of<T: Device + ?Sized, M>(session: &DeviceSession<T, M>) -> Self {
        let (identity, held_out) = session.bound_identity_and_hold_out();
        let connected = identity.is_some();
        let (device_name, unique_id) = identity.map(|i| (i.name, i.unique_id)).unzip();
        Self {
            connected,
            held_out,
            device_name,
            unique_id,
        }
    }
}

/// Hard tolerance for the lat/lon mismatch check. 0.01° ≈ 1 km on the
/// ground — finer than any operator would set deliberately, well above
/// numerical noise on either side. Not configurable.
pub const SITE_MATCH_TOLERANCE_DEG: f64 = 0.01;

impl EquipmentRegistry {
    /// `ca_cert_path` is the observatory CA (`Config::ca_cert_path`,
    /// rp.md §Configuration), forwarded to every device's Alpaca client so
    /// an `https://` `alpaca_url` signed by that CA verifies (issue #609).
    pub async fn new(
        equipment_config: &config::EquipmentConfig,
        ca_cert_path: Option<&std::path::Path>,
    ) -> Self {
        let mut cameras = Vec::new();
        let mut filter_wheels = Vec::new();
        let mut cover_calibrators = Vec::new();
        let mut focusers = Vec::new();

        for cam_config in &equipment_config.cameras {
            let entry = camera::connect_camera(cam_config, ca_cert_path).await;
            cameras.push(entry);
        }

        for fw_config in &equipment_config.filter_wheels {
            let entry = filter_wheel::connect_filter_wheel(fw_config, ca_cert_path).await;
            filter_wheels.push(entry);
        }

        for cc_config in &equipment_config.cover_calibrators {
            let entry = cover_calibrator::connect_cover_calibrator(cc_config, ca_cert_path).await;
            cover_calibrators.push(entry);
        }

        for foc_config in &equipment_config.focusers {
            let entry = focuser::connect_focuser(foc_config, ca_cert_path).await;
            focusers.push(entry);
        }

        let mut safety_monitors = Vec::new();
        for sm_config in &equipment_config.safety_monitors {
            let entry = safety_monitor::connect_safety_monitor(sm_config, ca_cert_path).await;
            safety_monitors.push(entry);
        }

        let mut switches = Vec::new();
        for switch_config in &equipment_config.switches {
            let entry = switch::connect_switch(switch_config, ca_cert_path).await;
            switches.push(entry);
        }

        let mut rotators = Vec::new();
        for rotator_config in &equipment_config.rotators {
            let entry = rotator::connect_rotator(rotator_config, ca_cert_path).await;
            rotators.push(entry);
        }

        let mut observing_conditions = Vec::new();
        for oc_config in &equipment_config.observing_conditions {
            let entry =
                observing_conditions::connect_observing_conditions(oc_config, ca_cert_path).await;
            observing_conditions.push(entry);
        }

        let mut domes = Vec::new();
        for dome_config in &equipment_config.domes {
            let entry = dome::connect_dome(dome_config, ca_cert_path).await;
            domes.push(entry);
        }

        let mount = match &equipment_config.mount {
            Some(mount_config) => Some(mount::connect_mount(mount_config, ca_cert_path).await),
            None => None,
        };

        Self {
            ca_cert_path: ca_cert_path.map(std::path::Path::to_path_buf),
            power: tokio::sync::Mutex::new(()),
            cameras,
            filter_wheels,
            cover_calibrators,
            focusers,
            safety_monitors,
            switches,
            rotators,
            observing_conditions,
            domes,
            mount,
        }
    }

    /// Every entry, in the reconnect supervisor's order: the imaging
    /// chain (cameras, filter wheels, cover calibrators, focusers, safety
    /// monitors), then switches, rotators, observing-conditions devices,
    /// domes and the mount.
    pub fn entries(&self) -> impl Iterator<Item = EntryRef<'_>> {
        self.cameras
            .iter()
            .map(EntryRef::Camera)
            .chain(self.filter_wheels.iter().map(EntryRef::FilterWheel))
            .chain(self.cover_calibrators.iter().map(EntryRef::CoverCalibrator))
            .chain(self.focusers.iter().map(EntryRef::Focuser))
            .chain(self.safety_monitors.iter().map(EntryRef::SafetyMonitor))
            .chain(self.switches.iter().map(EntryRef::Switch))
            .chain(self.rotators.iter().map(EntryRef::Rotator))
            .chain(
                self.observing_conditions
                    .iter()
                    .map(EntryRef::ObservingConditions),
            )
            .chain(self.domes.iter().map(EntryRef::Dome))
            .chain(self.mount.iter().map(EntryRef::Mount))
    }

    /// The entry `id` names: a roster id, or [`config::MOUNT_ID`] for the
    /// mount. Roster ids are unique across kinds and none is the mount's
    /// (rp.md § Power Switching), so at most one entry matches.
    #[must_use]
    pub fn entry(&self, id: &str) -> Option<EntryRef<'_>> {
        self.entries().find(|entry| entry.id() == id)
    }

    #[must_use]
    pub fn status(&self) -> EquipmentStatus {
        EquipmentStatus {
            cameras: self
                .cameras
                .iter()
                .map(|c| DeviceStatus::of(&c.id, &c.session))
                .collect(),
            filter_wheels: self
                .filter_wheels
                .iter()
                .map(|fw| DeviceStatus::of(&fw.id, &fw.session))
                .collect(),
            cover_calibrators: self
                .cover_calibrators
                .iter()
                .map(|cc| DeviceStatus::of(&cc.id, &cc.session))
                .collect(),
            focusers: self
                .focusers
                .iter()
                .map(|f| DeviceStatus::of(&f.id, &f.session))
                .collect(),
            safety_monitors: self
                .safety_monitors
                .iter()
                .map(|sm| DeviceStatus::of(&sm.id, &sm.session))
                .collect(),
            switches: self
                .switches
                .iter()
                .map(|sw| DeviceStatus::of(&sw.id, &sw.session))
                .collect(),
            rotators: self
                .rotators
                .iter()
                .map(|r| DeviceStatus::of(&r.id, &r.session))
                .collect(),
            observing_conditions: self
                .observing_conditions
                .iter()
                .map(|oc| DeviceStatus::of(&oc.id, &oc.session))
                .collect(),
            domes: self
                .domes
                .iter()
                .map(|d| DeviceStatus::of(&d.id, &d.session))
                .collect(),
            mount: self.mount.as_ref().map(|m| MountStatus::of(&m.session)),
        }
    }

    #[must_use]
    pub fn find_camera(&self, id: &str) -> Option<&CameraEntry> {
        self.cameras.iter().find(|c| c.id == id)
    }

    #[must_use]
    pub fn find_filter_wheel(&self, id: &str) -> Option<&FilterWheelEntry> {
        self.filter_wheels.iter().find(|fw| fw.id == id)
    }

    #[must_use]
    pub fn find_cover_calibrator(&self, id: &str) -> Option<&CoverCalibratorEntry> {
        self.cover_calibrators.iter().find(|cc| cc.id == id)
    }

    #[must_use]
    pub fn find_focuser(&self, id: &str) -> Option<&FocuserEntry> {
        self.focusers.iter().find(|f| f.id == id)
    }

    #[must_use]
    pub fn find_safety_monitor(&self, id: &str) -> Option<&SafetyMonitorEntry> {
        self.safety_monitors.iter().find(|sm| sm.id == id)
    }

    #[must_use]
    pub fn find_switch(&self, id: &str) -> Option<&SwitchEntry> {
        self.switches.iter().find(|sw| sw.id == id)
    }

    #[must_use]
    pub fn find_rotator(&self, id: &str) -> Option<&RotatorEntry> {
        self.rotators.iter().find(|r| r.id == id)
    }

    #[must_use]
    pub fn find_observing_conditions(&self, id: &str) -> Option<&ObservingConditionsEntry> {
        self.observing_conditions.iter().find(|oc| oc.id == id)
    }

    #[must_use]
    pub fn find_dome(&self, id: &str) -> Option<&DomeEntry> {
        self.domes.iter().find(|d| d.id == id)
    }

    /// Returns the singular mount entry, or `None` when no mount is
    /// configured. Singular: there is no `id` parameter.
    #[must_use]
    pub const fn find_mount(&self) -> Option<&MountEntry> {
        self.mount.as_ref()
    }

    /// Validate the configured site against the mount's reported
    /// `SiteLatitude`/`SiteLongitude`.
    ///
    /// `Ok(())` when no site is configured, no mount is connected, the
    /// mount lacks the property (any read error → debug-log skip), or
    /// the values agree to within `SITE_MATCH_TOLERANCE_DEG`.
    ///
    /// ASCOM does **not** expose a `CanGetSiteLatitude` capability
    /// bit — the read attempt itself is the capability probe, and
    /// `NOT_IMPLEMENTED` (or any other ASCOM error) is treated as
    /// "skip validation" rather than "fail loud".
    ///
    /// # Errors
    ///
    /// Returns [`RpError::SiteMismatch`] when both sides expose values
    /// and they disagree past the tolerance — the only failure.
    pub async fn validate_site(
        &self,
        site: Option<&config::SiteConfig>,
    ) -> crate::error::Result<()> {
        let Some(site) = site else {
            debug!("no site configured; skipping mount-side site validation");
            return Ok(());
        };
        let Some(mount) = self.mount.as_ref() else {
            debug!("no mount configured; skipping mount-side site validation");
            return Ok(());
        };
        if !mount.is_connected() {
            debug!("mount not connected; skipping mount-side site validation");
            return Ok(());
        }
        let Some(t) = mount.device() else {
            debug!("mount entry has no device handle; skipping site validation");
            return Ok(());
        };

        let mount_lat = match t.site_latitude().await {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    error = %e,
                    "mount did not report SiteLatitude; skipping mount-side site validation"
                );
                return Ok(());
            }
        };
        let mount_lon = match t.site_longitude().await {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    error = %e,
                    "mount did not report SiteLongitude; skipping mount-side site validation"
                );
                return Ok(());
            }
        };

        let lat_diff = (mount_lat - site.latitude_degrees).abs();
        // Longitude is angular: 179.99° E and -179.99° E are the
        // same meridian, not 360° apart. Take the modular distance
        // around 360° so the antimeridian doesn't trigger a false
        // mismatch.
        let lon_raw = (mount_lon - site.longitude_degrees).abs();
        let lon_diff = lon_raw.min(360.0 - lon_raw);
        if lat_diff > SITE_MATCH_TOLERANCE_DEG || lon_diff > SITE_MATCH_TOLERANCE_DEG {
            return Err(RpError::SiteMismatch {
                config_lat: site.latitude_degrees,
                config_lon: site.longitude_degrees,
                mount_lat,
                mount_lon,
            });
        }
        debug!(
            site_lat = site.latitude_degrees,
            site_lon = site.longitude_degrees,
            "mount-side site validation: configured site agrees with mount"
        );
        Ok(())
    }
}

/// One entry of the registry, of any kind (see the module docs).
#[derive(Clone, Copy)]
pub enum EntryRef<'a> {
    Camera(&'a CameraEntry),
    FilterWheel(&'a FilterWheelEntry),
    CoverCalibrator(&'a CoverCalibratorEntry),
    Focuser(&'a FocuserEntry),
    SafetyMonitor(&'a SafetyMonitorEntry),
    Switch(&'a SwitchEntry),
    Rotator(&'a RotatorEntry),
    ObservingConditions(&'a ObservingConditionsEntry),
    Dome(&'a DomeEntry),
    Mount(&'a MountEntry),
}

/// Run `$body` with `$session` bound to the entry's session slot,
/// whatever its kind: the slot's methods are generic, so one body
/// serves every arm.
macro_rules! with_session {
    ($entry:expr, $session:ident => $body:expr) => {
        match $entry {
            $crate::equipment::EntryRef::Camera(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::FilterWheel(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::CoverCalibrator(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::Focuser(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::SafetyMonitor(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::Switch(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::Rotator(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::ObservingConditions(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::Dome(e) => {
                let $session = &e.session;
                $body
            }
            $crate::equipment::EntryRef::Mount(e) => {
                let $session = &e.session;
                $body
            }
        }
    };
}
pub(crate) use with_session;

impl<'a> EntryRef<'a> {
    /// The kind as the reconnect supervisor and `equipment_changed`
    /// name it (`camera`, `filter_wheel`, …, `mount`).
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Camera(_) => "camera",
            Self::FilterWheel(_) => "filter_wheel",
            Self::CoverCalibrator(_) => "cover_calibrator",
            Self::Focuser(_) => "focuser",
            Self::SafetyMonitor(_) => "safety_monitor",
            Self::Switch(_) => "switch",
            Self::Rotator(_) => "rotator",
            Self::ObservingConditions(_) => "observing_conditions",
            Self::Dome(_) => "dome",
            Self::Mount(_) => "mount",
        }
    }

    /// The id that names the entry: its roster id, or
    /// [`config::MOUNT_ID`] for the mount.
    #[must_use]
    pub fn id(self) -> &'a str {
        match self {
            Self::Camera(e) => &e.id,
            Self::FilterWheel(e) => &e.id,
            Self::CoverCalibrator(e) => &e.id,
            Self::Focuser(e) => &e.id,
            Self::SafetyMonitor(e) => &e.id,
            Self::Switch(e) => &e.id,
            Self::Rotator(e) => &e.id,
            Self::ObservingConditions(e) => &e.id,
            Self::Dome(e) => &e.id,
            Self::Mount(_) => config::MOUNT_ID,
        }
    }

    /// The id an `equipment_changed` event carries: the roster id, or
    /// `None` for the mount, whose `device` is `null`.
    #[must_use]
    pub fn event_id(self) -> Option<&'a str> {
        match self {
            Self::Mount(_) => None,
            other => Some(other.id()),
        }
    }

    #[must_use]
    pub fn is_connected(self) -> bool {
        with_session!(self, session => session.is_connected())
    }

    #[must_use]
    pub fn is_held_out(self) -> bool {
        with_session!(self, session => session.is_held_out())
    }

    /// The `alpaca_url` of the service that serves the entry's device.
    #[must_use]
    pub fn alpaca_url(self) -> &'a str {
        self.address().alpaca_url
    }

    /// What the entry's config says about its device: its server, its
    /// number there and its credential.
    fn address(self) -> binding::RosterAddress<'a> {
        use binding::RosterEntry;
        match self {
            Self::Camera(e) => e.config.address(),
            Self::FilterWheel(e) => e.config.address(),
            Self::CoverCalibrator(e) => e.config.address(),
            Self::Focuser(e) => e.config.address(),
            Self::SafetyMonitor(e) => e.config.address(),
            Self::Switch(e) => e.config.address(),
            Self::Rotator(e) => e.config.address(),
            Self::ObservingConditions(e) => e.config.address(),
            Self::Dome(e) => e.config.address(),
            Self::Mount(e) => e.config.address(),
        }
    }
}

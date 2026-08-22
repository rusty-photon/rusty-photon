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
//! lives in [`alpaca`].
//!
//! `switch`, `rotator`, `observing_conditions`, and `dome` cover roster
//! membership and connectivity status only (rp.md § Equipment Integration)
//! — no MCP tool integration, unlike the other six kinds.
//!
//! The submodules' `*Entry` types and shared status types are
//! re-exported here so existing `crate::equipment::CameraEntry` etc.
//! callsites keep working unchanged.

pub mod alpaca;
pub mod camera;
pub mod cover_calibrator;
pub mod dome;
pub mod filter_wheel;
pub mod focuser;
pub mod mount;
pub mod observing_conditions;
pub mod rotator;
pub mod safety_monitor;
pub mod switch;
pub mod trains;

#[cfg(test)]
pub(crate) mod test_support;

pub use camera::CameraEntry;
pub use cover_calibrator::CoverCalibratorEntry;
pub use dome::DomeEntry;
pub use filter_wheel::FilterWheelEntry;
pub use focuser::FocuserEntry;
pub use mount::MountEntry;
pub use observing_conditions::ObservingConditionsEntry;
pub use rotator::RotatorEntry;
pub use safety_monitor::SafetyMonitorEntry;
pub use switch::SwitchEntry;

use serde::Serialize;
use tracing::debug;

use crate::config;
use crate::error::RpError;

#[derive(Default)]
pub struct EquipmentRegistry {
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

#[derive(Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub connected: bool,
}

/// Singular wire-format counterpart to [`MountEntry`] — no `id`.
#[derive(Serialize)]
pub struct MountStatus {
    pub connected: bool,
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

    #[must_use]
    pub fn status(&self) -> EquipmentStatus {
        EquipmentStatus {
            cameras: self
                .cameras
                .iter()
                .map(|c| DeviceStatus {
                    id: c.id.clone(),
                    connected: c.connected,
                })
                .collect(),
            filter_wheels: self
                .filter_wheels
                .iter()
                .map(|fw| DeviceStatus {
                    id: fw.id.clone(),
                    connected: fw.connected,
                })
                .collect(),
            cover_calibrators: self
                .cover_calibrators
                .iter()
                .map(|cc| DeviceStatus {
                    id: cc.id.clone(),
                    connected: cc.connected,
                })
                .collect(),
            focusers: self
                .focusers
                .iter()
                .map(|f| DeviceStatus {
                    id: f.id.clone(),
                    connected: f.connected,
                })
                .collect(),
            safety_monitors: self
                .safety_monitors
                .iter()
                .map(|sm| DeviceStatus {
                    id: sm.id.clone(),
                    connected: sm.connected,
                })
                .collect(),
            switches: self
                .switches
                .iter()
                .map(|sw| DeviceStatus {
                    id: sw.id.clone(),
                    connected: sw.connected,
                })
                .collect(),
            rotators: self
                .rotators
                .iter()
                .map(|r| DeviceStatus {
                    id: r.id.clone(),
                    connected: r.connected,
                })
                .collect(),
            observing_conditions: self
                .observing_conditions
                .iter()
                .map(|oc| DeviceStatus {
                    id: oc.id.clone(),
                    connected: oc.connected,
                })
                .collect(),
            domes: self
                .domes
                .iter()
                .map(|d| DeviceStatus {
                    id: d.id.clone(),
                    connected: d.connected,
                })
                .collect(),
            mount: self.mount.as_ref().map(|m| MountStatus {
                connected: m.connected,
            }),
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
        if !mount.connected {
            debug!("mount not connected; skipping mount-side site validation");
            return Ok(());
        }
        let Some(t) = mount.device.as_ref() else {
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

//! The virtual Telescope device
//! (docs/services/planetarium-bridge.md § The virtual Telescope device).
//!
//! Align (sync) is the sole import gesture: each accepted sync verb fires
//! one import and sets the virtual pointing. Slew verbs are simulated
//! motion — `Slewing` reads true for the convergence window while the
//! reported position interpolates — and never import. What
//! `RightAscension`/`Declination` return is subject to the altitude floor:
//! below it, the report snaps to the zenith idle point (RA = LST,
//! Dec = site latitude), the P3a wedge defense.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use ascom_alpaca::api::telescope::{
    AlignmentMode, DriveRate, EquatorialCoordinateType, PierSide, Telescope, TelescopeAxis,
};
use ascom_alpaca::api::Device;
use ascom_alpaca::{ASCOMError, ASCOMResult};
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, Site};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::config::{AssumeEpoch, DeviceConfig, SiteConfig};
use crate::epoch;
use crate::import::{ImportRequest, ImportSource, Importer, SOURCE_KIND};

/// The exact device name — pinned by `device_contract.feature`. It states
/// loudly that connecting planetarium clients are not driving a mount.
pub const DEVICE_NAME: &str = "Planetarium Bridge (virtual target entry — NOT a mount)";

const DESCRIPTION: &str = "Virtual ASCOM Alpaca Telescope for target entry from planetarium \
     apps: Align imports the selected coordinates as a paused rusty-photon target, slews are \
     simulated. It is NOT a mount and never moves hardware.";

/// A simulated slew in flight.
#[derive(Debug, Clone, Copy)]
struct Slew {
    from: (f64, f64),
    to: (f64, f64),
    started: Instant,
    duration: Duration,
}

#[derive(Debug)]
struct MountState {
    connected: bool,
    /// Virtual pointing (ICRS): where the last slew converged or the last
    /// sync placed it. Constant between motions (i.e. tracking).
    ra_hours: f64,
    dec_degrees: f64,
    target_ra: Option<f64>,
    target_dec: Option<f64>,
    slew: Option<Slew>,
    site: Site,
    site_elevation_m: f64,
}

#[derive(Debug)]
pub struct BridgeTelescope {
    unique_id: String,
    state: Mutex<MountState>,
    ephemeris: ErfarsEphemeris,
    assume_epoch: AssumeEpoch,
    slew_duration: Duration,
    /// `None` disables the reported-position policy.
    floor_deg: Option<f64>,
    importer: Importer,
    /// Peer address of the most recent Alpaca request — the import
    /// provenance's `client` field (single-client in practice; P3a).
    last_client: Arc<Mutex<Option<SocketAddr>>>,
}

impl BridgeTelescope {
    /// Build the virtual telescope, pointed at the zenith idle pose.
    ///
    /// # Errors
    ///
    /// Returns an error if `rp_ephemeris`'s site rejects the latitude
    /// or longitude range — defensive here: the config newtypes enforce
    /// the same ranges before this runs.
    pub fn new(
        device: &DeviceConfig,
        site_config: &SiteConfig,
        ephemeris: ErfarsEphemeris,
        importer: Importer,
        last_client: Arc<Mutex<Option<SocketAddr>>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let site = Site::new(
            site_config.site_latitude_deg.degrees(),
            site_config.site_longitude_deg.degrees(),
        )?;
        // Start pointed at the zenith idle point — the "parked overhead"
        // resting pose the reported-position policy snaps to anyway.
        let initial_ra = ephemeris.sidereal_time(&site, Utc::now()).lst_hours;
        Ok(Self {
            unique_id: device.unique_id.clone(),
            state: Mutex::new(MountState {
                connected: false,
                ra_hours: initial_ra,
                dec_degrees: site.latitude_degrees,
                target_ra: None,
                target_dec: None,
                slew: None,
                site,
                site_elevation_m: site_config.site_elevation_m,
            }),
            ephemeris,
            assume_epoch: device.assume_epoch,
            slew_duration: device.slew_duration,
            floor_deg: device
                .report_altitude_floor_deg
                .map(super::config::FloorDeg::degrees),
            importer,
            last_client,
        })
    }

    /// Run `f` against the folded-forward mount state.
    fn with_state<R>(&self, f: impl FnOnce(&mut MountState) -> R) -> R {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        fold_position(&mut guard);
        f(&mut guard)
    }

    /// The reported position: virtual pointing while its computed altitude
    /// clears the floor, else the zenith idle point. Evaluated at read time
    /// from the live site.
    fn reported_position(&self) -> (f64, f64) {
        let (pointing, site) = self.with_state(|s| ((s.ra_hours, s.dec_degrees), s.site));
        let Some(floor) = self.floor_deg else {
            return pointing;
        };
        let now = Utc::now();
        match self.ephemeris.alt_az(
            &site,
            IcrsCoord {
                ra_hours: pointing.0,
                dec_degrees: pointing.1,
            },
            now,
        ) {
            Ok(alt_az) if alt_az.altitude_degrees < floor => {
                let lst = self.ephemeris.sidereal_time(&site, now).lst_hours;
                (lst, site.latitude_degrees)
            }
            Ok(_) => pointing,
            Err(e) => {
                debug!("alt-az for the floor policy failed ({e}); reporting raw pointing");
                pointing
            }
        }
    }

    /// Shared slew path: validate, convert, start the simulated motion.
    fn start_slew(&self, verb: &str, ra_hours: f64, dec_degrees: f64) -> ASCOMResult<()> {
        validate_coords(ra_hours, dec_degrees)?;
        let (icrs_ra, icrs_dec) =
            epoch::to_icrs(self.assume_epoch, ra_hours, dec_degrees, Utc::now());
        debug!(
            verb,
            ra_hours, dec_degrees, "simulated slew started (never an import)"
        );
        self.with_state(|s| {
            s.target_ra = Some(ra_hours);
            s.target_dec = Some(dec_degrees);
            s.slew = Some(Slew {
                from: (s.ra_hours, s.dec_degrees),
                to: (icrs_ra, icrs_dec),
                started: Instant::now(),
                duration: self.slew_duration,
            });
        });
        Ok(())
    }

    /// Shared sync path — the import gesture: validate, convert, fire the
    /// import, and move the virtual pointing to the synced coordinates.
    fn do_sync(&self, verb: &str, ra_hours: f64, dec_degrees: f64) -> ASCOMResult<()> {
        validate_coords(ra_hours, dec_degrees)?;
        let now = Utc::now();
        let (icrs_ra, icrs_dec) = epoch::to_icrs(self.assume_epoch, ra_hours, dec_degrees, now);
        let client = {
            let guard = match self.last_client.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.map_or_else(|| "unknown".to_owned(), |addr| addr.to_string())
        };
        debug!(
            verb,
            ra_hours, dec_degrees, client, "Align received; firing the import"
        );
        self.importer.submit(ImportRequest {
            ra_hours: icrs_ra,
            dec_degrees: icrs_dec,
            source: ImportSource {
                kind: SOURCE_KIND.to_owned(),
                client,
                received_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            },
        });
        self.with_state(|s| {
            s.slew = None;
            s.ra_hours = icrs_ra;
            s.dec_degrees = icrs_dec;
            s.target_ra = Some(ra_hours);
            s.target_dec = Some(dec_degrees);
        });
        Ok(())
    }

    fn stored_target(&self) -> ASCOMResult<(f64, f64)> {
        self.with_state(|s| match (s.target_ra, s.target_dec) {
            (Some(ra), Some(dec)) => Ok((ra, dec)),
            _ => Err(ASCOMError::VALUE_NOT_SET),
        })
    }
}

/// Complete or advance an in-flight slew; called under the state lock.
#[expect(
    clippy::suboptimal_flops,
    reason = "from + (to − from)·frac is the canonical lerp shape; the simulated slew gains nothing observable from fusing"
)]
fn fold_position(state: &mut MountState) {
    if let Some(slew) = state.slew {
        let frac = slew_fraction(
            slew.started.elapsed().as_secs_f64(),
            slew.duration.as_secs_f64(),
        );
        if frac >= 1.0 {
            state.ra_hours = slew.to.0;
            state.dec_degrees = slew.to.1;
            state.slew = None;
            debug!(
                ra_hours = state.ra_hours,
                dec_degrees = state.dec_degrees,
                "simulated slew converged"
            );
        } else {
            state.ra_hours = interp_ra(slew.from.0, slew.to.0, frac);
            state.dec_degrees = slew.from.1 + (slew.to.1 - slew.from.1) * frac;
        }
    }
}

/// Fraction of a slew completed. A zero duration (the config accepts
/// `slew_duration: "0s"`) counts as already converged — dividing by it
/// would fold `0/0 = NaN` into the state on a same-instant read.
fn slew_fraction(elapsed_secs: f64, duration_secs: f64) -> f64 {
    if duration_secs > 0.0 {
        elapsed_secs / duration_secs
    } else {
        1.0
    }
}

/// RA interpolation along the shortest arc, wrap-aware at 0h/24h.
fn interp_ra(from: f64, to: f64, frac: f64) -> f64 {
    let mut delta = (to - from).rem_euclid(24.0);
    if delta > 12.0 {
        delta -= 24.0;
    }
    (from + delta * frac).rem_euclid(24.0)
}

fn validate_coords(ra_hours: f64, dec_degrees: f64) -> ASCOMResult<()> {
    if !(0.0..24.0).contains(&ra_hours) || !(-90.0..=90.0).contains(&dec_degrees) {
        warn!("client sent out-of-range coordinates: RA {ra_hours} h / Dec {dec_degrees} deg");
        return Err(ASCOMError::invalid_value(format!(
            "RA {ra_hours} h / Dec {dec_degrees} deg out of range"
        )));
    }
    Ok(())
}

#[async_trait]
impl Device for BridgeTelescope {
    fn static_name(&self) -> &str {
        DEVICE_NAME
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.with_state(|s| s.connected))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        info!(
            "planetarium client {}",
            if connected {
                "connected"
            } else {
                "disconnected"
            }
        );
        self.with_state(|s| {
            s.connected = connected;
            if connected {
                // The Target properties are session state: a fresh connect
                // starts with them unset (read-before-write errors per the
                // ASCOM contract). The virtual pointing survives — the
                // reported-position story spans sessions.
                s.target_ra = None;
                s.target_dec = None;
            }
        });
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(DESCRIPTION.to_owned())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon planetarium-bridge".to_owned())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_owned())
    }
}

#[async_trait]
impl Telescope for BridgeTelescope {
    async fn alignment_mode(&self) -> ASCOMResult<AlignmentMode> {
        Ok(AlignmentMode::GermanPolar)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialCoordinateType> {
        Ok(EquatorialCoordinateType::J2000)
    }

    async fn at_home(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn at_park(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn can_slew(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_slew_async(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn can_sync(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn destination_side_of_pier(
        &self,
        right_ascension: f64,
        declination: f64,
    ) -> ASCOMResult<PierSide> {
        validate_coords(right_ascension, declination)?;
        let site = self.with_state(|s| s.site);
        let lst = self.ephemeris.sidereal_time(&site, Utc::now()).lst_hours;
        // GEM convention: a target west of the meridian (positive hour
        // angle) is observed from the east side of the pier. A GermanPolar
        // device must answer this consistently on both sides of the
        // meridian even though it never flips anything.
        let mut hour_angle = (lst - right_ascension).rem_euclid(24.0);
        if hour_angle > 12.0 {
            hour_angle -= 24.0;
        }
        Ok(if hour_angle > 0.0 {
            PierSide::East
        } else {
            PierSide::West
        })
    }

    async fn right_ascension(&self) -> ASCOMResult<f64> {
        Ok(self.reported_position().0)
    }

    async fn declination(&self) -> ASCOMResult<f64> {
        Ok(self.reported_position().1)
    }

    async fn right_ascension_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn declination_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn altitude(&self) -> ASCOMResult<f64> {
        let (ra_hours, dec_degrees) = self.reported_position();
        let site = self.with_state(|s| s.site);
        self.ephemeris
            .alt_az(
                &site,
                IcrsCoord {
                    ra_hours,
                    dec_degrees,
                },
                Utc::now(),
            )
            .map(|alt_az| alt_az.altitude_degrees)
            .map_err(|e| ASCOMError::invalid_operation(format!("alt-az failed: {e}")))
    }

    async fn azimuth(&self) -> ASCOMResult<f64> {
        let (ra_hours, dec_degrees) = self.reported_position();
        let site = self.with_state(|s| s.site);
        self.ephemeris
            .alt_az(
                &site,
                IcrsCoord {
                    ra_hours,
                    dec_degrees,
                },
                Utc::now(),
            )
            .map(|alt_az| alt_az.azimuth_degrees)
            .map_err(|e| ASCOMError::invalid_operation(format!("alt-az failed: {e}")))
    }

    async fn sidereal_time(&self) -> ASCOMResult<f64> {
        let site = self.with_state(|s| s.site);
        Ok(self.ephemeris.sidereal_time(&site, Utc::now()).lst_hours)
    }

    async fn site_latitude(&self) -> ASCOMResult<f64> {
        Ok(self.with_state(|s| s.site.latitude_degrees))
    }

    async fn set_site_latitude(&self, site_latitude: f64) -> ASCOMResult<()> {
        let longitude = self.with_state(|s| s.site.longitude_degrees);
        let site = Site::new(site_latitude, longitude)
            .map_err(|e| ASCOMError::invalid_value(e.to_string()))?;
        info!("adopting client-pushed SiteLatitude = {site_latitude} deg");
        self.with_state(|s| s.site = site);
        Ok(())
    }

    async fn site_longitude(&self) -> ASCOMResult<f64> {
        Ok(self.with_state(|s| s.site.longitude_degrees))
    }

    async fn set_site_longitude(&self, site_longitude: f64) -> ASCOMResult<()> {
        let latitude = self.with_state(|s| s.site.latitude_degrees);
        let site = Site::new(latitude, site_longitude)
            .map_err(|e| ASCOMError::invalid_value(e.to_string()))?;
        info!("adopting client-pushed SiteLongitude = {site_longitude} deg");
        self.with_state(|s| s.site = site);
        Ok(())
    }

    async fn site_elevation(&self) -> ASCOMResult<f64> {
        Ok(self.with_state(|s| s.site_elevation_m))
    }

    async fn set_site_elevation(&self, site_elevation: f64) -> ASCOMResult<()> {
        // The ASCOM valid range (also what ConformU enforces).
        if !(-300.0..=10_000.0).contains(&site_elevation) {
            return Err(ASCOMError::invalid_value(format!(
                "SiteElevation {site_elevation} m out of range [-300, 10000]"
            )));
        }
        debug!("client set SiteElevation = {site_elevation} m");
        self.with_state(|s| s.site_elevation_m = site_elevation);
        Ok(())
    }

    async fn slewing(&self) -> ASCOMResult<bool> {
        Ok(self.with_state(|s| s.slew.is_some()))
    }

    async fn slew_settle_time(&self) -> ASCOMResult<Duration> {
        Ok(Duration::ZERO)
    }

    async fn target_right_ascension(&self) -> ASCOMResult<f64> {
        self.stored_target().map(|(ra, _)| ra)
    }

    async fn set_target_right_ascension(&self, target_right_ascension: f64) -> ASCOMResult<()> {
        validate_coords(target_right_ascension, 0.0)?;
        self.with_state(|s| s.target_ra = Some(target_right_ascension));
        Ok(())
    }

    async fn target_declination(&self) -> ASCOMResult<f64> {
        self.stored_target().map(|(_, dec)| dec)
    }

    async fn set_target_declination(&self, target_declination: f64) -> ASCOMResult<()> {
        validate_coords(0.0, target_declination)?;
        self.with_state(|s| s.target_dec = Some(target_declination));
        Ok(())
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        // Constant: the virtual pointing holds RA/Dec, which is exactly
        // what a tracking mount reports.
        Ok(true)
    }

    async fn tracking_rate(&self) -> ASCOMResult<DriveRate> {
        Ok(DriveRate::Sidereal)
    }

    async fn set_tracking_rate(&self, tracking_rate: DriveRate) -> ASCOMResult<()> {
        if tracking_rate == DriveRate::Sidereal {
            Ok(())
        } else {
            Err(ASCOMError::invalid_value("only Sidereal is supported"))
        }
    }

    async fn tracking_rates(&self) -> ASCOMResult<Vec<DriveRate>> {
        Ok(vec![DriveRate::Sidereal])
    }

    async fn utc_date(&self) -> ASCOMResult<SystemTime> {
        Ok(SystemTime::now())
    }

    async fn axis_rates(
        &self,
        axis: TelescopeAxis,
    ) -> ASCOMResult<Vec<std::ops::RangeInclusive<f64>>> {
        debug!("client read AxisRates for {axis:?}: none (no manual motion)");
        Ok(Vec::new())
    }

    async fn abort_slew(&self) -> ASCOMResult<()> {
        debug!("AbortSlew: ending the simulated motion");
        self.with_state(|s| s.slew = None);
        Ok(())
    }

    async fn slew_to_coordinates(&self, right_ascension: f64, declination: f64) -> ASCOMResult<()> {
        self.start_slew("SlewToCoordinates", right_ascension, declination)?;
        // The blocking form's contract: return once the slew completes.
        tokio::time::sleep(self.slew_duration).await;
        Ok(())
    }

    async fn slew_to_coordinates_async(
        &self,
        right_ascension: f64,
        declination: f64,
    ) -> ASCOMResult<()> {
        self.start_slew("SlewToCoordinatesAsync", right_ascension, declination)
    }

    async fn slew_to_target(&self) -> ASCOMResult<()> {
        let (ra, dec) = self.stored_target()?;
        self.start_slew("SlewToTarget", ra, dec)?;
        tokio::time::sleep(self.slew_duration).await;
        Ok(())
    }

    async fn slew_to_target_async(&self) -> ASCOMResult<()> {
        let (ra, dec) = self.stored_target()?;
        self.start_slew("SlewToTargetAsync", ra, dec)
    }

    async fn sync_to_coordinates(&self, right_ascension: f64, declination: f64) -> ASCOMResult<()> {
        self.do_sync("SyncToCoordinates", right_ascension, declination)
    }

    async fn sync_to_target(&self) -> ASCOMResult<()> {
        let (ra, dec) = self.stored_target()?;
        self.do_sync("SyncToTarget", ra, dec)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn interp_ra_takes_the_short_way_across_the_wrap() {
        let halfway = interp_ra(23.8, 0.2, 0.5);
        assert!(
            (halfway - 0.0).abs() < 1e-9 || (halfway - 24.0).abs() < 1e-9,
            "expected the midpoint at the wrap, got {halfway}"
        );
        assert!((interp_ra(23.8, 0.2, 1.0) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn interp_ra_is_plain_interpolation_away_from_the_wrap() {
        assert!((interp_ra(2.0, 4.0, 0.25) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn slew_fraction_is_elapsed_over_duration() {
        assert!((slew_fraction(1.0, 4.0) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn slew_fraction_treats_zero_duration_as_converged() {
        assert!((slew_fraction(0.0, 0.0) - 1.0).abs() < 1e-12);
        assert!((slew_fraction(5.0, 0.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn out_of_range_coordinates_are_rejected() {
        assert!(validate_coords(24.0, 0.0).is_err());
        assert!(validate_coords(-0.1, 0.0).is_err());
        assert!(validate_coords(0.0, 90.5).is_err());
        assert!(validate_coords(0.0, -90.5).is_err());
        assert!(validate_coords(12.0, -45.0).is_ok());
        assert!(validate_coords(0.0, 90.0).is_ok());
    }

    #[test]
    fn the_device_name_matches_the_contract_pin() {
        assert_eq!(
            DEVICE_NAME,
            "Planetarium Bridge (virtual target entry — NOT a mount)"
        );
        assert!(DESCRIPTION.contains("NOT a mount"));
    }
}

//! A mount the unit tests script, answering in the process with no server,
//! so that a test can hold a pulse open or run on paused time.

use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::{
    DriveRate, EquatorialCoordinateType, GuideDirection, PierSide, TelescopeAxis,
};
use ascom_alpaca::api::{Device, Telescope};
use ascom_alpaca::{ASCOMError, ASCOMResult};

/// A tracking mount on the East side of the pier at declination 10° that
/// reports guide rates of 0.002 °/s and whose pulses end with their
/// duration.
#[derive(Debug)]
pub struct FakeMount {
    pub tracking: bool,
    pub slewing: bool,
    pub at_park: bool,
    /// `None` for a mount that does not implement `SideOfPier`.
    pub side: Option<PierSide>,
    pub can_pulse_guide: bool,
    /// `None` for a mount that does not implement the guide rates.
    pub guide_rate: Option<f64>,
    /// `PulseGuide` never answers.
    pub pulse_hangs: bool,
    /// `PulseGuide` answers an error.
    pub pulse_refused: bool,
    /// `IsPulseGuiding` reads true.
    pub pulse_guiding: AtomicBool,
    /// `IsPulseGuiding` answers an error.
    pub pulse_guiding_fails: bool,
    /// How long each read of the mount check (`Tracking`) takes.
    pub read_delay: Duration,
    /// `PulseGuide` calls.
    pub pulses: AtomicUsize,
}

impl Default for FakeMount {
    fn default() -> Self {
        Self {
            tracking: true,
            slewing: false,
            at_park: false,
            side: Some(PierSide::East),
            can_pulse_guide: true,
            guide_rate: Some(0.002),
            pulse_hangs: false,
            pulse_refused: false,
            pulse_guiding: AtomicBool::new(false),
            pulse_guiding_fails: false,
            read_delay: Duration::ZERO,
            pulses: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Device for FakeMount {
    fn static_name(&self) -> &'static str {
        "fake mount"
    }

    fn unique_id(&self) -> &'static str {
        "fake-mount"
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn set_connected(&self, _connected: bool) -> ASCOMResult<()> {
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok("a mount the unit tests script".to_owned())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("guider tests".to_owned())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok("0".to_owned())
    }
}

#[async_trait::async_trait]
impl Telescope for FakeMount {
    async fn at_home(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn at_park(&self) -> ASCOMResult<bool> {
        Ok(self.at_park)
    }

    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        Ok(self.can_pulse_guide)
    }

    async fn declination(&self) -> ASCOMResult<f64> {
        Ok(10.0)
    }

    async fn declination_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialCoordinateType> {
        Ok(EquatorialCoordinateType::Topocentric)
    }

    async fn guide_rate_declination(&self) -> ASCOMResult<f64> {
        self.guide_rate.ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn guide_rate_right_ascension(&self) -> ASCOMResult<f64> {
        self.guide_rate.ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn is_pulse_guiding(&self) -> ASCOMResult<bool> {
        if self.pulse_guiding_fails {
            return Err(ASCOMError::invalid_operation("not now"));
        }
        Ok(self.pulse_guiding.load(Ordering::SeqCst))
    }

    async fn right_ascension(&self) -> ASCOMResult<f64> {
        Ok(6.0)
    }

    async fn right_ascension_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn side_of_pier(&self) -> ASCOMResult<PierSide> {
        self.side.ok_or(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn sidereal_time(&self) -> ASCOMResult<f64> {
        Ok(6.0)
    }

    async fn slewing(&self) -> ASCOMResult<bool> {
        Ok(self.slewing)
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        tokio::time::sleep(self.read_delay).await;
        Ok(self.tracking)
    }

    async fn tracking_rate(&self) -> ASCOMResult<DriveRate> {
        Ok(DriveRate::Sidereal)
    }

    async fn utc_date(&self) -> ASCOMResult<SystemTime> {
        Ok(SystemTime::now())
    }

    async fn axis_rates(&self, _axis: TelescopeAxis) -> ASCOMResult<Vec<RangeInclusive<f64>>> {
        Ok(Vec::new())
    }

    async fn pulse_guide(
        &self,
        _direction: GuideDirection,
        _duration: Duration,
    ) -> ASCOMResult<()> {
        self.pulses.fetch_add(1, Ordering::SeqCst);
        if self.pulse_hangs {
            std::future::pending::<()>().await;
        }
        if self.pulse_refused {
            return Err(ASCOMError::invalid_operation("not now"));
        }
        Ok(())
    }
}

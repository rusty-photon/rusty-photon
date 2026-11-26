//! The mount's side of a run.
//!
//! The mount check before every pulse and once in every cycle that sends
//! none, the pulses, and what a request reads of the mount before a run
//! starts (docs/services/guider.md § The mount: checks and pulses). The
//! only command the guider sends the mount is `PulseGuide`.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::{GuideDirection, PierSide as AlpacaSide};
use ascom_alpaca::api::Telescope;
use rp_guiding::{Direction, GuideRates, PierSide, Pulse};
use tokio::time::Instant;
use tracing::debug;

use crate::api::{ApiError, ErrorCode, Stop, StopReason};
use crate::devices::{member, MemberError};

/// How often `IsPulseGuiding` is read once a pulse's duration has passed.
const PULSE_POLL: Duration = Duration::from_millis(25);

/// What a request read of the mount before a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MountStart {
    /// The run's pier side.
    pub side: PierSide,
    /// `Declination`, in degrees.
    pub declination_deg: f64,
    /// `GuideRateRightAscension` and `GuideRateDeclination`; `None` for a
    /// mount that does not implement them.
    pub guide_rates: Option<GuideRates>,
}

/// What the mount check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Readings {
    tracking: bool,
    slewing: bool,
    at_park: bool,
    side: PierSide,
}

/// The pier side `SideOfPier` reports.
const fn side_of(side: AlpacaSide) -> PierSide {
    match side {
        AlpacaSide::East => PierSide::East,
        AlpacaSide::West => PierSide::West,
        AlpacaSide::Unknown => PierSide::Unknown,
    }
}

/// Read what the mount check reads. A `SideOfPier` the mount does not
/// implement is the side `unknown`.
async fn read(mount: &dyn Telescope) -> Result<Readings, MemberError> {
    let (tracking, slewing, at_park, side) = tokio::join!(
        member(mount.tracking()),
        member(mount.slewing()),
        member(mount.at_park()),
        member(mount.side_of_pier()),
    );
    let side = match side {
        Ok(side) => side_of(side),
        Err(error) if error.not_implemented() => PierSide::Unknown,
        Err(error) => return Err(error),
    };
    Ok(Readings {
        tracking: tracking?,
        slewing: slewing?,
        at_park: at_park?,
        side,
    })
}

/// What keeps a run from going on, when anything does: the first of a park,
/// a slew, tracking off and another pier side than `side`, the run's.
fn verdict(readings: Readings, side: PierSide) -> Option<(StopReason, String)> {
    if readings.at_park {
        Some((StopReason::MountParked, "the mount is parked".to_owned()))
    } else if readings.slewing {
        Some((StopReason::MountSlewing, "the mount is slewing".to_owned()))
    } else if !readings.tracking {
        Some((
            StopReason::MountNotTracking,
            "the mount is not tracking".to_owned(),
        ))
    } else if side != PierSide::Unknown && readings.side != side {
        Some((
            StopReason::PierSideChanged,
            format!(
                "the mount is on the {:?} side of the pier, and the run started on the {side:?}",
                readings.side
            ),
        ))
    } else {
        None
    }
}

/// Read the mount at a request that starts a run: the mount check,
/// `CanPulseGuide`, the declination and the guide rates.
///
/// # Errors
///
/// `mount_not_ready` for a mount that is parked, slewing, not tracking or
/// cannot pulse guide, and `device_unreachable` for a read that fails.
pub async fn ready(mount: &dyn Telescope) -> Result<MountStart, ApiError> {
    let unreachable = |error: MemberError| {
        ApiError::new(
            ErrorCode::DeviceUnreachable,
            format!("the mount does not answer: {error}"),
        )
    };
    let (readings, can_pulse_guide, declination_deg, ra_rate, dec_rate) = tokio::join!(
        read(mount),
        member(mount.can_pulse_guide()),
        member(mount.declination()),
        member(mount.guide_rate_right_ascension()),
        member(mount.guide_rate_declination()),
    );
    let readings = readings.map_err(unreachable)?;
    if let Some((_, message)) = verdict(readings, readings.side) {
        return Err(ApiError::new(ErrorCode::MountNotReady, message));
    }
    if !can_pulse_guide.map_err(unreachable)? {
        return Err(ApiError::new(
            ErrorCode::MountNotReady,
            "the mount cannot pulse guide",
        ));
    }
    let guide_rates = match (ra_rate, dec_rate) {
        (Ok(ra_deg_per_s), Ok(dec_deg_per_s)) => Some(GuideRates {
            ra_deg_per_s,
            dec_deg_per_s,
        }),
        (Err(error), _) | (_, Err(error)) if error.not_implemented() => None,
        (Err(error), _) | (_, Err(error)) => return Err(unreachable(error)),
    };
    let start = MountStart {
        side: readings.side,
        declination_deg: declination_deg.map_err(unreachable)?,
        guide_rates,
    };
    debug!(?start, "the mount is ready for a run");
    Ok(start)
}

/// Why a mount call of a run's cycle did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The run stops.
    Stop(Stop),
    /// A read or a `PulseGuide` call failed before a pulse went out: the
    /// cycle sends no pulse.
    Failed(String),
    /// The `PulseGuide` call returned, so the pulse went out, and the
    /// `IsPulseGuiding` read after it failed.
    Unconfirmed(String),
}

/// The mount during a run.
pub struct RunMount {
    mount: Arc<dyn Telescope>,
    /// The pier side the run started on.
    side: PierSide,
    pulse_timeout: Duration,
    max_failures: u32,
    /// Cycles in a row with a failed mount call.
    failures: u32,
}

impl RunMount {
    #[must_use]
    pub fn new(
        mount: Arc<dyn Telescope>,
        side: PierSide,
        pulse_timeout: Duration,
        max_failures: u32,
    ) -> Self {
        Self {
            mount,
            side,
            pulse_timeout,
            max_failures,
            failures: 0,
        }
    }

    /// The mount.
    #[must_use]
    pub fn telescope(&self) -> Arc<dyn Telescope> {
        Arc::clone(&self.mount)
    }

    /// The mount check.
    ///
    /// # Errors
    ///
    /// [`Fault::Stop`] for a mount that is parked, slewing, not tracking or
    /// on another pier side than the run's, and [`Fault::Failed`] for a read
    /// that fails.
    pub async fn check(&self) -> Result<(), Fault> {
        let readings = read(&*self.mount)
            .await
            .map_err(|error| Fault::Failed(format!("the mount check failed: {error}")))?;
        match verdict(readings, self.side) {
            Some((reason, message)) => Err(Fault::Stop(Stop::new(reason, message))),
            None => Ok(()),
        }
    }

    /// `pulse`, ended: the call has returned, its duration has passed and
    /// `IsPulseGuiding` reads false. The caller has checked the mount.
    ///
    /// # Errors
    ///
    /// [`Fault::Stop`] with `pulse_timeout` for a pulse still in progress
    /// `mount.pulse_timeout` after its duration, [`Fault::Failed`] for a
    /// `PulseGuide` call that fails, and [`Fault::Unconfirmed`] for an
    /// `IsPulseGuiding` read that fails after the call returned.
    pub async fn pulse(&self, pulse: Pulse) -> Result<(), Fault> {
        let direction = match pulse.direction {
            Direction::North => GuideDirection::North,
            Direction::South => GuideDirection::South,
            Direction::East => GuideDirection::East,
            Direction::West => GuideDirection::West,
        };
        let started = Instant::now();
        let ends = started.checked_add(pulse.duration).unwrap_or(started);
        let deadline = ends.checked_add(self.pulse_timeout).unwrap_or(ends);
        let timed_out = || {
            Fault::Stop(Stop::new(
                StopReason::PulseTimeout,
                format!(
                    "a {:?} pulse of {} was still in progress {} after its duration",
                    pulse.direction,
                    humantime::format_duration(pulse.duration),
                    humantime::format_duration(self.pulse_timeout)
                ),
            ))
        };
        match tokio::time::timeout_at(deadline, self.mount.pulse_guide(direction, pulse.duration))
            .await
        {
            Err(_) => return Err(timed_out()),
            Ok(Err(error)) => return Err(Fault::Failed(format!("PulseGuide failed: {error}"))),
            Ok(Ok(())) => {}
        }
        tokio::time::sleep_until(ends).await;
        // Only `IsPulseGuiding` reading true shows a pulse in progress, and
        // only a read asked once the deadline has passed shows it in
        // progress then: one asked before it is asked again. A read that
        // has not answered is a call like any other, held to the call
        // deadline, and not a pulse in progress.
        loop {
            let asked = Instant::now();
            match member(self.mount.is_pulse_guiding()).await {
                Err(error) => {
                    return Err(Fault::Unconfirmed(format!(
                        "reading IsPulseGuiding failed: {error}"
                    )))
                }
                Ok(false) => break,
                Ok(true) if asked >= deadline => return Err(timed_out()),
                Ok(true) => tokio::time::sleep(PULSE_POLL).await,
            }
        }
        debug!(?pulse, "pulse ended");
        Ok(())
    }

    /// Count a cycle: one with a failed mount call adds to the cycles in a
    /// row, one without starts the count again. The stop once
    /// `mount.max_failures` have failed in a row.
    pub fn cycle(&mut self, failed: Option<&str>) -> Option<Stop> {
        let Some(message) = failed else {
            self.failures = 0;
            return None;
        };
        self.failures = self.failures.saturating_add(1);
        debug!(failures = self.failures, max = self.max_failures, %message, "a mount cycle failed");
        (self.failures >= self.max_failures).then(|| {
            Stop::new(
                StopReason::MountUnreachable,
                format!(
                    "{} cycles in a row had a failed mount call, the last because {message}",
                    self.max_failures
                ),
            )
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const fn readings(tracking: bool, slewing: bool, at_park: bool, side: PierSide) -> Readings {
        Readings {
            tracking,
            slewing,
            at_park,
            side,
        }
    }

    #[test]
    fn a_tracking_mount_on_the_runs_side_goes_on() {
        assert_eq!(
            verdict(readings(true, false, false, PierSide::East), PierSide::East),
            None
        );
    }

    #[test]
    fn a_parked_mount_is_parked_whatever_else_it_reports() {
        let (reason, message) =
            verdict(readings(false, true, true, PierSide::West), PierSide::East).unwrap();
        assert_eq!(reason, StopReason::MountParked);
        assert_eq!(message, "the mount is parked");
    }

    #[test]
    fn a_slewing_mount_is_slewing_before_it_is_not_tracking() {
        let (reason, _) =
            verdict(readings(false, true, false, PierSide::East), PierSide::East).unwrap();
        assert_eq!(reason, StopReason::MountSlewing);
    }

    #[test]
    fn a_mount_that_does_not_track_stops_the_run() {
        let (reason, _) = verdict(
            readings(false, false, false, PierSide::East),
            PierSide::East,
        )
        .unwrap();
        assert_eq!(reason, StopReason::MountNotTracking);
    }

    #[test]
    fn another_pier_side_stops_a_run_with_a_known_side_only() {
        let (reason, _) =
            verdict(readings(true, false, false, PierSide::West), PierSide::East).unwrap();
        assert_eq!(reason, StopReason::PierSideChanged);
        assert_eq!(
            verdict(
                readings(true, false, false, PierSide::West),
                PierSide::Unknown
            ),
            None
        );
    }

    use std::sync::atomic::Ordering;

    use crate::test_mount::FakeMount;

    fn run(mount: FakeMount) -> (Arc<FakeMount>, RunMount) {
        let mount = Arc::new(mount);
        let run = RunMount::new(
            Arc::clone(&mount) as Arc<dyn Telescope>,
            PierSide::East,
            Duration::from_secs(1),
            3,
        );
        (mount, run)
    }

    fn west(millis: u64) -> Pulse {
        Pulse {
            direction: Direction::West,
            duration: Duration::from_millis(millis),
        }
    }

    #[tokio::test]
    async fn a_ready_mount_gives_the_runs_side_declination_and_guide_rates() {
        let start = ready(&FakeMount::default()).await.unwrap();
        assert_eq!(
            start,
            MountStart {
                side: PierSide::East,
                declination_deg: 10.0,
                guide_rates: Some(GuideRates {
                    ra_deg_per_s: 0.002,
                    dec_deg_per_s: 0.002
                }),
            }
        );
    }

    #[tokio::test]
    async fn a_mount_without_guide_rates_or_a_pier_side_is_ready_without_them() {
        let start = ready(&FakeMount {
            guide_rate: None,
            side: None,
            ..FakeMount::default()
        })
        .await
        .unwrap();
        assert_eq!(start.guide_rates, None);
        assert_eq!(start.side, PierSide::Unknown);
    }

    #[tokio::test]
    async fn a_mount_that_is_not_ready_says_why() {
        for (mount, why) in [
            (
                FakeMount {
                    at_park: true,
                    tracking: false,
                    ..FakeMount::default()
                },
                "the mount is parked",
            ),
            (
                FakeMount {
                    slewing: true,
                    ..FakeMount::default()
                },
                "the mount is slewing",
            ),
            (
                FakeMount {
                    tracking: false,
                    ..FakeMount::default()
                },
                "the mount is not tracking",
            ),
            (
                FakeMount {
                    can_pulse_guide: false,
                    ..FakeMount::default()
                },
                "the mount cannot pulse guide",
            ),
        ] {
            let error = ready(&mount).await.unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str()),
                (ErrorCode::MountNotReady, why)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_ends_once_its_duration_has_passed() {
        let (mount, run) = run(FakeMount::default());
        let started = Instant::now();
        run.pulse(west(300)).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_still_guiding_after_the_pulse_timeout_stops_the_run() {
        let (_, run) = run(FakeMount {
            pulse_guiding: true.into(),
            ..FakeMount::default()
        });
        let started = Instant::now();
        let Err(Fault::Stop(stop)) = run.pulse(west(300)).await else {
            panic!("the pulse ended");
        };
        assert_eq!(stop.reason, StopReason::PulseTimeout);
        // The first read asked once the 1.3 s deadline has passed stops it.
        assert!(
            started.elapsed() < Duration::from_millis(1_400),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_stops_the_run_only_on_a_read_asked_after_the_pulse_timeout() {
        let (_, run) = run(FakeMount {
            pulse_guiding: true.into(),
            pulse_guiding_delay: Duration::from_millis(700),
            ..FakeMount::default()
        });
        let started = Instant::now();
        let Err(Fault::Stop(stop)) = run.pulse(west(300)).await else {
            panic!("the pulse ended");
        };
        assert_eq!(stop.reason, StopReason::PulseTimeout);
        // Reads asked at 0.3 s and about 1.0 s answer true before the 1.3 s
        // deadline is behind them; the one asked at about 1.75 s stops it
        // as it answers, at about 2.45 s.
        assert!(
            (Duration::from_millis(2_400)..Duration::from_millis(2_500))
                .contains(&started.elapsed()),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_read_that_finds_the_pulse_ended_ends_it() {
        let (_, run) = run(FakeMount {
            pulse_guiding_delay: Duration::from_secs(5),
            ..FakeMount::default()
        });
        run.pulse(west(300)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_with_no_answer_is_a_failed_call_not_a_pulse_in_progress() {
        let (_, run) = run(FakeMount {
            pulse_guiding: true.into(),
            pulse_guiding_delay: Duration::from_secs(60),
            ..FakeMount::default()
        });
        let Err(Fault::Unconfirmed(message)) = run.pulse(west(300)).await else {
            panic!("the unanswered read was taken for a pulse in progress");
        };
        assert!(message.contains("no answer within 10s"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_call_that_does_not_return_by_the_pulse_timeout_stops_the_run() {
        let (_, run) = run(FakeMount {
            pulse_hangs: true,
            ..FakeMount::default()
        });
        let Err(Fault::Stop(stop)) = run.pulse(west(300)).await else {
            panic!("the pulse ended");
        };
        assert_eq!(stop.reason, StopReason::PulseTimeout);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_pulse_fails_the_cycle() {
        let (_, run) = run(FakeMount {
            pulse_refused: true,
            ..FakeMount::default()
        });
        let Err(Fault::Failed(message)) = run.pulse(west(300)).await else {
            panic!("the pulse did not fail");
        };
        assert!(message.starts_with("PulseGuide failed"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pulse_whose_end_cannot_be_read_went_out() {
        let (mount, run) = run(FakeMount {
            pulse_guiding_fails: true,
            ..FakeMount::default()
        });
        let Err(Fault::Unconfirmed(message)) = run.pulse(west(300)).await else {
            panic!("the pulse's end was read");
        };
        assert!(
            message.starts_with("reading IsPulseGuiding failed"),
            "{message}"
        );
        assert_eq!(mount.pulses.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_mount_on_another_side_than_the_runs_fails_its_check() {
        let (_, run) = run(FakeMount {
            side: Some(AlpacaSide::West),
            ..FakeMount::default()
        });
        let Err(Fault::Stop(stop)) = run.check().await else {
            panic!("the mount passed its check");
        };
        assert_eq!(stop.reason, StopReason::PierSideChanged);
    }

    #[test]
    fn mount_failures_stop_the_run_only_at_the_most_allowed_in_a_row() {
        let (_, mut mount) = run(FakeMount::default());
        assert_eq!(mount.cycle(Some("one")), None);
        assert_eq!(mount.cycle(Some("two")), None);
        assert_eq!(mount.cycle(None), None);
        assert_eq!(mount.cycle(Some("three")), None);
        assert_eq!(mount.cycle(Some("four")), None);
        let stop = mount.cycle(Some("five")).unwrap();
        assert_eq!(stop.reason, StopReason::MountUnreachable);
        assert!(stop.message.ends_with("because five"), "{}", stop.message);
    }
}

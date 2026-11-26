//! The read-only custom Action `ParkMatchesPowerUpPose`: whether the mount
//! parks at the pose its next power-up seeds, which is whether a power cut
//! keeps the mount's pose.
//!
//! The answer is read from what the driver holds — the power-up pose and
//! the park target the connect resolved, the site latitude and the
//! handshake's counts per revolution — so the Action sends nothing to the
//! mount and writes no file.
//!
//! See the design doc's
//! [§`ParkMatchesPowerUpPose`](../../../../docs/services/star-adventurer-gti.md#parkmatchespoweruppose).

use ascom_alpaca::ASCOMResult;
use serde_json::{json, Value};
use tracing::debug;

use super::MountDevice;
use crate::config::ApPark;

/// The Action's name, as `SupportedActions` spells it.
pub(super) const PARK_MATCHES_POWER_UP_POSE: &str = "ParkMatchesPowerUpPose";

/// An encoder pair with a value on both axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TickPair {
    pub(super) ra: i32,
    pub(super) dec: i32,
}

impl TickPair {
    /// The pair, when both axes have a value.
    pub(super) fn of(ra: Option<i32>, dec: Option<i32>) -> Option<Self> {
        Some(Self { ra: ra?, dec: dec? })
    }

    fn to_json(self) -> Value {
        json!({ "ra": self.ra, "dec": self.dec })
    }
}

/// The Action's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ParkPoseAnswer {
    pub(super) matches: bool,
    pub(super) power_up_pose: ApPark,
    pub(super) power_up_ticks: Option<TickPair>,
    pub(super) park_target_ticks: Option<TickPair>,
}

impl ParkPoseAnswer {
    /// The answer for a power-up pose, its encoder pair and the park
    /// target. It matches only when both pairs are present and equal on
    /// both axes: two absent pairs are no match.
    pub(super) fn new(
        power_up_pose: ApPark,
        power_up_ticks: Option<TickPair>,
        park_target_ticks: Option<TickPair>,
    ) -> Self {
        let matches = power_up_ticks.is_some() && power_up_ticks == park_target_ticks;
        Self {
            matches,
            power_up_pose,
            power_up_ticks,
            park_target_ticks,
        }
    }

    /// The JSON string the Action answers. An absent pair is `null`.
    pub(super) fn to_json(self) -> String {
        json!({
            "matches": self.matches,
            "power_up_pose": self.power_up_pose.as_str(),
            "power_up_ticks": self.power_up_ticks.map(TickPair::to_json),
            "park_target_ticks": self.park_target_ticks.map(TickPair::to_json),
        })
        .to_string()
    }
}

impl MountDevice {
    /// Answer `ParkMatchesPowerUpPose`. `NOT_CONNECTED` while the mount is
    /// not connected, because the connect resolves the power-up pose and
    /// the park target the answer reads.
    ///
    /// The park target is the one the driver holds now: the connect's, or
    /// the one a `SetPark` or an anchoring sync has put in its place since.
    /// A position that is lost or awaiting its power-up check has no park
    /// target on either axis. The answer reads that from the position
    /// itself, which changes before the device state follows it.
    pub(super) async fn park_matches_power_up_pose(&self) -> ASCOMResult<String> {
        self.ensure_connected().await?;
        let (connect_pose, park_ra, park_dec) = {
            let s = self.state.read().await;
            (s.unpark_from_ap_position, s.park_ra_ticks, s.park_dec_ticks)
        };
        let power_up_pose = connect_pose.unwrap_or(self.config.unpark_from_ap_position);
        let power_up_ticks = self
            .ap_park_target_ticks(power_up_pose)
            .await
            .map(|(ra, dec)| TickPair { ra, dec });
        let park_target_ticks = if self.manager.power().position_known() {
            TickPair::of(park_ra, park_dec)
        } else {
            None
        };
        let answer = ParkPoseAnswer::new(power_up_pose, power_up_ticks, park_target_ticks);
        debug!(?answer, "answered {PARK_MATCHES_POWER_UP_POSE}");
        Ok(answer.to_json())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const POSE: TickPair = TickPair {
        ra: -907_200,
        dec: 725_760,
    };

    #[test]
    fn a_park_target_equal_to_the_power_up_ticks_matches() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark3, Some(POSE), Some(POSE));
        assert!(answer.matches, "{answer:?}");
    }

    #[test]
    fn a_park_target_off_on_ra_alone_does_not_match() {
        let target = TickPair {
            ra: POSE.ra + 1,
            ..POSE
        };
        let answer = ParkPoseAnswer::new(ApPark::ApPark3, Some(POSE), Some(target));
        assert!(!answer.matches, "{answer:?}");
    }

    #[test]
    fn a_park_target_off_on_dec_alone_does_not_match() {
        let target = TickPair {
            dec: POSE.dec - 1,
            ..POSE
        };
        let answer = ParkPoseAnswer::new(ApPark::ApPark3, Some(POSE), Some(target));
        assert!(!answer.matches, "{answer:?}");
    }

    #[test]
    fn no_park_target_does_not_match() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark3, Some(POSE), None);
        assert!(!answer.matches, "{answer:?}");
    }

    #[test]
    fn no_power_up_ticks_does_not_match_any_park_target() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark0, None, Some(POSE));
        assert!(!answer.matches, "{answer:?}");
    }

    #[test]
    fn two_absent_pairs_do_not_match() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark0, None, None);
        assert!(!answer.matches, "{answer:?}");
    }

    #[test]
    fn a_tick_pair_needs_both_axes() {
        assert_eq!(
            TickPair::of(Some(1), Some(2)),
            Some(TickPair { ra: 1, dec: 2 })
        );
        assert_eq!(TickPair::of(Some(1), None), None);
        assert_eq!(TickPair::of(None, Some(2)), None);
        assert_eq!(TickPair::of(None, None), None);
    }

    #[test]
    fn the_answer_is_the_documented_json_object() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark3, Some(POSE), Some(POSE));
        let json: Value = serde_json::from_str(&answer.to_json()).unwrap();
        assert_eq!(
            json,
            json!({
                "matches": true,
                "power_up_pose": "ap_park_3",
                "power_up_ticks": {"ra": -907_200, "dec": 725_760},
                "park_target_ticks": {"ra": -907_200, "dec": 725_760},
            })
        );
    }

    #[test]
    fn an_absent_pair_is_null_in_the_answer() {
        let answer = ParkPoseAnswer::new(ApPark::ApPark0, None, None);
        let json: Value = serde_json::from_str(&answer.to_json()).unwrap();
        assert_eq!(
            json,
            json!({
                "matches": false,
                "power_up_pose": "ap_park_0",
                "power_up_ticks": null,
                "park_target_ticks": null,
            })
        );
    }
}

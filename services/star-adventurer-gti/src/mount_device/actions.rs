//! Driver-specific ASCOM `Action` handlers for [`MountDevice`].
//!
//! The standard ASCOM Telescope interface has no concept of an
//! "unpark from a named physical pose," so the three operations that
//! the design's [§Unpark from AP position] section defines are exposed
//! as vendor `Action(name, parameters)` calls, advertised through
//! `SupportedActions`. The `impl Device` block in [`super::device`]
//! dispatches to the handlers here.
//!
//! All three take the single `parameters` string as an
//! `ap_park_0..ap_park_5` token (see [`parse_ap_park`]). The two
//! persisting Actions (`SetUnparkFromApPosition`, `SetPreferredApPark`)
//! refuse when the driver was started without `--config`, mirroring the
//! `SetPark` capability gate — there is nowhere to persist to. The
//! recovery Action (`UnparkFromApPosition`) writes no config and does
//! not need a config path.
//!
//! [§Unpark from AP position]: ../../../../docs/services/star-adventurer-gti.md#unpark-from-ap-position

use std::sync::atomic::Ordering;

use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};

use crate::config::ApPark;

use super::park_persistence::write_mount_fields_to_config;
use super::MountDevice;

/// The driver-specific ASCOM vendor Actions. Owns its `SupportedActions`
/// names and the name→variant parsing the `Action` dispatch in
/// [`super::device`] uses, so the name set lives in exactly one place.
///
/// Declaration order is the order `SupportedActions` advertises. The
/// per-variant strings are the ASCOM Alpaca vendor Action names on the
/// wire, so each is pinned explicitly rather than taken from its variant
/// name; matching is exact — case-sensitive and untrimmed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
pub(super) enum ApParkAction {
    #[strum(serialize = "SetUnparkFromApPosition")]
    SetUnparkFromApPosition,
    #[strum(serialize = "SetPreferredApPark")]
    SetPreferredApPark,
    #[strum(serialize = "UnparkFromApPosition")]
    UnparkFromApPosition,
}

impl ApParkAction {
    /// The ASCOM action name.
    pub(super) fn name(self) -> &'static str {
        self.into()
    }

    /// Resolve an ASCOM action name; `None` for an unrecognised name
    /// (the caller maps that to `ACTION_NOT_IMPLEMENTED`).
    pub(super) fn from_name(name: &str) -> Option<Self> {
        name.parse().ok()
    }
}

/// Parse an `Action`'s single `parameters` string into an [`ApPark`]
/// (whitespace-trimmed), mapping an unrecognised token to
/// `INVALID_VALUE`. The token↔variant mapping itself is canonical on
/// [`ApPark`] (`FromStr` / [`ApPark::as_str`]); this only adapts it to
/// the ASCOM error type.
fn parse_ap_park(parameter: &str) -> ASCOMResult<ApPark> {
    parameter
        .trim()
        .parse::<ApPark>()
        .map_err(|e| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, e.to_string()))
}

impl MountDevice {
    /// Persist `key = value` to the on-disk config via the atomic-rename
    /// helper, off the async runtime. Both persisting Actions route
    /// through here; the caller has already verified `config_file_path`
    /// is `Some`.
    async fn persist_mount_ap_park(&self, key: &'static str, park: ApPark) -> ASCOMResult<()> {
        let path = self.config_file_path.as_ref().ok_or_else(|| {
            ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                format!("{key} requires the driver to be started with --config <path>"),
            )
        })?;
        let path = path.clone();
        let value = serde_json::Value::String(park.as_str().to_string());
        tokio::task::spawn_blocking(move || write_mount_fields_to_config(&path, &[(key, value)]))
            .await
            .map_err(|e| {
                ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    format!("{key} write task join error: {e}"),
                )
            })?
            .map_err(ASCOMError::from)
    }

    /// `SetUnparkFromApPosition(park)` — persist a new
    /// `unpark_from_ap_position`. Lazy: the value takes effect on the
    /// *next* fresh-power-up auto-seed (`seed_after_connect` re-reads it
    /// from disk); the current session's encoder is left untouched.
    /// Operators wanting an immediate apply use `UnparkFromApPosition`.
    pub(super) async fn handle_set_unpark_from_ap_position(
        &self,
        parameter: &str,
    ) -> ASCOMResult<String> {
        let park = parse_ap_park(parameter)?;
        self.persist_mount_ap_park("unpark_from_ap_position", park)
            .await?;
        tracing::debug!(
            unpark_from_ap_position = ?park,
            "SetUnparkFromApPosition persisted to config file"
        );
        Ok(park.as_str().to_string())
    }

    /// `SetPreferredApPark(park)` — set the `Park()` target. Rejects
    /// `ap_park_0` (not a slew target). Persists to config and, when
    /// connected, re-resolves the live park target so the change applies
    /// to the next `Park()` without a reconnect; an explicit raw
    /// `park_*_ticks` override still wins per-axis.
    pub(super) async fn handle_set_preferred_ap_park(
        &self,
        parameter: &str,
    ) -> ASCOMResult<String> {
        let park = parse_ap_park(parameter)?;
        if park == ApPark::ApPark0 {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "preferred AP park cannot be ap_park_0 (\"current position\" is not a slew target)",
            ));
        }
        self.persist_mount_ap_park("preferred_ap_park", park)
            .await?;
        // Re-resolve the live park target when connected so the change
        // applies this session. `read_connect_config` re-reads the
        // (just-persisted) value plus any raw `park_*_ticks` override
        // from disk, so the raw-ticks-win rule holds. When disconnected
        // the next connect resolves it.
        if self.session.read().await.is_some() {
            let cfg = self.read_connect_config().await?;
            self.load_park_target_after_connect(&cfg).await?;
        }
        tracing::debug!(preferred_ap_park = ?park, "SetPreferredApPark persisted to config file");
        Ok(park.as_str().to_string())
    }

    /// `UnparkFromApPosition(park)` — recovery unpark. The operator
    /// asserts the OTA is physically at `park`; the driver makes the
    /// firmware encoder match, *regardless* of the current encoder
    /// state, then clears `AtPark`.
    ///
    /// Refuses when disconnected (`NOT_CONNECTED`), not parked, or
    /// slewing (`INVALID_OPERATION`). For `ap_park_0` ("current
    /// position") there is no encoder change — it is the standard
    /// `Unpark()` end state. For `ap_park_1..ap_park_5` it runs the
    /// [`MountDevice::reset_mount_encoders`] safe-stop-then-seed
    /// sequence before clearing `AtPark`.
    pub(super) async fn handle_unpark_from_ap_position(
        &self,
        parameter: &str,
    ) -> ASCOMResult<String> {
        let park = parse_ap_park(parameter)?;
        self.ensure_connected().await?;
        if !self.state.read().await.at_park {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "UnparkFromApPosition refused: mount is not parked",
            ));
        }
        if self.slew_in_progress.load(Ordering::SeqCst) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "UnparkFromApPosition refused: slew in progress",
            ));
        }
        if park == ApPark::ApPark0 {
            // "Current position": no encoder change — equivalent to a
            // standard `Unpark()`. The operator will plate-solve + sync.
            tracing::debug!("UnparkFromApPosition(ap_park_0): clearing AtPark, no encoder change");
        } else {
            // The operator-confirmed physical pose: make the firmware
            // encoder match it regardless of current state.
            let (ra_ticks, dec_ticks) = self
                .ap_park_target_ticks(park)
                .await
                .ok_or(ASCOMError::NOT_CONNECTED)?;
            self.with_session(async |session| {
                self.reset_mount_encoders(session, ra_ticks, dec_ticks)
                    .await
            })
            .await?;
            // The operator just asserted the physical pose: that is
            // ground truth for the encoder→pose mapping. Anchor the
            // frame and arm any park-target axis an unanchored connect
            // left empty.
            self.anchor_frame_and_rearm_park_target().await;
            tracing::debug!(
                unpark_from_ap_position = ?park,
                seeded_ra_ticks = ra_ticks,
                seeded_dec_ticks = dec_ticks,
                "UnparkFromApPosition reset firmware encoder to the named AP park"
            );
        }
        self.state.write().await.at_park = false;
        Ok(park.as_str().to_string())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use strum::VariantArray;

    use super::ApParkAction;

    #[test]
    fn all_is_the_supported_actions_order() {
        assert_eq!(
            ApParkAction::VARIANTS,
            [
                ApParkAction::SetUnparkFromApPosition,
                ApParkAction::SetPreferredApPark,
                ApParkAction::UnparkFromApPosition,
            ]
        );
        // Variant order is the order `SupportedActions` advertises.
        let names: Vec<&'static str> = ApParkAction::VARIANTS.iter().map(|a| a.name()).collect();
        assert_eq!(
            names,
            vec![
                "SetUnparkFromApPosition",
                "SetPreferredApPark",
                "UnparkFromApPosition",
            ]
        );
    }

    #[test]
    fn name_is_the_exact_ascom_action_string() {
        assert_eq!(
            ApParkAction::SetUnparkFromApPosition.name(),
            "SetUnparkFromApPosition"
        );
        assert_eq!(
            ApParkAction::SetPreferredApPark.name(),
            "SetPreferredApPark"
        );
        assert_eq!(
            ApParkAction::UnparkFromApPosition.name(),
            "UnparkFromApPosition"
        );
    }

    #[test]
    fn from_name_round_trips_every_variant() {
        for &action in ApParkAction::VARIANTS {
            assert_eq!(ApParkAction::from_name(action.name()).unwrap(), action);
        }
    }

    #[test]
    fn from_name_is_case_sensitive_and_rejects_near_misses() {
        // Names match exactly — no case folding, no trimming. Anything
        // else falls through to `ACTION_NOT_IMPLEMENTED` in dispatch.
        for name in [
            "setunparkfromapposition",
            "SETUNPARKFROMAPPOSITION",
            "SetUnParkFromApPosition",
            "set_unpark_from_ap_position",
            "SetPreferredAPPark",
            "setPreferredApPark",
            "unparkFromApPosition",
            "UnparkFromApPosition ",
            " UnparkFromApPosition",
            "UnparkFromApPositions",
            "",
            "NoSuchAction",
        ] {
            assert_eq!(ApParkAction::from_name(name), None, "name {name:?}");
        }
    }
}

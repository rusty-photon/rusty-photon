//! Validated ICRS pointing.

use serde::{Deserialize, Serialize};

/// A J2000 mean equator/equinox (ICRS) pointing, valid by construction.
///
/// Parse-don't-validate: the only way to a value is [`IcrsCoord::try_new`]
/// (or deserialization, which routes through it), so an out-of-range
/// coordinate is unrepresentable. Fields are private; read them via
/// [`IcrsCoord::ra_hours`] / [`IcrsCoord::dec_degrees`]. The wire form is
/// the flat `{ra_hours, dec_degrees}` pair, one shape on disk and on the
/// MCP wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(try_from = "IcrsCoordWire", into = "IcrsCoordWire")]
pub struct IcrsCoord {
    ra_hours: f64,
    dec_degrees: f64,
}

impl IcrsCoord {
    /// Validates `ra_hours ∈ [0, 24)` and `dec_degrees ∈ [-90, 90]`.
    ///
    /// Both ranges admit `-0.0`, which is stored as `0.0` (`+ 0.0` is
    /// zero for either sign of zero and the identity for every other
    /// value), so a coordinate never reads back or serializes as `-0.0`.
    ///
    /// # Errors
    ///
    /// [`CoordError::RaOutOfRange`] or [`CoordError::DecOutOfRange`],
    /// naming the offending value.
    pub fn try_new(ra_hours: f64, dec_degrees: f64) -> Result<Self, CoordError> {
        if !(0.0..24.0).contains(&ra_hours) {
            return Err(CoordError::RaOutOfRange { ra_hours });
        }
        if !(-90.0..=90.0).contains(&dec_degrees) {
            return Err(CoordError::DecOutOfRange { dec_degrees });
        }
        Ok(Self {
            ra_hours: ra_hours + 0.0,
            dec_degrees: dec_degrees + 0.0,
        })
    }

    /// Right ascension in decimal hours, `[0, 24)`.
    #[must_use]
    pub const fn ra_hours(&self) -> f64 {
        self.ra_hours
    }

    /// Declination in decimal degrees, `[-90, 90]`.
    #[must_use]
    pub const fn dec_degrees(&self) -> f64 {
        self.dec_degrees
    }
}

/// The flat serde shape [`IcrsCoord`] (de)serializes through, so on-disk
/// and wire forms stay `{ra_hours, dec_degrees}` while construction is
/// still validated (`try_from` routes through [`IcrsCoord::try_new`]).
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
struct IcrsCoordWire {
    ra_hours: f64,
    dec_degrees: f64,
}

impl TryFrom<IcrsCoordWire> for IcrsCoord {
    type Error = CoordError;

    fn try_from(w: IcrsCoordWire) -> Result<Self, Self::Error> {
        Self::try_new(w.ra_hours, w.dec_degrees)
    }
}

impl From<IcrsCoord> for IcrsCoordWire {
    fn from(c: IcrsCoord) -> Self {
        Self {
            ra_hours: c.ra_hours,
            dec_degrees: c.dec_degrees,
        }
    }
}

/// Errors constructing an [`IcrsCoord`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CoordError {
    /// `ra_hours` was outside `[0, 24)`.
    #[error("ra_hours {ra_hours} is outside [0, 24)")]
    RaOutOfRange {
        /// The offending right ascension.
        ra_hours: f64,
    },
    /// `dec_degrees` was outside `[-90, 90]`.
    #[error("dec_degrees {dec_degrees} is outside [-90, 90]")]
    DecOutOfRange {
        /// The offending declination.
        dec_degrees: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_new_accepts_in_range_including_boundaries() {
        let c = IcrsCoord::try_new(0.0, 90.0).unwrap();
        assert_eq!(c.ra_hours(), 0.0);
        assert_eq!(c.dec_degrees(), 90.0);
        IcrsCoord::try_new(23.999, -90.0).unwrap();
    }

    #[test]
    fn try_new_rejects_ra_out_of_range() {
        assert!(matches!(
            IcrsCoord::try_new(24.0, 0.0),
            Err(CoordError::RaOutOfRange { .. })
        ));
        assert!(matches!(
            IcrsCoord::try_new(-0.1, 0.0),
            Err(CoordError::RaOutOfRange { .. })
        ));
    }

    #[test]
    fn try_new_rejects_dec_out_of_range() {
        assert!(matches!(
            IcrsCoord::try_new(0.0, 90.1),
            Err(CoordError::DecOutOfRange { .. })
        ));
        assert!(matches!(
            IcrsCoord::try_new(0.0, -90.1),
            Err(CoordError::DecOutOfRange { .. })
        ));
    }

    #[test]
    fn serde_round_trips_through_the_flat_wire() {
        let c = IcrsCoord::try_new(1.0, 2.0).unwrap();
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v, serde_json::json!({"ra_hours": 1.0, "dec_degrees": 2.0}));
        assert_eq!(serde_json::from_value::<IcrsCoord>(v).unwrap(), c);
    }

    #[test]
    fn a_negative_zero_is_stored_and_serialized_as_zero() {
        // `-0.0 == 0.0`, so the sign is what the assertions read.
        let c = IcrsCoord::try_new(-0.0, -0.0).unwrap();
        assert!(c.ra_hours().is_sign_positive(), "{c:?}");
        assert!(c.dec_degrees().is_sign_positive(), "{c:?}");
        let wire: IcrsCoord =
            serde_json::from_str(r#"{"ra_hours": -0.0, "dec_degrees": -0.0}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            r#"{"ra_hours":0.0,"dec_degrees":0.0}"#
        );
    }

    #[test]
    fn deserialize_rejects_an_out_of_range_wire_value() {
        let bad = serde_json::json!({"ra_hours": 99.0, "dec_degrees": 0.0});
        assert!(serde_json::from_value::<IcrsCoord>(bad).is_err());
    }
}

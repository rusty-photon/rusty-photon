//! Frame binning — the `(x, y)` factor pair, rendered `"2x2"`.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Frame binning, rendered as `"{x}x{y}"` (e.g. `"1x1"`, `"2x2"`).
///
/// The `(filter, binning, exposure_duration)` triple is the acquisition quota key.
/// `x`/`y` stay public: any `u8 × u8` is shape-valid, so there is no bound
/// to protect. [`FromStr`] is the exact inverse of the derived `Display`,
/// so a rendered `{binning}` token round-trips.
///
/// Serde uses the same `"AxB"` string, not the derived struct shape:
/// `#[serde(into = "String", try_from = "String")]` delegates to
/// `Display`/`FromStr` (the same derive-plus-wire-conversion pattern
/// [`crate::IcrsCoord`] uses) — one canonical encoding across the redb
/// store, the MCP wire, config, and the filename token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, derive_more::Display)]
#[display("{x}x{y}")]
#[serde(into = "String", try_from = "String")]
pub struct Binning {
    /// Horizontal binning factor.
    pub x: u8,
    /// Vertical binning factor.
    pub y: u8,
}

impl From<Binning> for String {
    fn from(b: Binning) -> Self {
        b.to_string()
    }
}

impl TryFrom<String> for Binning {
    type Error = BinningParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Manual because the derive generates the *struct* schema from the
/// fields — it cannot follow `#[serde(into/try_from)]` to the string
/// wire shape. (`IcrsCoord` gets away with deriving only because its
/// private fields happen to mirror its wire type field-for-field.)
#[cfg(feature = "schema")]
impl schemars::JsonSchema for Binning {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Binning")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9]+x[0-9]+$",
            "description": "Frame binning as \"AxB\", e.g. \"1x1\", \"2x2\"",
        })
    }
}

impl FromStr for Binning {
    type Err = BinningParseError;

    /// Parses `"AxB"` (e.g. `"1x1"`, `"2x2"`) into a [`Binning`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (x, y) = s
            .split_once('x')
            .ok_or_else(|| BinningParseError(s.to_string()))?;
        let x = x
            .parse::<u8>()
            .map_err(|_| BinningParseError(s.to_string()))?;
        let y = y
            .parse::<u8>()
            .map_err(|_| BinningParseError(s.to_string()))?;
        Ok(Self { x, y })
    }
}

/// The input was not `"AxB"` with two `u8` factors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid binning {0:?}: expected \"AxB\", e.g. \"1x1\"")]
pub struct BinningParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_as_wxh() {
        assert_eq!(Binning { x: 2, y: 2 }.to_string(), "2x2");
    }

    #[test]
    fn from_str_round_trips_display() {
        let b = Binning { x: 3, y: 1 };
        assert_eq!(Binning::from_str(&b.to_string()).unwrap(), b);
    }

    #[test]
    fn from_str_rejects_non_axb() {
        assert!("1".parse::<Binning>().is_err());
        assert!("1x".parse::<Binning>().is_err());
        assert!("x1".parse::<Binning>().is_err());
        assert!("1x1x1".parse::<Binning>().is_err());
    }

    #[test]
    fn from_str_rejects_out_of_u8_range() {
        assert!("999x1".parse::<Binning>().is_err());
    }

    #[test]
    fn serde_round_trips_as_the_axb_string() {
        let b = Binning { x: 2, y: 2 };
        let v = serde_json::to_value(b).unwrap();
        assert_eq!(v, serde_json::json!("2x2"));
        assert_eq!(serde_json::from_value::<Binning>(v).unwrap(), b);
    }

    // `{"x": 2, "y": 2}` is what a plain derive would accept; the wire form
    // is only the `"AxB"` string.
    #[test]
    fn deserialize_rejects_the_derived_struct_shape_and_bad_strings() {
        assert!(serde_json::from_value::<Binning>(serde_json::json!({"x": 2, "y": 2})).is_err());
        assert!(serde_json::from_value::<Binning>(serde_json::json!("2x")).is_err());
    }
}

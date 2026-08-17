//! ASCOM error-mapping helpers shared across drivers.
//!
//! The common driver error *enum* is generated per driver by
//! [`driver_error!`](crate::driver_error); this module holds the one mapping that
//! can't be a `From` impl — `ApplyError` → `ASCOMError` — because both types are
//! foreign to this crate (the orphan rule forbids the impl here).

use ascom_alpaca::{ASCOMError, ASCOMErrorCode};
use rusty_photon_config::actions::ApplyError;

/// The `config.apply` error mapping, used by the generic config-action
/// [`dispatch`](crate::dispatch). A parse failure is a client error
/// (`INVALID_VALUE`); a read / persist / internal-serialize failure is an
/// operation failure (`INVALID_OPERATION`). Validation failures never reach here
/// — they come back as an `Ok(ConfigApplyResponse{ status: invalid })`.
///
/// A free function rather than `impl From<ApplyError> for ASCOMError`: the orphan
/// rule forbids that impl in this crate (both `ApplyError` and `ASCOMError` are
/// foreign here), and the dispatch is the only caller.
#[must_use]
pub fn apply_error_to_ascom(err: &ApplyError) -> ASCOMError {
    let code = match err {
        ApplyError::Parse(_) => ASCOMErrorCode::INVALID_VALUE,
        ApplyError::ReadFile(_) | ApplyError::Persist(_) | ApplyError::Serialize(_) => {
            ASCOMErrorCode::INVALID_OPERATION
        }
    };
    ASCOMError::new(code, format!("config.apply: {err}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn apply_parse_error_maps_to_invalid_value() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{ not json").unwrap_err();
        let e = apply_error_to_ascom(&ApplyError::Parse(serde_err));
        assert_eq!(e.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[test]
    fn apply_persist_and_serialize_map_to_invalid_operation() {
        let persist =
            apply_error_to_ascom(&ApplyError::Persist(std::io::Error::other("disk full")));
        assert_eq!(persist.code, ASCOMErrorCode::INVALID_OPERATION);
        let serde_err = serde_json::from_str::<serde_json::Value>("{ bad").unwrap_err();
        let serialize = apply_error_to_ascom(&ApplyError::Serialize(serde_err));
        assert_eq!(serialize.code, ASCOMErrorCode::INVALID_OPERATION);
    }
}

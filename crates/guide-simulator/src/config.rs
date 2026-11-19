//! What construction refuses, and the checks that refuse it.

/// A configuration value the simulator cannot model, named by its field.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field} {reason}")]
pub struct ConfigError {
    /// The field, as it is written in the configuration types.
    pub field: &'static str,
    /// What is wrong with its value.
    pub reason: &'static str,
}

impl ConfigError {
    const fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

/// `value` is finite.
pub const fn finite(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ConfigError::new(field, "must be finite"))
    }
}

/// `value` is finite and above zero.
pub fn positive(field: &'static str, value: f64) -> Result<(), ConfigError> {
    finite(field, value)?;
    if value > 0.0 {
        Ok(())
    } else {
        Err(ConfigError::new(field, "must be above zero"))
    }
}

/// `value` is finite and not below zero.
pub fn non_negative(field: &'static str, value: f64) -> Result<(), ConfigError> {
    finite(field, value)?;
    if value >= 0.0 {
        Ok(())
    } else {
        Err(ConfigError::new(field, "must not be negative"))
    }
}

/// `value` is finite and within `low..=high`.
pub fn within(field: &'static str, value: f64, low: f64, high: f64) -> Result<(), ConfigError> {
    finite(field, value)?;
    if (low..=high).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::new(field, "is out of range"))
    }
}

/// `holds` is true; otherwise `field` is refused for `reason`.
pub const fn require(
    holds: bool,
    field: &'static str,
    reason: &'static str,
) -> Result<(), ConfigError> {
    if holds {
        Ok(())
    } else {
        Err(ConfigError::new(field, reason))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn positive_refuses_zero_and_names_the_field() {
        let err = positive("camera.gain_e_per_adu", 0.0).unwrap_err();
        assert_eq!(err.to_string(), "camera.gain_e_per_adu must be above zero");
    }

    #[test]
    fn every_check_refuses_a_value_that_is_not_finite() {
        assert_eq!(finite("f", f64::NAN).unwrap_err().reason, "must be finite");
        assert_eq!(
            positive("f", f64::INFINITY).unwrap_err().reason,
            "must be finite"
        );
        assert_eq!(
            non_negative("f", f64::NAN).unwrap_err().reason,
            "must be finite"
        );
        assert_eq!(
            within("f", f64::NAN, 0.0, 1.0).unwrap_err().reason,
            "must be finite"
        );
    }

    #[test]
    fn non_negative_takes_zero_and_refuses_below_it() {
        non_negative("f", 0.0).unwrap();
        assert_eq!(
            non_negative("f", -0.1).unwrap_err().reason,
            "must not be negative"
        );
    }

    #[test]
    fn within_takes_both_ends() {
        within("f", 0.0, 0.0, 1.0).unwrap();
        within("f", 1.0, 0.0, 1.0).unwrap();
        assert_eq!(
            within("f", 1.1, 0.0, 1.0).unwrap_err().reason,
            "is out of range"
        );
    }

    #[test]
    fn require_refuses_with_its_reason() {
        require(true, "f", "never").unwrap();
        assert_eq!(
            require(false, "f", "is wrong").unwrap_err().reason,
            "is wrong"
        );
    }
}

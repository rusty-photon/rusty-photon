//! Switch definitions for the PPBA device
//!
//! This module defines all switches exposed by the PPBA device via the ASCOM Switch interface.
//! Switches are numbered from 0 to `MAX_SWITCH` - 1.

/// Total number of switches exposed by the PPBA device
pub const MAX_SWITCH: usize = <SwitchId as strum::EnumCount>::COUNT;

/// Switch identifiers for the PPBA device
///
/// The ASCOM switch id is [`SwitchInfo::id`], not the variant's discriminant,
/// so variants may be reordered freely. The ids must stay contiguous from
/// zero: `MAX_SWITCH` is the variant count, so every id in `0..MAX_SWITCH`
/// has to resolve through [`Self::from_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumCount, strum::VariantArray)]
pub enum SwitchId {
    // Controllable switches (CanWrite = true)
    /// Quad 12V output (boolean: 0=off, 1=on)
    Quad12V,
    /// Adjustable output (boolean: 0=off, 1=on)
    AdjustableOutput,
    /// Dew Heater A PWM (analog: 0-255)
    DewHeaterA,
    /// Dew Heater B PWM (analog: 0-255)
    DewHeaterB,
    /// USB Hub control (boolean: 0=off, 1=on)
    UsbHub,
    /// Auto-Dew enable (boolean: 0=off, 1=on)
    AutoDew,

    // Read-only switches - Power Statistics (from PS command)
    /// Average current draw in Amps
    AverageCurrent,
    /// Cumulative amp-hours consumed
    AmpHours,
    /// Cumulative watt-hours consumed
    WattHours,
    /// Device uptime in hours
    Uptime,

    // Read-only switches - Sensor Data (from PA command)
    /// Input voltage in Volts
    InputVoltage,
    /// Total current draw in Amps
    TotalCurrent,
    /// Ambient temperature in Celsius
    Temperature,
    /// Relative humidity percentage
    Humidity,
    /// Calculated dewpoint in Celsius
    Dewpoint,
    /// Power warning flag (overcurrent/short)
    PowerWarning,
}

impl SwitchId {
    /// Try to convert a usize to a `SwitchId`, returning `None` when no
    /// switch carries that id.
    #[must_use]
    pub fn from_id(id: usize) -> Option<Self> {
        <Self as strum::VariantArray>::VARIANTS
            .iter()
            .copied()
            .find(|switch| switch.info().id == id)
    }

    /// Get the numeric ID for this switch
    #[must_use]
    pub const fn id(&self) -> usize {
        self.info().id
    }

    /// Get the switch information for this switch
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive switch table: one row per variant, no logic to extract"
    )]
    pub const fn info(&self) -> SwitchInfo {
        match self {
            // Controllable switches
            Self::Quad12V => SwitchInfo {
                id: 0,
                name: "Quad 12V Output",
                description: "Controls the quad 12V power output",
                can_write: true,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },
            Self::AdjustableOutput => SwitchInfo {
                id: 1,
                name: "Adjustable Output",
                description: "Controls the adjustable voltage output on/off",
                can_write: true,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },
            Self::DewHeaterA => SwitchInfo {
                id: 2,
                name: "Dew Heater A",
                description: "PWM control for Dew Heater A (0-255)",
                can_write: true,
                min_value: 0.0,
                max_value: 255.0,
                step: 1.0,
            },
            Self::DewHeaterB => SwitchInfo {
                id: 3,
                name: "Dew Heater B",
                description: "PWM control for Dew Heater B (0-255)",
                can_write: true,
                min_value: 0.0,
                max_value: 255.0,
                step: 1.0,
            },
            Self::UsbHub => SwitchInfo {
                id: 4,
                name: "USB Hub",
                description: "Controls the USB 2.0 hub power",
                can_write: true,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },
            Self::AutoDew => SwitchInfo {
                id: 5,
                name: "Auto-Dew",
                description: "Enables automatic dew heater control",
                can_write: true,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },

            // Read-only switches - Power Statistics
            Self::AverageCurrent => SwitchInfo {
                id: 6,
                name: "Average Current",
                description: "Average current draw in Amps",
                can_write: false,
                min_value: 0.0,
                max_value: 20.0,
                step: 0.01,
            },
            Self::AmpHours => SwitchInfo {
                id: 7,
                name: "Amp Hours",
                description: "Cumulative amp-hours consumed",
                can_write: false,
                min_value: 0.0,
                max_value: 9999.0,
                step: 0.01,
            },
            Self::WattHours => SwitchInfo {
                id: 8,
                name: "Watt Hours",
                description: "Cumulative watt-hours consumed",
                can_write: false,
                min_value: 0.0,
                max_value: 99999.0,
                step: 0.1,
            },
            Self::Uptime => SwitchInfo {
                id: 9,
                name: "Uptime",
                description: "Device uptime in hours",
                can_write: false,
                min_value: 0.0,
                max_value: 99999.0,
                step: 0.01,
            },

            // Read-only switches - Sensor Data
            Self::InputVoltage => SwitchInfo {
                id: 10,
                name: "Input Voltage",
                description: "Input voltage in Volts",
                can_write: false,
                min_value: 0.0,
                max_value: 15.0,
                step: 0.1,
            },
            Self::TotalCurrent => SwitchInfo {
                id: 11,
                name: "Total Current",
                description: "Total current draw in Amps",
                can_write: false,
                min_value: 0.0,
                max_value: 20.0,
                step: 0.01,
            },
            Self::Temperature => SwitchInfo {
                id: 12,
                name: "Temperature",
                description: "Ambient temperature in Celsius",
                can_write: false,
                min_value: -40.0,
                max_value: 60.0,
                step: 0.1,
            },
            Self::Humidity => SwitchInfo {
                id: 13,
                name: "Humidity",
                description: "Relative humidity percentage",
                can_write: false,
                min_value: 0.0,
                max_value: 100.0,
                step: 1.0,
            },
            Self::Dewpoint => SwitchInfo {
                id: 14,
                name: "Dewpoint",
                description: "Calculated dewpoint in Celsius",
                can_write: false,
                min_value: -40.0,
                max_value: 60.0,
                step: 0.1,
            },
            Self::PowerWarning => SwitchInfo {
                id: 15,
                name: "Power Warning",
                description: "Power warning flag (overcurrent/short circuit)",
                can_write: false,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },
        }
    }
}

/// Information about a switch
#[derive(Debug, Clone)]
pub struct SwitchInfo {
    pub id: usize,
    pub name: &'static str,
    pub description: &'static str,
    pub can_write: bool,
    pub min_value: f64,
    pub max_value: f64,
    pub step: f64,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn max_switch_is_sixteen() {
        assert_eq!(MAX_SWITCH, 16);
    }

    #[test]
    fn all_switch_ids_are_valid() {
        for id in 0..MAX_SWITCH {
            let switch_id = SwitchId::from_id(id);
            assert!(switch_id.is_some(), "Switch ID {id} should be valid");
        }
    }

    #[test]
    fn switch_id_beyond_max_is_invalid() {
        for id in MAX_SWITCH..=20 {
            assert_eq!(SwitchId::from_id(id), None, "id {id} must not map");
        }
        for id in [100, 65535, usize::MAX] {
            assert_eq!(SwitchId::from_id(id), None, "id {id} must not map");
        }
    }

    #[test]
    fn from_id_maps_each_id_to_its_variant() {
        assert_eq!(SwitchId::from_id(0), Some(SwitchId::Quad12V));
        assert_eq!(SwitchId::from_id(1), Some(SwitchId::AdjustableOutput));
        assert_eq!(SwitchId::from_id(2), Some(SwitchId::DewHeaterA));
        assert_eq!(SwitchId::from_id(3), Some(SwitchId::DewHeaterB));
        assert_eq!(SwitchId::from_id(4), Some(SwitchId::UsbHub));
        assert_eq!(SwitchId::from_id(5), Some(SwitchId::AutoDew));
        assert_eq!(SwitchId::from_id(6), Some(SwitchId::AverageCurrent));
        assert_eq!(SwitchId::from_id(7), Some(SwitchId::AmpHours));
        assert_eq!(SwitchId::from_id(8), Some(SwitchId::WattHours));
        assert_eq!(SwitchId::from_id(9), Some(SwitchId::Uptime));
        assert_eq!(SwitchId::from_id(10), Some(SwitchId::InputVoltage));
        assert_eq!(SwitchId::from_id(11), Some(SwitchId::TotalCurrent));
        assert_eq!(SwitchId::from_id(12), Some(SwitchId::Temperature));
        assert_eq!(SwitchId::from_id(13), Some(SwitchId::Humidity));
        assert_eq!(SwitchId::from_id(14), Some(SwitchId::Dewpoint));
        assert_eq!(SwitchId::from_id(15), Some(SwitchId::PowerWarning));
    }

    #[test]
    fn switch_id_roundtrip() {
        for id in 0..MAX_SWITCH {
            let switch_id = SwitchId::from_id(id).unwrap();
            assert_eq!(switch_id.id(), id);
        }
    }

    #[test]
    fn all_switches_have_info() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).map(|s| s.info());
            assert!(info.is_some(), "Switch {id} should have info");
        }
    }

    #[test]
    fn switch_info_has_valid_ranges() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                info.min_value <= info.max_value,
                "Switch {} min ({}) > max ({})",
                id,
                info.min_value,
                info.max_value
            );
            assert!(info.step > 0.0, "Switch {id} step must be positive");
        }
    }

    #[test]
    fn controllable_switches_are_writable() {
        let writable_ids = [0, 1, 2, 3, 4, 5];
        for id in writable_ids {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                info.can_write,
                "Switch {} ({}) should be writable",
                id, info.name
            );
        }
    }

    #[test]
    fn sensor_switches_are_readonly() {
        for id in 6..16 {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                !info.can_write,
                "Switch {} ({}) should be read-only",
                id, info.name
            );
        }
    }

    #[test]
    fn boolean_switches_have_correct_range() {
        let boolean_ids = [0, 1, 4, 5, 15];
        for id in boolean_ids {
            let info = SwitchId::from_id(id).unwrap().info();
            assert_eq!(info.min_value, 0.0, "Boolean switch {id} min should be 0");
            assert_eq!(info.max_value, 1.0, "Boolean switch {id} max should be 1");
            assert_eq!(info.step, 1.0, "Boolean switch {id} step should be 1");
        }
    }

    #[test]
    fn pwm_switches_have_correct_range() {
        let pwm_ids = [2, 3];
        for id in pwm_ids {
            let info = SwitchId::from_id(id).unwrap().info();
            assert_eq!(info.min_value, 0.0, "PWM switch {id} min should be 0");
            assert_eq!(info.max_value, 255.0, "PWM switch {id} max should be 255");
            assert_eq!(info.step, 1.0, "PWM switch {id} step should be 1");
        }
    }

    #[test]
    fn switch_names_are_not_empty() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                !info.name.is_empty(),
                "Switch {id} name should not be empty"
            );
        }
    }

    #[test]
    fn switch_descriptions_are_not_empty() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                !info.description.is_empty(),
                "Switch {id} description should not be empty"
            );
        }
    }

    #[test]
    fn specific_switch_names() {
        assert_eq!(SwitchId::from_id(0).unwrap().info().name, "Quad 12V Output");
        assert_eq!(
            SwitchId::from_id(1).unwrap().info().name,
            "Adjustable Output"
        );
        assert_eq!(SwitchId::from_id(2).unwrap().info().name, "Dew Heater A");
        assert_eq!(SwitchId::from_id(3).unwrap().info().name, "Dew Heater B");
        assert_eq!(SwitchId::from_id(4).unwrap().info().name, "USB Hub");
        assert_eq!(SwitchId::from_id(5).unwrap().info().name, "Auto-Dew");
        assert_eq!(SwitchId::from_id(6).unwrap().info().name, "Average Current");
        assert_eq!(SwitchId::from_id(7).unwrap().info().name, "Amp Hours");
        assert_eq!(SwitchId::from_id(8).unwrap().info().name, "Watt Hours");
        assert_eq!(SwitchId::from_id(9).unwrap().info().name, "Uptime");
        assert_eq!(SwitchId::from_id(10).unwrap().info().name, "Input Voltage");
        assert_eq!(SwitchId::from_id(11).unwrap().info().name, "Total Current");
        assert_eq!(SwitchId::from_id(12).unwrap().info().name, "Temperature");
        assert_eq!(SwitchId::from_id(13).unwrap().info().name, "Humidity");
        assert_eq!(SwitchId::from_id(14).unwrap().info().name, "Dewpoint");
        assert_eq!(SwitchId::from_id(15).unwrap().info().name, "Power Warning");
    }
}

//! Switch definitions for the UPBv2 device.
//!
//! This module defines every switch exposed through the ASCOM Switch
//! interface. Switches are numbered from 0 to [`MAX_SWITCH`] - 1; the table
//! is the one in `docs/services/upbv2-driver.md`.
//!
//! Unlike `ppba-driver`'s flat enum, the ids here come in families — four 12 V
//! outputs, three dew channels, six USB ports, and their per-channel current
//! and overcurrent readings. [`SwitchId`] therefore carries the channel with
//! the variant, which keeps `match` exhaustive at every use site: a new family
//! member is a compile error rather than a silently missing row.
//!
//! Ids are written as literals in both [`SwitchId::from_id`] and
//! [`SwitchId::info`] rather than computed from family offsets. The two are
//! kept in agreement by `every_id_in_range_resolves`, which round-trips every
//! id in `0..MAX_SWITCH` through both — so a drifted row fails the build, and
//! the table stays readable as data.

use crate::protocol::{DewChannel, OutputId, UsbPortId, VARIABLE_VOLTS_MAX, VARIABLE_VOLTS_MIN};

/// Total number of switches exposed by the UPBv2 device.
pub const MAX_SWITCH: usize = 39;

/// The variable-output switch publishes its range as `f64` literals because a
/// `u8`→`f64` conversion is not available in a `const fn`. These assertions
/// make the two representations impossible to drift apart.
const _: () = assert!(VARIABLE_VOLTS_MIN == 3);
const _: () = assert!(VARIABLE_VOLTS_MAX == 12);

/// Switch identifiers for the UPBv2 device.
///
/// The ASCOM switch id is [`SwitchInfo::id`], not the variant's discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchId {
    // ---- Controllable ----
    /// One of the four switched 12 V outputs.
    Output(OutputId),
    /// One of the three dew heater channels (PWM 0-255).
    DewHeater(DewChannel),
    /// The 3-12 V variable output. Writing it stores to EEPROM.
    VariableVoltage,
    /// One of the six switched USB ports.
    UsbPort(UsbPortId),

    // ---- Read-only: sensors ----
    /// Input voltage in Volts.
    InputVoltage,
    /// Total current draw in Amps.
    TotalCurrent,
    /// Total power draw in Watts.
    PowerDraw,
    /// Ambient temperature in Celsius.
    Temperature,
    /// Relative humidity percentage.
    Humidity,
    /// Calculated dewpoint in Celsius.
    Dewpoint,

    // ---- Read-only: per-channel telemetry ----
    /// Current draw of one 12 V output, in Amps.
    OutputCurrent(OutputId),
    /// Current draw of one dew channel, in Amps.
    DewCurrent(DewChannel),
    /// Overcurrent / short-circuit flag for one 12 V output.
    OutputOvercurrent(OutputId),
    /// Overcurrent / short-circuit flag for one dew channel.
    DewOvercurrent(DewChannel),
    /// Which dew channels the device drives itself (0-7). Read-only: this
    /// driver never writes `PD:`.
    AutoDewChannels,

    // ---- Read-only: power counters ----
    /// Average current draw in Amps.
    AverageCurrent,
    /// Cumulative amp-hours consumed.
    AmpHours,
    /// Cumulative watt-hours consumed.
    WattHours,
    /// Device uptime in hours.
    Uptime,
}

impl SwitchId {
    /// Every switch, in ascending id order.
    #[must_use]
    pub fn all() -> Vec<Self> {
        (0..MAX_SWITCH).filter_map(Self::from_id).collect()
    }

    /// Try to convert a usize to a `SwitchId`, returning `None` when no switch
    /// carries that id.
    #[must_use]
    pub const fn from_id(id: usize) -> Option<Self> {
        Some(match id {
            0 => Self::Output(OutputId::One),
            1 => Self::Output(OutputId::Two),
            2 => Self::Output(OutputId::Three),
            3 => Self::Output(OutputId::Four),
            4 => Self::DewHeater(DewChannel::A),
            5 => Self::DewHeater(DewChannel::B),
            6 => Self::DewHeater(DewChannel::C),
            7 => Self::VariableVoltage,
            8 => Self::UsbPort(UsbPortId::One),
            9 => Self::UsbPort(UsbPortId::Two),
            10 => Self::UsbPort(UsbPortId::Three),
            11 => Self::UsbPort(UsbPortId::Four),
            12 => Self::UsbPort(UsbPortId::Five),
            13 => Self::UsbPort(UsbPortId::Six),
            14 => Self::InputVoltage,
            15 => Self::TotalCurrent,
            16 => Self::PowerDraw,
            17 => Self::Temperature,
            18 => Self::Humidity,
            19 => Self::Dewpoint,
            20 => Self::OutputCurrent(OutputId::One),
            21 => Self::OutputCurrent(OutputId::Two),
            22 => Self::OutputCurrent(OutputId::Three),
            23 => Self::OutputCurrent(OutputId::Four),
            24 => Self::DewCurrent(DewChannel::A),
            25 => Self::DewCurrent(DewChannel::B),
            26 => Self::DewCurrent(DewChannel::C),
            27 => Self::OutputOvercurrent(OutputId::One),
            28 => Self::OutputOvercurrent(OutputId::Two),
            29 => Self::OutputOvercurrent(OutputId::Three),
            30 => Self::OutputOvercurrent(OutputId::Four),
            31 => Self::DewOvercurrent(DewChannel::A),
            32 => Self::DewOvercurrent(DewChannel::B),
            33 => Self::DewOvercurrent(DewChannel::C),
            34 => Self::AutoDewChannels,
            35 => Self::AverageCurrent,
            36 => Self::AmpHours,
            37 => Self::WattHours,
            38 => Self::Uptime,
            _ => return None,
        })
    }

    /// The numeric ASCOM id for this switch.
    #[must_use]
    pub const fn id(&self) -> usize {
        self.info().id
    }

    /// The switch metadata: id, name, description, writability and range.
    #[must_use]
    pub const fn info(&self) -> SwitchInfo {
        match *self {
            Self::Output(port) => output_info(port),
            Self::DewHeater(channel) => dew_info(channel),
            Self::VariableVoltage => SwitchInfo {
                id: 7,
                name: "Variable Output Voltage",
                description: "Variable output voltage setpoint in Volts (3-12). \
                              Stored in the device's EEPROM",
                can_write: true,
                min_value: 3.0,
                max_value: 12.0,
                step: 1.0,
            },
            Self::UsbPort(port) => usb_info(port),
            Self::InputVoltage
            | Self::TotalCurrent
            | Self::PowerDraw
            | Self::Temperature
            | Self::Humidity
            | Self::Dewpoint => sensor_info(*self),
            Self::OutputCurrent(port) => output_current_info(port),
            Self::DewCurrent(channel) => dew_current_info(channel),
            Self::OutputOvercurrent(port) => output_overcurrent_info(port),
            Self::DewOvercurrent(channel) => dew_overcurrent_info(channel),
            Self::AutoDewChannels => SwitchInfo {
                id: 34,
                name: "Auto-Dew Channels",
                description: "Which dew channels the device drives itself (0 = none, \
                              1 = all, 2-7 = individual combinations). Set it in the \
                              Pegasus Astro software; this driver only reports it",
                can_write: false,
                min_value: 0.0,
                max_value: 7.0,
                step: 1.0,
            },
            Self::AverageCurrent | Self::AmpHours | Self::WattHours | Self::Uptime => {
                counter_info(*self)
            }
        }
    }
}

/// Shared shape for the four 12 V outputs.
const fn output_info(port: OutputId) -> SwitchInfo {
    let (id, name, description) = match port {
        OutputId::One => (0, "12V Output 1", "Switches the 12V output on port 1"),
        OutputId::Two => (1, "12V Output 2", "Switches the 12V output on port 2"),
        OutputId::Three => (2, "12V Output 3", "Switches the 12V output on port 3"),
        OutputId::Four => (3, "12V Output 4", "Switches the 12V output on port 4"),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: true,
        min_value: 0.0,
        max_value: 1.0,
        step: 1.0,
    }
}

/// Shared shape for the three dew channels. `can_write` is the *static*
/// answer; a channel under auto-dew control reports `false` dynamically from
/// the switch device, which is where the live mask is known.
const fn dew_info(channel: DewChannel) -> SwitchInfo {
    let (id, name, description) = match channel {
        DewChannel::A => (
            4,
            "Dew Heater A",
            "PWM duty cycle for dew heater A (0-255). Read-only while auto-dew drives this channel",
        ),
        DewChannel::B => (
            5,
            "Dew Heater B",
            "PWM duty cycle for dew heater B (0-255). Read-only while auto-dew drives this channel",
        ),
        DewChannel::C => (
            6,
            "Dew Heater C",
            "PWM duty cycle for dew heater C (0-255). Read-only while auto-dew drives this channel",
        ),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: true,
        min_value: 0.0,
        max_value: 255.0,
        step: 1.0,
    }
}

/// Shared shape for the six USB ports.
const fn usb_info(port: UsbPortId) -> SwitchInfo {
    let (id, name, description) = match port {
        UsbPortId::One => (8, "USB Port 1", "Switches USB 3.0 port 1"),
        UsbPortId::Two => (9, "USB Port 2", "Switches USB 3.0 port 2"),
        UsbPortId::Three => (10, "USB Port 3", "Switches USB 3.0 port 3"),
        UsbPortId::Four => (11, "USB Port 4", "Switches USB 3.0 port 4"),
        UsbPortId::Five => (12, "USB Port 5", "Switches USB 2.0 port 5"),
        UsbPortId::Six => (13, "USB Port 6", "Switches USB 2.0 port 6"),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: true,
        min_value: 0.0,
        max_value: 1.0,
        step: 1.0,
    }
}

/// The six environmental and bulk-power readings, ids 14-19.
///
/// Only ever called with one of those six variants; any other is a
/// programming error in [`SwitchId::info`], and the fallback row exists to
/// keep the function total rather than to be reachable.
const fn sensor_info(switch: SwitchId) -> SwitchInfo {
    let (id, name, description, min_value, max_value, step) = match switch {
        SwitchId::InputVoltage => (
            14,
            "Input Voltage",
            "Input voltage in Volts",
            0.0,
            15.0,
            0.1,
        ),
        SwitchId::TotalCurrent => (
            15,
            "Total Current",
            "Total current draw in Amps",
            0.0,
            25.0,
            0.01,
        ),
        SwitchId::PowerDraw => (
            16,
            "Power Draw",
            "Total power draw in Watts",
            0.0,
            300.0,
            1.0,
        ),
        SwitchId::Temperature => (
            17,
            "Temperature",
            "Ambient temperature in Celsius",
            -40.0,
            60.0,
            0.1,
        ),
        SwitchId::Humidity => (
            18,
            "Humidity",
            "Relative humidity percentage",
            0.0,
            100.0,
            1.0,
        ),
        _ => (
            19,
            "Dewpoint",
            "Calculated dewpoint in Celsius",
            -40.0,
            60.0,
            0.1,
        ),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: false,
        min_value,
        max_value,
        step,
    }
}

/// The four cumulative power counters, ids 35-38.
///
/// Total for the same reason [`sensor_info`] is.
const fn counter_info(switch: SwitchId) -> SwitchInfo {
    let (id, name, description, max_value, step) = match switch {
        SwitchId::AverageCurrent => (
            35,
            "Average Current",
            "Average current draw in Amps",
            25.0,
            0.01,
        ),
        SwitchId::AmpHours => (
            36,
            "Amp Hours",
            "Cumulative amp-hours consumed",
            9999.0,
            0.01,
        ),
        SwitchId::WattHours => (
            37,
            "Watt Hours",
            "Cumulative watt-hours consumed",
            99999.0,
            0.1,
        ),
        _ => (38, "Uptime", "Device uptime in hours", 99999.0, 0.01),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: false,
        min_value: 0.0,
        max_value,
        step,
    }
}

/// Per-output current draw, ids 20-23.
const fn output_current_info(port: OutputId) -> SwitchInfo {
    let (id, name) = match port {
        OutputId::One => (20, "12V Output 1 Current"),
        OutputId::Two => (21, "12V Output 2 Current"),
        OutputId::Three => (22, "12V Output 3 Current"),
        OutputId::Four => (23, "12V Output 4 Current"),
    };
    SwitchInfo {
        id,
        name,
        description: "Current draw of this 12V output in Amps",
        can_write: false,
        min_value: 0.0,
        max_value: 10.0,
        step: 0.01,
    }
}

/// Per-dew-channel current draw, ids 24-26.
const fn dew_current_info(channel: DewChannel) -> SwitchInfo {
    let (id, name) = match channel {
        DewChannel::A => (24, "Dew Heater A Current"),
        DewChannel::B => (25, "Dew Heater B Current"),
        DewChannel::C => (26, "Dew Heater C Current"),
    };
    SwitchInfo {
        id,
        name,
        description: "Current draw of this dew channel in Amps",
        can_write: false,
        min_value: 0.0,
        max_value: 5.0,
        step: 0.01,
    }
}

/// Per-output overcurrent flag, ids 27-30.
const fn output_overcurrent_info(port: OutputId) -> SwitchInfo {
    let (id, name) = match port {
        OutputId::One => (27, "12V Output 1 Overcurrent"),
        OutputId::Two => (28, "12V Output 2 Overcurrent"),
        OutputId::Three => (29, "12V Output 3 Overcurrent"),
        OutputId::Four => (30, "12V Output 4 Overcurrent"),
    };
    SwitchInfo {
        id,
        name,
        description: "Overcurrent or short-circuit flag for this 12V output. \
                      The device shuts the port down when it trips",
        can_write: false,
        min_value: 0.0,
        max_value: 1.0,
        step: 1.0,
    }
}

/// Per-dew-channel overcurrent flag, ids 31-33.
const fn dew_overcurrent_info(channel: DewChannel) -> SwitchInfo {
    let (id, name) = match channel {
        DewChannel::A => (31, "Dew Heater A Overcurrent"),
        DewChannel::B => (32, "Dew Heater B Overcurrent"),
        DewChannel::C => (33, "Dew Heater C Overcurrent"),
    };
    SwitchInfo {
        id,
        name,
        description: "Overcurrent or short-circuit flag for this dew channel. \
                      The device shuts the channel down when it trips",
        can_write: false,
        min_value: 0.0,
        max_value: 1.0,
        step: 1.0,
    }
}

/// Information about a switch.
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
    use std::collections::HashSet;

    /// The last writable id. Ids 0-13 are settable; 14-38 are telemetry.
    const LAST_WRITABLE_ID: usize = 13;

    #[test]
    fn all_lists_exactly_max_switch_entries() {
        assert_eq!(SwitchId::all().len(), MAX_SWITCH);
    }

    #[test]
    fn all_ids_are_contiguous_from_zero() {
        let ids: Vec<usize> = SwitchId::all().iter().map(SwitchId::id).collect();
        assert_eq!(ids, (0..MAX_SWITCH).collect::<Vec<_>>());
    }

    #[test]
    fn every_id_in_range_resolves() {
        for id in 0..MAX_SWITCH {
            let switch = SwitchId::from_id(id)
                .unwrap_or_else(|| panic!("id {id} does not resolve to a switch"));
            assert_eq!(switch.id(), id, "id {id} round-trips to a different id");
        }
    }

    #[test]
    fn the_first_id_past_the_end_does_not_resolve() {
        assert!(SwitchId::from_id(MAX_SWITCH).is_none());
    }

    #[test]
    fn far_out_of_range_ids_do_not_resolve() {
        for id in [40, 100, 999, usize::MAX] {
            assert!(SwitchId::from_id(id).is_none(), "id {id} resolved");
        }
    }

    #[test]
    fn writable_switches_are_exactly_the_first_fourteen() {
        let writable: Vec<usize> = (0..MAX_SWITCH)
            .filter(|id| SwitchId::from_id(*id).unwrap().info().can_write)
            .collect();
        assert_eq!(writable, (0..=LAST_WRITABLE_ID).collect::<Vec<_>>());
    }

    #[test]
    fn every_switch_has_a_non_empty_name_and_description() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(!info.name.is_empty(), "switch {id} has no name");
            assert!(
                !info.description.is_empty(),
                "switch {id} has no description"
            );
        }
    }

    #[test]
    fn switch_names_are_unique() {
        let names: HashSet<&str> = SwitchId::all()
            .iter()
            .map(|switch| switch.info().name)
            .collect();
        assert_eq!(
            names.len(),
            MAX_SWITCH,
            "two switches share a name; ASCOM clients key on it"
        );
    }

    #[test]
    fn every_switch_has_a_usable_range() {
        for id in 0..MAX_SWITCH {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                info.min_value < info.max_value,
                "switch {id} has min >= max"
            );
            assert!(info.step > 0.0, "switch {id} has a non-positive step");
        }
    }

    #[test]
    fn the_four_outputs_occupy_ids_zero_through_three() {
        let ids: Vec<usize> = OutputId::ALL
            .iter()
            .map(|port| SwitchId::Output(*port).id())
            .collect();
        assert_eq!(ids, [0, 1, 2, 3]);
    }

    #[test]
    fn the_three_dew_channels_occupy_ids_four_through_six() {
        let ids: Vec<usize> = DewChannel::ALL
            .iter()
            .map(|ch| SwitchId::DewHeater(*ch).id())
            .collect();
        assert_eq!(ids, [4, 5, 6]);
    }

    #[test]
    fn the_variable_output_is_id_seven() {
        assert_eq!(SwitchId::VariableVoltage.id(), 7);
    }

    #[test]
    fn the_six_usb_ports_occupy_ids_eight_through_thirteen() {
        let ids: Vec<usize> = UsbPortId::ALL
            .iter()
            .map(|port| SwitchId::UsbPort(*port).id())
            .collect();
        assert_eq!(ids, [8, 9, 10, 11, 12, 13]);
    }

    #[test]
    fn auto_dew_channels_is_read_only() {
        let info = SwitchId::AutoDewChannels.info();
        assert!(
            !info.can_write,
            "this driver never writes PD:; the switch reports only"
        );
        assert_eq!(info.max_value, 7.0);
    }

    #[test]
    fn the_variable_output_publishes_the_three_to_twelve_volt_range() {
        let info = SwitchId::VariableVoltage.info();
        assert_eq!(info.min_value, f64::from(VARIABLE_VOLTS_MIN));
        assert_eq!(info.max_value, f64::from(VARIABLE_VOLTS_MAX));
    }

    #[test]
    fn dew_heaters_publish_the_full_pwm_range() {
        for ch in DewChannel::ALL {
            let info = SwitchId::DewHeater(ch).info();
            assert_eq!(info.min_value, 0.0);
            assert_eq!(info.max_value, 255.0);
        }
    }

    #[test]
    fn overcurrent_flags_cover_four_outputs_then_three_dew_channels() {
        let output_ids: Vec<usize> = OutputId::ALL
            .iter()
            .map(|port| SwitchId::OutputOvercurrent(*port).id())
            .collect();
        let dew_ids: Vec<usize> = DewChannel::ALL
            .iter()
            .map(|ch| SwitchId::DewOvercurrent(*ch).id())
            .collect();
        assert_eq!(output_ids, [27, 28, 29, 30]);
        assert_eq!(dew_ids, [31, 32, 33]);
    }

    #[test]
    fn the_power_counters_are_the_last_four_ids() {
        assert_eq!(SwitchId::AverageCurrent.id(), 35);
        assert_eq!(SwitchId::AmpHours.id(), 36);
        assert_eq!(SwitchId::WattHours.id(), 37);
        assert_eq!(SwitchId::Uptime.id(), MAX_SWITCH - 1);
    }
}

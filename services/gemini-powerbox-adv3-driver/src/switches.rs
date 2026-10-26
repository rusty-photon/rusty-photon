//! Switch definitions for the Advanced 3.
//!
//! This module defines every switch exposed through the ASCOM Switch
//! interface. Switches are numbered from 0 to [`MAX_SWITCH`] - 1; the table
//! is the one in `docs/services/gemini-powerbox-adv3-driver.md`, ordered the
//! way the box's panel is: DC outputs, USB ports, dew channels, telemetry.
//!
//! [`SwitchId`] carries the connector with the variant, which keeps `match`
//! exhaustive at every use site: a new family member is a compile error
//! rather than a silently missing row. Ids are written as literals in both
//! [`SwitchId::from_id`] and [`SwitchId::info`]; `every_id_in_range_resolves`
//! round-trips every id through both, so a drifted row fails the build.

use rusty_photon_server_config::switch_labels::{SwitchLabels, SwitchTable};

use crate::protocol::{DcOutput, DewChannel, UsbPort, DEW_DUTY_MAX};

/// Total number of switches exposed by the Switch device.
pub const MAX_SWITCH: usize = 26;

/// The value the three temperature rows publish while their sensor is
/// absent: the DS18B20 library convention for a disconnected probe, below
/// either sensor's range, and the rows' declared minimum.
pub const TEMPERATURE_SENTINEL: f64 = -127.0;

/// The value the humidity row publishes while the AHT20 is absent: the
/// sensor's floor, and the row's declared minimum.
pub const HUMIDITY_SENTINEL: f64 = 0.0;

/// The duty row's ceiling publishes as an `f64` literal because a `u8`→`f64`
/// conversion is not available in a `const fn`. This keeps the two from
/// drifting apart.
const _: () = assert!(DEW_DUTY_MAX == 100);

/// Switch identifiers for the Advanced 3.
///
/// The ASCOM switch id is [`SwitchInfo::id`], not the variant's discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchId {
    // ---- Connectors ----
    /// The always-on 12 V output. Read-only, always 1; listed so the panel
    /// is complete and the connector can be labelled.
    Dc1,
    /// One of the four switched 12 V outputs.
    Dc(DcOutput),
    /// One of the six switchable USB ports.
    Usb(UsbPort),
    /// A dew channel's manual duty, 0-100 %. Writable in Manual mode only.
    DewDuty(DewChannel),
    /// A dew channel's enabled flag. Writable in Auto and Switch modes only.
    DewEnabled(DewChannel),
    /// A dew channel's mode: 0 Auto, 1 Manual, 2 Switch.
    DewMode(DewChannel),

    // ---- Read-only telemetry ----
    /// Input voltage in Volts.
    InputVoltage,
    /// Total output current in Amps.
    OutputCurrent,
    /// Total output power in Watts.
    OutputPower,
    /// Ambient temperature in Celsius, from the AHT20.
    Temperature,
    /// Relative humidity in percent, from the AHT20.
    Humidity,
    /// Dew point in Celsius, computed by the hub.
    Dewpoint,
    /// Lens temperature in Celsius, from the DS18B20 probe.
    LensTemperature,
    /// Whether the AHT20 is attached.
    AmbientSensorAttached,
    /// Whether the DS18B20 is attached.
    LensProbeAttached,
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
            0 => Self::Dc1,
            1 => Self::Dc(DcOutput::Dc2),
            2 => Self::Dc(DcOutput::Dc3),
            3 => Self::Dc(DcOutput::Dc4),
            4 => Self::Dc(DcOutput::Dc5),
            5 => Self::Usb(UsbPort::A),
            6 => Self::Usb(UsbPort::B),
            7 => Self::Usb(UsbPort::C),
            8 => Self::Usb(UsbPort::D),
            9 => Self::Usb(UsbPort::E),
            10 => Self::Usb(UsbPort::F),
            11 => Self::DewDuty(DewChannel::Dew6),
            12 => Self::DewDuty(DewChannel::Dew7),
            13 => Self::DewEnabled(DewChannel::Dew6),
            14 => Self::DewEnabled(DewChannel::Dew7),
            15 => Self::DewMode(DewChannel::Dew6),
            16 => Self::DewMode(DewChannel::Dew7),
            17 => Self::InputVoltage,
            18 => Self::OutputCurrent,
            19 => Self::OutputPower,
            20 => Self::Temperature,
            21 => Self::Humidity,
            22 => Self::Dewpoint,
            23 => Self::LensTemperature,
            24 => Self::AmbientSensorAttached,
            25 => Self::LensProbeAttached,
            _ => return None,
        })
    }

    /// The numeric ASCOM id for this switch.
    #[must_use]
    pub const fn id(&self) -> usize {
        self.info().id
    }

    /// The switch metadata: id, name, description, static writability and
    /// range. A dew row's `can_write` is the *static* answer; whether the
    /// channel's mode makes it writable right now is the switch device's to
    /// say, where the live frame is known.
    #[must_use]
    pub const fn info(&self) -> SwitchInfo {
        match *self {
            Self::Dc1 => SwitchInfo {
                id: 0,
                name: "DC1",
                description: "The always-on 12 V output DC1, behind an ideal diode. \
                              Read-only: it is on whenever the hub is powered",
                can_write: false,
                min_value: 0.0,
                max_value: 1.0,
                step: 1.0,
            },
            Self::Dc(output) => dc_info(output),
            Self::Usb(port) => usb_info(port),
            Self::DewDuty(channel) => dew_duty_info(channel),
            Self::DewEnabled(channel) => dew_enabled_info(channel),
            Self::DewMode(channel) => dew_mode_info(channel),
            Self::InputVoltage
            | Self::OutputCurrent
            | Self::OutputPower
            | Self::Temperature
            | Self::Humidity
            | Self::Dewpoint
            | Self::LensTemperature
            | Self::AmbientSensorAttached
            | Self::LensProbeAttached => telemetry_info(*self),
        }
    }

    /// The connector an operator labels to rename this switch, and the
    /// suffix that survives the label.
    ///
    /// The three dew rows follow their connector: labelling `DEW6` as
    /// `Guide scope heater` renames `DEW6 Duty`, `DEW6 Enabled` and
    /// `DEW6 Mode` together. DC1 is read-only but a connector, so it takes a
    /// label. The telemetry and sensor rows keep their names: they report
    /// physical quantities a client has to be able to interpret.
    #[must_use]
    pub const fn label_key(&self) -> (&'static str, &'static str) {
        match *self {
            Self::Dc1 => ("DC1", ""),
            Self::Dc(output) => (output.name(), ""),
            Self::Usb(port) => (port.name(), ""),
            Self::DewDuty(channel) => (channel.name(), " Duty"),
            Self::DewEnabled(channel) => (channel.name(), " Enabled"),
            Self::DewMode(channel) => (channel.name(), " Mode"),
            Self::InputVoltage
            | Self::OutputCurrent
            | Self::OutputPower
            | Self::Temperature
            | Self::Humidity
            | Self::Dewpoint
            | Self::LensTemperature
            | Self::AmbientSensorAttached
            | Self::LensProbeAttached => (self.info().name, ""),
        }
    }

    /// This switch's published name under `labels`: the operator's label for
    /// its connector where there is one, otherwise the built-in name.
    #[must_use]
    pub fn effective_name(&self, labels: &GeminiSwitchLabels) -> String {
        let (key, suffix) = self.label_key();
        format!("{}{suffix}", labels.resolve(key))
    }
}

/// Shared shape for the four switched 12 V outputs.
const fn dc_info(output: DcOutput) -> SwitchInfo {
    let (id, name, description) = match output {
        DcOutput::Dc2 => (1, "DC2", "Switches the 12 V output DC2"),
        DcOutput::Dc3 => (2, "DC3", "Switches the 12 V output DC3"),
        DcOutput::Dc4 => (3, "DC4", "Switches the 12 V output DC4"),
        DcOutput::Dc5 => (4, "DC5", "Switches the 12 V output DC5"),
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

/// Shared shape for the six USB ports.
const fn usb_info(port: UsbPort) -> SwitchInfo {
    let (id, name, description) = match port {
        UsbPort::A => (5, "USB A", "Switches USB port A (USB 3.2 Gen1)"),
        UsbPort::B => (6, "USB B", "Switches USB port B (USB 3.2 Gen1)"),
        UsbPort::C => (7, "USB C", "Switches USB port C (USB 2.0)"),
        UsbPort::D => (8, "USB D", "Switches USB port D (USB 2.0)"),
        UsbPort::E => (9, "USB E", "Switches USB port E (USB 2.0)"),
        UsbPort::F => (10, "USB F", "Switches USB port F (USB 2.0)"),
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

/// The manual duty of one dew channel, 0-100 %.
const fn dew_duty_info(channel: DewChannel) -> SwitchInfo {
    let (id, name, description) = match channel {
        DewChannel::Dew6 => (
            11,
            "DEW6 Duty",
            "Manual duty of dew heater DEW6 in percent. Writable in Manual mode only; \
             in Auto and Switch modes it reports what the hub reports",
        ),
        DewChannel::Dew7 => (
            12,
            "DEW7 Duty",
            "Manual duty of dew heater DEW7 in percent. Writable in Manual mode only; \
             in Auto and Switch modes it reports what the hub reports",
        ),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: true,
        min_value: 0.0,
        max_value: 100.0,
        step: 1.0,
    }
}

/// The enabled flag of one dew channel.
const fn dew_enabled_info(channel: DewChannel) -> SwitchInfo {
    let (id, name, description) = match channel {
        DewChannel::Dew6 => (
            13,
            "DEW6 Enabled",
            "Whether dew heater DEW6 is enabled: in Auto mode 1 runs the PID loop, in \
             Switch mode 1 is plain DC on. Writable in Auto and Switch modes only",
        ),
        DewChannel::Dew7 => (
            14,
            "DEW7 Enabled",
            "Whether dew heater DEW7 is enabled: in Auto mode 1 runs the PID loop, in \
             Switch mode 1 is plain DC on. Writable in Auto and Switch modes only",
        ),
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

/// The mode of one dew channel.
const fn dew_mode_info(channel: DewChannel) -> SwitchInfo {
    let (id, name, description) = match channel {
        DewChannel::Dew6 => (
            15,
            "DEW6 Mode",
            "Dew heater DEW6 mode: 0 Auto (PID on the two sensors), 1 Manual (the duty), \
             2 Switch (plain DC). Auto needs both sensors attached",
        ),
        DewChannel::Dew7 => (
            16,
            "DEW7 Mode",
            "Dew heater DEW7 mode: 0 Auto (PID on the two sensors), 1 Manual (the duty), \
             2 Switch (plain DC). Auto needs both sensors attached",
        ),
    };
    SwitchInfo {
        id,
        name,
        description,
        can_write: true,
        min_value: 0.0,
        max_value: 2.0,
        step: 1.0,
    }
}

/// The nine read-only rows, ids 17-25.
///
/// Only ever called with one of those nine variants; any other is a
/// programming error in [`SwitchId::info`], and the fallback row exists to
/// keep the function total rather than to be reachable.
const fn telemetry_info(switch: SwitchId) -> SwitchInfo {
    let (id, name, description, min_value, max_value, step) = match switch {
        SwitchId::InputVoltage => (
            17,
            "Input Voltage",
            "Input voltage in Volts",
            0.0,
            19.0,
            0.1,
        ),
        SwitchId::OutputCurrent => (
            18,
            "Output Current",
            "Total output current in Amps",
            0.0,
            15.0,
            0.01,
        ),
        SwitchId::OutputPower => (
            19,
            "Output Power",
            "Total output power in Watts",
            0.0,
            180.0,
            0.01,
        ),
        SwitchId::Temperature => (
            20,
            "Temperature",
            "Ambient temperature from the AHT20 in Celsius; -127 while the sensor is absent \
             (see Ambient Sensor Attached)",
            -127.0,
            85.0,
            0.01,
        ),
        SwitchId::Humidity => (
            21,
            "Humidity",
            "Relative humidity from the AHT20 in percent; 0 while the sensor is absent \
             (see Ambient Sensor Attached)",
            0.0,
            100.0,
            0.01,
        ),
        SwitchId::Dewpoint => (
            22,
            "Dewpoint",
            "Dew point the hub computes from the AHT20 in Celsius; -127 while the sensor \
             is absent (see Ambient Sensor Attached)",
            -127.0,
            85.0,
            0.01,
        ),
        SwitchId::LensTemperature => (
            23,
            "Lens Temperature",
            "Lens temperature from the DS18B20 probe in Celsius; -127 while the probe is \
             absent (see Lens Probe Attached)",
            -127.0,
            125.0,
            0.01,
        ),
        SwitchId::AmbientSensorAttached => (
            24,
            "Ambient Sensor Attached",
            "Whether the AHT20 ambient temperature and humidity sensor is attached to the \
             Temp port",
            0.0,
            1.0,
            1.0,
        ),
        _ => (
            25,
            "Lens Probe Attached",
            "Whether the DS18B20 lens temperature probe is attached to the Dew Temp port",
            0.0,
            1.0,
            1.0,
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

/// The Advanced 3's switch table, as [`SwitchLabels`] checks an operator's
/// label map against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiSwitchTable;

/// The Advanced 3's operator label map: `switch.labels` in the config file.
pub type GeminiSwitchLabels = SwitchLabels<GeminiSwitchTable>;

/// The connectors an operator may label, in the order of their first row.
/// `DEW6` and `DEW7` are connectors, not rows: a label on one renames all
/// three of its rows.
const LABELLABLE: [&str; 13] = [
    "DC1", "DC2", "DC3", "DC4", "DC5", "USB A", "USB B", "USB C", "USB D", "USB E", "USB F",
    "DEW6", "DEW7",
];

impl SwitchTable for GeminiSwitchTable {
    fn labellable() -> &'static [&'static str] {
        &LABELLABLE
    }

    fn effective_names(labels: &GeminiSwitchLabels) -> Vec<String> {
        SwitchId::all()
            .iter()
            .map(|switch| switch.effective_name(labels))
            .collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rusty_photon_server_config::switch_labels::SwitchLabelError;
    use std::collections::HashSet;

    /// The last id with a command behind it. Ids 1-16 are settable; 0 and
    /// 17-25 are read-only.
    const LAST_WRITABLE_ID: usize = 16;

    #[test]
    fn all_lists_exactly_max_switch_entries() {
        assert_eq!(SwitchId::all().len(), MAX_SWITCH);
    }

    #[test]
    fn every_id_in_range_resolves_and_round_trips() {
        for id in 0..MAX_SWITCH {
            let switch = SwitchId::from_id(id)
                .unwrap_or_else(|| panic!("id {id} does not resolve to a switch"));
            assert_eq!(switch.id(), id, "id {id} round-trips to a different id");
        }
    }

    #[test]
    fn ids_past_the_end_do_not_resolve() {
        for id in [MAX_SWITCH, 27, 100, usize::MAX] {
            assert!(SwitchId::from_id(id).is_none(), "id {id} resolved");
        }
    }

    #[test]
    fn writable_switches_are_one_through_sixteen_and_dc1_is_not() {
        let writable: Vec<usize> = (0..MAX_SWITCH)
            .filter(|id| SwitchId::from_id(*id).unwrap().info().can_write)
            .collect();
        assert_eq!(writable, (1..=LAST_WRITABLE_ID).collect::<Vec<_>>());
        assert!(!SwitchId::Dc1.info().can_write);
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
    fn the_table_is_in_panel_order() {
        assert_eq!(SwitchId::Dc1.id(), 0);
        assert_eq!(DcOutput::ALL.map(|o| SwitchId::Dc(o).id()), [1, 2, 3, 4]);
        assert_eq!(
            UsbPort::ALL.map(|p| SwitchId::Usb(p).id()),
            [5, 6, 7, 8, 9, 10]
        );
        assert_eq!(DewChannel::ALL.map(|c| SwitchId::DewDuty(c).id()), [11, 12]);
        assert_eq!(
            DewChannel::ALL.map(|c| SwitchId::DewEnabled(c).id()),
            [13, 14]
        );
        assert_eq!(DewChannel::ALL.map(|c| SwitchId::DewMode(c).id()), [15, 16]);
        assert_eq!(SwitchId::LensProbeAttached.id(), MAX_SWITCH - 1);
    }

    #[test]
    fn the_ranges_follow_the_design_table() {
        let table = [
            (11_usize, 0.0, 100.0, 1.0),
            (15, 0.0, 2.0, 1.0),
            (17, 0.0, 19.0, 0.1),
            (18, 0.0, 15.0, 0.01),
            (19, 0.0, 180.0, 0.01),
            (20, -127.0, 85.0, 0.01),
            (21, 0.0, 100.0, 0.01),
            (22, -127.0, 85.0, 0.01),
            (23, -127.0, 125.0, 0.01),
        ];
        for (id, min, max, step) in table {
            let info = SwitchId::from_id(id).unwrap().info();
            assert_eq!(info.min_value, min, "switch {id} min");
            assert_eq!(info.max_value, max, "switch {id} max");
            assert_eq!(info.step, step, "switch {id} step");
        }
    }

    #[test]
    fn the_sentinels_sit_inside_their_rows_declared_ranges() {
        // A client that checks a reading against MinSwitchValue must not
        // reject the sentinel.
        for id in [20, 22, 23] {
            let info = SwitchId::from_id(id).unwrap().info();
            assert!(
                (info.min_value..=info.max_value).contains(&TEMPERATURE_SENTINEL),
                "switch {id}"
            );
        }
        let humidity = SwitchId::Humidity.info();
        assert!((humidity.min_value..=humidity.max_value).contains(&HUMIDITY_SENTINEL));
    }

    /// Build a label map, keeping the rules' verdict.
    fn try_labels(pairs: &[(&str, &str)]) -> Result<GeminiSwitchLabels, SwitchLabelError> {
        GeminiSwitchLabels::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// Build a label map, panicking on a map the rules reject.
    fn labels(pairs: &[(&str, &str)]) -> GeminiSwitchLabels {
        try_labels(pairs).unwrap()
    }

    #[test]
    fn the_labellable_names_are_the_thirteen_connectors() {
        assert_eq!(
            GeminiSwitchTable::labellable(),
            [
                "DC1", "DC2", "DC3", "DC4", "DC5", "USB A", "USB B", "USB C", "USB D", "USB E",
                "USB F", "DEW6", "DEW7"
            ]
        );
    }

    #[test]
    fn no_telemetry_switch_can_be_labelled() {
        for id in 17..MAX_SWITCH {
            let name = SwitchId::from_id(id).unwrap().info().name;
            assert!(
                !GeminiSwitchTable::labellable().contains(&name),
                "switch {id} ({name}) must keep its published name"
            );
            assert!(try_labels(&[(name, "Sky")]).is_err(), "{name} took a label");
        }
    }

    #[test]
    fn an_empty_label_map_publishes_the_built_in_names() {
        let empty = GeminiSwitchLabels::default();
        let published: Vec<String> = SwitchId::all()
            .iter()
            .map(|switch| switch.info().name.to_string())
            .collect();
        assert_eq!(GeminiSwitchTable::effective_names(&empty), published);
    }

    #[test]
    fn labelling_a_dew_connector_renames_its_three_rows() {
        let labels = labels(&[("DEW6", "Guide scope heater")]);
        assert_eq!(
            SwitchId::from_id(11).unwrap().effective_name(&labels),
            "Guide scope heater Duty"
        );
        assert_eq!(
            SwitchId::from_id(13).unwrap().effective_name(&labels),
            "Guide scope heater Enabled"
        );
        assert_eq!(
            SwitchId::from_id(15).unwrap().effective_name(&labels),
            "Guide scope heater Mode"
        );
        // DEW7 is untouched.
        assert_eq!(
            SwitchId::from_id(12).unwrap().effective_name(&labels),
            "DEW7 Duty"
        );
    }

    #[test]
    fn labelling_a_dc_output_or_usb_port_renames_only_that_row() {
        let labels = labels(&[("DC2", "Mount"), ("USB A", "Imaging camera")]);
        assert_eq!(
            SwitchId::from_id(1).unwrap().effective_name(&labels),
            "Mount"
        );
        assert_eq!(SwitchId::from_id(2).unwrap().effective_name(&labels), "DC3");
        assert_eq!(
            SwitchId::from_id(5).unwrap().effective_name(&labels),
            "Imaging camera"
        );
        assert_eq!(
            SwitchId::from_id(6).unwrap().effective_name(&labels),
            "USB B"
        );
    }

    #[test]
    fn the_read_only_dc1_takes_a_label() {
        let labels = labels(&[("DC1", "Always on: hub fan")]);
        assert_eq!(SwitchId::Dc1.effective_name(&labels), "Always on: hub fan");
    }

    #[test]
    fn a_label_that_collides_with_another_row_is_rejected() {
        // Nothing in the map mentions "DEW7 Duty"; the collision is with a
        // row that follows an untouched connector.
        let err = try_labels(&[("DEW6", "DEW7")]).unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::DuplicateName {
                name: "DEW7 Duty".to_string(),
            }
        );
        let err = try_labels(&[("DC2", "Temperature")]).unwrap_err();
        assert!(
            matches!(err, SwitchLabelError::DuplicateName { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_key_that_names_a_row_rather_than_a_connector_is_rejected() {
        let err = try_labels(&[("DEW6 Duty", "Heater")]).unwrap_err();
        assert!(err.to_string().contains("DEW6 Duty"), "{err}");
    }
}

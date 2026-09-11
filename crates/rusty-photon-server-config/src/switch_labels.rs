//! Operator labels for ASCOM switch names.
//!
//! A Pegasus powerbox names its ports by number — `12V Output 1`, `USB Port
//! 5`. *What is plugged into port 1* is a fact the operator knows and the
//! driver cannot learn, so without somewhere to record it every Alpaca client
//! shows the port number and nothing else. [`SwitchLabels`] is that place: a
//! map in the service config that replaces the built-in name of a switch
//! corresponding to a connector on the box.
//!
//! The map is keyed by the **built-in name**, not the switch id. Ids are the
//! ASCOM addressing scheme and never move, but a config file keyed by number
//! cannot be read without the id table open, and a mistyped id silently names
//! a different switch. A mistyped *name* names no switch at all, which is why
//! every rule below is enforced in [`Deserialize`] rather than by a later
//! validation pass: a bad map fails the config load, naming the entry.
//!
//! Rules, in the order [`SwitchLabels::new`] checks them:
//!
//! 1. Every key names a switch in [`SwitchTable::labellable`].
//! 2. Every label has non-whitespace content. Removing an entry is how a
//!    switch goes back to its built-in name; an empty string would otherwise
//!    be a second, silent way to spell it.
//! 3. The resulting names are unique. ASCOM clients key on the switch name,
//!    and [`SwitchTable::effective_names`] reports every name the table would
//!    publish — including the telemetry rows a label governs — so a collision
//!    with another label *or* with an unlabelled switch is caught here.
//!
//! Each driver parameterises the type with its own [`SwitchTable`], so the
//! two Pegasus drivers share one implementation of the rules while keeping
//! their own tables.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The switch table a [`SwitchLabels`] map is checked against.
///
/// Implemented on a zero-sized marker type next to the driver's switch table,
/// which is the only place that knows which ids are connectors and which
/// telemetry rows follow them.
pub trait SwitchTable: Sized {
    /// The built-in names an operator may relabel, in ascending switch id
    /// order: the switches that correspond to a connector on the box.
    ///
    /// Read-only rows report physical quantities, so they keep their names —
    /// a renamed `Humidity` leaves a client no way to know what it is
    /// reading.
    fn labellable() -> &'static [&'static str];

    /// Every switch's name under `labels`, in ascending id order.
    ///
    /// One label may govern more than one name: on a box with per-port
    /// telemetry, labelling an output also renames its current reading and
    /// its overcurrent flag. The result is what [`SwitchLabels::new`] checks
    /// for duplicates, so those derived names take part in the uniqueness
    /// rule too.
    fn effective_names(labels: &SwitchLabels<Self>) -> Vec<String>;
}

/// Operator labels for a driver's switch table, keyed by built-in name.
///
/// Construct it through [`Self::new`] or by deserializing a JSON object; both
/// enforce the module's three rules. An empty map is the default and leaves
/// every switch with its built-in name.
pub struct SwitchLabels<T: SwitchTable> {
    by_name: BTreeMap<String, String>,
    /// `fn() -> T` rather than `T`: the marker is never held, so this keeps
    /// the type `Send`/`Sync` and covariant regardless of `T`.
    table: PhantomData<fn() -> T>,
}

impl<T: SwitchTable> SwitchLabels<T> {
    /// Check `labels` against `T`'s table and build the map.
    ///
    /// Labels are stored trimmed, so a value that differs only in surrounding
    /// whitespace persists and compares as one label.
    ///
    /// # Errors
    ///
    /// Returns the first rule the map breaks: a key naming no labellable
    /// switch, a label with no non-whitespace content, or a name two switches
    /// would share.
    pub fn new(labels: BTreeMap<String, String>) -> Result<Self, SwitchLabelError> {
        let mut by_name = BTreeMap::new();
        for (switch, label) in labels {
            if !T::labellable().contains(&switch.as_str()) {
                return Err(SwitchLabelError::UnknownSwitch {
                    switch,
                    labellable: T::labellable().join(", "),
                });
            }
            let label = label.trim();
            if label.is_empty() {
                return Err(SwitchLabelError::EmptyLabel { switch });
            }
            by_name.insert(switch, label.to_string());
        }

        let candidate = Self {
            by_name,
            table: PhantomData,
        };
        let mut seen = BTreeSet::new();
        for name in T::effective_names(&candidate) {
            if !seen.insert(name.clone()) {
                return Err(SwitchLabelError::DuplicateName { name });
            }
        }
        Ok(candidate)
    }

    /// Whether no switch carries a label.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// How many switches carry a label.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// The label for `built_in`, if the operator set one.
    #[must_use]
    pub fn get(&self, built_in: &str) -> Option<&str> {
        self.by_name.get(built_in).map(String::as_str)
    }

    /// The label for `built_in`, or `built_in` itself when unlabelled.
    #[must_use]
    pub fn resolve<'a>(&'a self, built_in: &'a str) -> &'a str {
        self.get(built_in).unwrap_or(built_in)
    }

    /// Every `(built-in name, label)` pair, in built-in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_name
            .iter()
            .map(|(switch, label)| (switch.as_str(), label.as_str()))
    }
}

/// Why a label map was rejected.
///
/// Each variant names the offending entry, because the operator's next step
/// is to find it in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchLabelError {
    /// A key that names no switch an operator may relabel.
    UnknownSwitch {
        /// The key as written in the config.
        switch: String,
        /// The names that would have been accepted, comma-separated.
        labellable: String,
    },
    /// A label with no non-whitespace content.
    EmptyLabel {
        /// The switch whose label was blank.
        switch: String,
    },
    /// A name two switches would both publish.
    DuplicateName {
        /// The name they would share.
        name: String,
    },
}

impl fmt::Display for SwitchLabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSwitch { switch, labellable } => write!(
                f,
                "\"{switch}\" is not a switch that can be labelled; \
                 label one of: {labellable}"
            ),
            Self::EmptyLabel { switch } => write!(
                f,
                "the label for \"{switch}\" is blank; \
                 remove the entry to keep the built-in name"
            ),
            Self::DuplicateName { name } => write!(
                f,
                "two switches would both be named \"{name}\"; \
                 ASCOM clients key on the name, so every switch needs its own"
            ),
        }
    }
}

impl std::error::Error for SwitchLabelError {}

// `T` is a marker the map never holds, so these impls carry no bound on it —
// which `derive` would add and which would make `SwitchLabels` un-cloneable
// for a zero-sized table type that derives nothing.
impl<T: SwitchTable> fmt::Debug for SwitchLabels<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SwitchLabels").field(&self.by_name).finish()
    }
}

impl<T: SwitchTable> Clone for SwitchLabels<T> {
    fn clone(&self) -> Self {
        Self {
            by_name: self.by_name.clone(),
            table: PhantomData,
        }
    }
}

impl<T: SwitchTable> Default for SwitchLabels<T> {
    fn default() -> Self {
        Self {
            by_name: BTreeMap::new(),
            table: PhantomData,
        }
    }
}

impl<T: SwitchTable> PartialEq for SwitchLabels<T> {
    fn eq(&self, other: &Self) -> bool {
        self.by_name == other.by_name
    }
}

impl<T: SwitchTable> Eq for SwitchLabels<T> {}

impl<T: SwitchTable> Serialize for SwitchLabels<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.by_name.serialize(serializer)
    }
}

impl<'de, T: SwitchTable> Deserialize<'de> for SwitchLabels<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let by_name = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::new(by_name).map_err(serde::de::Error::custom)
    }
}

/// A plain `{built-in name: label}` object. The labellable names are not
/// enumerated in the schema: the rule is the driver's table, not the shape,
/// and a schema that listed them would go stale the moment the table grew.
impl<T: SwitchTable> schemars::JsonSchema for SwitchLabels<T> {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SwitchLabels")
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": { "type": "string" },
            "description": "Operator labels for switches that correspond to a connector, \
                            keyed by the switch's built-in name",
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Two connectors and one telemetry row that follows the first of them,
    /// which is the shape both Pegasus drivers' tables reduce to here.
    struct TestTable;

    const PORT_ONE: &str = "Output 1";
    const PORT_TWO: &str = "Output 2";
    const SENSOR: &str = "Temperature";

    impl SwitchTable for TestTable {
        fn labellable() -> &'static [&'static str] {
            &[PORT_ONE, PORT_TWO]
        }

        fn effective_names(labels: &SwitchLabels<Self>) -> Vec<String> {
            vec![
                labels.resolve(PORT_ONE).to_string(),
                labels.resolve(PORT_TWO).to_string(),
                format!("{} Current", labels.resolve(PORT_ONE)),
                SENSOR.to_string(),
            ]
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn parse(json: &str) -> Result<SwitchLabels<TestTable>, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn the_default_map_is_empty() {
        let labels = SwitchLabels::<TestTable>::default();
        assert!(labels.is_empty());
        assert_eq!(labels.len(), 0);
    }

    #[test]
    fn an_unlabelled_switch_resolves_to_its_built_in_name() {
        let labels = SwitchLabels::<TestTable>::default();
        assert_eq!(labels.resolve(PORT_ONE), PORT_ONE);
        assert_eq!(labels.get(PORT_ONE), None);
    }

    #[test]
    fn a_label_replaces_the_built_in_name() {
        let labels = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "QHY600")])).unwrap();
        assert_eq!(labels.resolve(PORT_ONE), "QHY600");
        assert_eq!(labels.get(PORT_ONE), Some("QHY600"));
        assert_eq!(labels.resolve(PORT_TWO), PORT_TWO);
    }

    #[test]
    fn a_label_is_stored_trimmed() {
        let labels = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "  QHY600 ")])).unwrap();
        assert_eq!(labels.resolve(PORT_ONE), "QHY600");
    }

    #[test]
    fn a_key_that_names_no_labellable_switch_is_rejected() {
        let err = SwitchLabels::<TestTable>::new(labels(&[(SENSOR, "Sky")])).unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::UnknownSwitch {
                switch: SENSOR.to_string(),
                labellable: "Output 1, Output 2".to_string(),
            }
        );
    }

    #[test]
    fn a_blank_label_is_rejected() {
        let err = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "   ")])).unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::EmptyLabel {
                switch: PORT_ONE.to_string(),
            }
        );
    }

    #[test]
    fn two_switches_labelled_the_same_are_rejected() {
        let err = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "Cam"), (PORT_TWO, "Cam")]))
            .unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::DuplicateName {
                name: "Cam".to_string(),
            }
        );
    }

    #[test]
    fn a_label_colliding_with_an_unlabelled_switch_is_rejected() {
        let err = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, PORT_TWO)])).unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::DuplicateName {
                name: PORT_TWO.to_string(),
            }
        );
    }

    #[test]
    fn a_label_colliding_with_a_derived_telemetry_name_is_rejected() {
        // Labelling port 2 as "Output 1 Current" collides with the row that
        // follows port 1 — a name no key in the map mentions.
        let err =
            SwitchLabels::<TestTable>::new(labels(&[(PORT_TWO, "Output 1 Current")])).unwrap_err();
        assert_eq!(
            err,
            SwitchLabelError::DuplicateName {
                name: "Output 1 Current".to_string(),
            }
        );
    }

    #[test]
    fn iter_yields_every_pair_in_built_in_name_order() {
        let labels =
            SwitchLabels::<TestTable>::new(labels(&[(PORT_TWO, "Flat"), (PORT_ONE, "Cam")]))
                .unwrap();
        let pairs: Vec<(&str, &str)> = labels.iter().collect();
        assert_eq!(pairs, [(PORT_ONE, "Cam"), (PORT_TWO, "Flat")]);
    }

    #[test]
    fn an_empty_object_deserializes_to_the_default() {
        assert_eq!(parse("{}").unwrap(), SwitchLabels::<TestTable>::default());
    }

    #[test]
    fn deserialize_enforces_the_rules_and_names_the_offender() {
        let err = parse(r#"{"Output 9": "QHY600"}"#).unwrap_err();
        assert!(
            err.to_string().contains("\"Output 9\" is not a switch"),
            "{err}"
        );
    }

    #[test]
    fn a_map_round_trips_through_json() {
        let labels = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "QHY600")])).unwrap();
        let json = serde_json::to_string(&labels).unwrap();
        assert_eq!(json, r#"{"Output 1":"QHY600"}"#);
        assert_eq!(parse(&json).unwrap(), labels);
    }

    #[test]
    fn the_schema_is_an_object_of_strings() {
        let schema = schemars::schema_for!(SwitchLabels<TestTable>);
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(value["type"], "object");
        assert_eq!(value["additionalProperties"]["type"], "string");
    }

    #[test]
    fn debug_shows_the_pairs() {
        let labels = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "QHY600")])).unwrap();
        assert!(format!("{labels:?}").contains("QHY600"));
    }

    #[test]
    fn clone_preserves_the_pairs() {
        let labels = SwitchLabels::<TestTable>::new(labels(&[(PORT_ONE, "QHY600")])).unwrap();
        assert_eq!(labels.clone(), labels);
    }
}

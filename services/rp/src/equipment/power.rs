//! The power map at run time (rp.md § Power Switching).
//!
//! A `switches[]` entry's `outputs` name each output as the box's driver
//! reports it (`GetSwitchName`), so an output is found on the box by
//! reading the names of its switches, and the refusals `set_switch`
//! makes come from the map and the registry's hold-outs alone. The MCP
//! tools in `mcp/built_in/power.rs` put the two together.

use ascom_alpaca::api::Switch;

use crate::config::{OutputConfig, SwitchConfig};

/// One switch of a power box, as `get_switch` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputReading {
    pub name: String,
    pub index: usize,
    pub can_write: bool,
    pub on: bool,
    pub value: f64,
}

/// The names of every switch the box reports, by switch id.
///
/// # Errors
///
/// The driver's error, as text, when `MaxSwitch` or a name does not
/// read.
pub async fn switch_names(device: &dyn Switch) -> Result<Vec<String>, String> {
    let count = device
        .max_switch()
        .await
        .map_err(|e| format!("could not read MaxSwitch: {e}"))?;
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        names.push(
            device
                .get_switch_name(index)
                .await
                .map_err(|e| format!("could not read the name of switch {index}: {e}"))?,
        );
    }
    Ok(names)
}

/// The switch id of the output named `output` among `names`.
///
/// # Errors
///
/// A message naming the switch and listing the names it does report.
pub fn index_of(switch_id: &str, names: &[String], output: &str) -> Result<usize, String> {
    names.iter().position(|n| n == output).ok_or_else(|| {
        format!(
            "switch '{switch_id}' reports no output named '{output}'; it reports: {}",
            names
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Read switch `index` of the box: whether it is writable, on, and its
/// value.
///
/// # Errors
///
/// The driver's error, as text, for the first read that fails.
pub async fn read_output(
    device: &dyn Switch,
    index: usize,
    name: String,
) -> Result<OutputReading, String> {
    let can_write = device
        .can_write(index)
        .await
        .map_err(|e| format!("could not read CanWrite of '{name}': {e}"))?;
    let on = device
        .get_switch(index)
        .await
        .map_err(|e| format!("could not read the state of '{name}': {e}"))?;
    let value = device
        .get_switch_value(index)
        .await
        .map_err(|e| format!("could not read the value of '{name}': {e}"))?;
    Ok(OutputReading {
        name,
        index,
        can_write,
        on,
        value,
    })
}

/// The power map's entry for `output`, if `set_switch` may switch it.
///
/// The map must list the output, and to switch it off (`on` false) it
/// must not be protected and every device the map puts on it must be
/// held out. `is_held_out` answers for a device id; an id the registry
/// does not know counts as not held out.
///
/// # Errors
///
/// The refusal, in words that name the reason.
pub fn switchable<'a>(
    switch: &'a SwitchConfig,
    output: &str,
    on: bool,
    is_held_out: impl Fn(&str) -> bool,
) -> Result<&'a OutputConfig, String> {
    let Some(mapped) = switch.output(output) else {
        return Err(format!(
            "output '{output}' of switch '{}' is not in rp's power map, so rp does not know \
             what it feeds; list it under the switch's outputs to switch it through rp",
            switch.id
        ));
    };
    if on {
        return Ok(mapped);
    }
    if let Some(reason) = &mapped.protected {
        return Err(format!(
            "output '{output}' of switch '{}' is protected and is never switched off through \
             rp: {}",
            switch.id,
            reason.as_str()
        ));
    }
    let in_hands: Vec<&str> = mapped
        .devices
        .iter()
        .map(String::as_str)
        .filter(|id| !is_held_out(id))
        .collect();
    if !in_hands.is_empty() {
        return Err(format!(
            "output '{output}' of switch '{}' still feeds devices rp has not disconnected: {}; \
             disconnect each with disconnect_device before switching the output off",
            switch.id,
            in_hands.join(", ")
        ));
    }
    Ok(mapped)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::ProtectedReason;

    fn power_box() -> SwitchConfig {
        SwitchConfig {
            id: "ppba".to_string(),
            name: None,
            alpaca_url: "http://127.0.0.1:1".to_string(),
            device_number: 0,
            unique_id: None,
            auth: None,
            outputs: vec![
                OutputConfig {
                    name: "Quad 12V Output".to_string(),
                    devices: vec!["main-cam".to_string(), "mount".to_string()],
                    protected: None,
                },
                OutputConfig {
                    name: "Adjustable Output".to_string(),
                    devices: Vec::new(),
                    protected: Some(ProtectedReason::try_new("the Pi runs off it".into()).unwrap()),
                },
            ],
        }
    }

    #[test]
    fn an_unmapped_output_is_refused_either_way() {
        let config = power_box();
        for on in [true, false] {
            let err = switchable(&config, "USB Hub", on, |_| true).unwrap_err();
            assert!(err.contains("not in rp's power map"), "{err}");
        }
    }

    #[test]
    fn a_protected_output_switches_on_but_never_off() {
        let config = power_box();
        assert_eq!(
            switchable(&config, "Adjustable Output", true, |_| true)
                .unwrap()
                .name,
            "Adjustable Output"
        );
        let err = switchable(&config, "Adjustable Output", false, |_| true).unwrap_err();
        assert!(err.contains("the Pi runs off it"), "{err}");
    }

    #[test]
    fn an_output_switches_off_only_once_every_device_on_it_is_held_out() {
        let config = power_box();
        let err = switchable(&config, "Quad 12V Output", false, |id| id == "main-cam").unwrap_err();
        assert!(err.contains("mount"), "{err}");
        assert!(!err.contains("main-cam"), "{err}");
        assert_eq!(
            switchable(&config, "Quad 12V Output", false, |_| true)
                .unwrap()
                .devices,
            vec!["main-cam".to_string(), "mount".to_string()]
        );
    }

    #[test]
    fn switching_on_ignores_the_devices_still_connected() {
        let config = power_box();
        assert!(switchable(&config, "Quad 12V Output", true, |_| false).is_ok());
    }

    #[test]
    fn an_output_is_found_by_its_exact_name() {
        let names = vec!["Quad 12V Output".to_string(), "USB Hub".to_string()];
        assert_eq!(index_of("ppba", &names, "USB Hub").unwrap(), 1);
        let err = index_of("ppba", &names, "usb hub").unwrap_err();
        assert!(err.contains("'Quad 12V Output', 'USB Hub'"), "{err}");
    }
}

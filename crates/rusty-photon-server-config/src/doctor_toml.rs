//! Parser for the `services/<svc>/pkg/doctor.toml` catalog-metadata
//! microformat (see `docs/services/doctor.md` §The derived catalog).
//!
//! The format is deliberately a strict TOML subset — `#` comments, blank
//! lines, and exactly the keys below — parsed with std only, so the 17
//! per-service parity tests and doctor's embedded catalog share one parser
//! without pulling a TOML crate into every service's dev-dependencies.
//! Anything outside the subset is a loud error, never a guess.
//!
//! ```toml
//! class = "alpaca"  # "alpaca" | "core"
//! port = 11113
//! # Optional: the unit hard-requires an operator-written config and never
//! # self-creates one (docs/packaging.md's "config-gated" services, e.g.
//! # ConditionPathExists= on Linux, start type Manual on Windows). Defaults
//! # to false — most services self-create their defaults on first start.
//! config_gated = false
//! # Optional hardware identity (docs/services/doctor.md §Hardware):
//! serial_pointer = "/serial/port"
//! serial_default_unix = "/dev/ttyUSB0"
//! serial_default_windows = "COM3"
//! serial_gate_pointer = "/transport/kind"
//! serial_gate_value = "usb"
//! usb_vendor = "0403"
//! usb_product = "6015"
//! usb_model = "PPBA"
//! ```

/// Which shared server shape a service's `server` block uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerClass {
    /// [`crate::AlpacaServerConfig`] — the 11 Alpaca drivers.
    Alpaca,
    /// [`crate::ServerConfig`] — the plain-HTTP services.
    Core,
    /// [`crate::AdvertisingServerConfig`] — a plain-HTTP service that also
    /// advertises its own URL to another process. rp alone today.
    Advertising,
}

/// Where a service's config keeps its serial device path, and what the
/// service falls back to when the file or field is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialMeta {
    /// JSON pointer from the config-file root, e.g. `/serial/port`.
    pub pointer: String,
    pub default_unix: String,
    pub default_windows: String,
    /// When set, the serial checks apply only while the config value at
    /// `.0` equals `.1` (star-adventurer-gti: `/transport/kind` = `usb`).
    pub gate: Option<(String, String)>,
}

/// The USB identity a service's device reports on the bus.
///
/// `model` is a product-string substring — required in practice where
/// the VID:PID is a generic bridge chip shared across devices (FTDI
/// FT-X, RP2040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbMeta {
    /// `idVendor`, four lowercase hex digits.
    pub vendor: String,
    /// `idProduct`, four lowercase hex digits. Omitted for vendor-only
    /// families (a QHY camera is any `1618` device).
    pub product: Option<String>,
    pub model: Option<String>,
}

/// One service's parsed catalog metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorToml {
    pub class: ServerClass,
    pub port: u16,
    /// The unit hard-requires a hand-written config and never self-creates
    /// one (docs/packaging.md's "config-gated" services). Defaults to
    /// `false`.
    pub config_gated: bool,
    pub serial: Option<SerialMeta>,
    pub usb: Option<UsbMeta>,
}

/// Parse a `doctor.toml`. Errors name the offending line so a parity-test
/// failure is self-explanatory.
///
/// # Errors
///
/// Returns a message naming the offending line for an unknown key, a
/// malformed value, or a duplicate; a missing required key is reported
/// as ``missing key `<name>` `` with no line to name.
pub fn parse(content: &str) -> Result<DoctorToml, String> {
    let mut class = None;
    let mut port = None;
    let mut config_gated = None;
    let mut strings: [(&str, Option<String>); 8] = [
        ("serial_pointer", None),
        ("serial_default_unix", None),
        ("serial_default_windows", None),
        ("serial_gate_pointer", None),
        ("serial_gate_value", None),
        ("usb_vendor", None),
        ("usb_product", None),
        ("usb_model", None),
    ];
    for (idx, raw) in content.lines().enumerate() {
        // 1-based for display; the saturation is unreachable (a file
        // with usize::MAX lines does not fit in memory).
        let lineno = idx.saturating_add(1);
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {lineno}: expected `key = value`: {raw:?}"))?;
        match (key.trim(), value.trim()) {
            ("class", v) => {
                let parsed = match v {
                    "\"alpaca\"" => ServerClass::Alpaca,
                    "\"core\"" => ServerClass::Core,
                    "\"advertising\"" => ServerClass::Advertising,
                    other => {
                        return Err(format!(
                            "line {lineno}: class must be \"alpaca\", \"core\", or \
                             \"advertising\", got {other}"
                        ))
                    }
                };
                if class.replace(parsed).is_some() {
                    return Err(format!("line {lineno}: duplicate key `class`"));
                }
            }
            ("port", v) => {
                let parsed = v
                    .parse::<u16>()
                    .map_err(|e| format!("line {lineno}: port is not a u16: {e}"))?;
                if port.replace(parsed).is_some() {
                    return Err(format!("line {lineno}: duplicate key `port`"));
                }
            }
            ("config_gated", v) => {
                let parsed = match v {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(format!(
                            "line {lineno}: config_gated must be true or false, got {other}"
                        ))
                    }
                };
                if config_gated.replace(parsed).is_some() {
                    return Err(format!("line {lineno}: duplicate key `config_gated`"));
                }
            }
            (key, v) => {
                let slot = strings
                    .iter_mut()
                    .find(|(name, _)| *name == key)
                    .ok_or_else(|| format!("line {lineno}: unknown key `{key}`"))?;
                let parsed = parse_string(v).map_err(|e| format!("line {lineno}: {key} {e}"))?;
                if slot.1.replace(parsed).is_some() {
                    return Err(format!("line {lineno}: duplicate key `{key}`"));
                }
            }
        }
    }
    let mut take = |key: &str| {
        strings
            .iter_mut()
            .find(|(name, _)| *name == key)
            .and_then(|(_, v)| v.take())
    };
    let serial = build_serial(
        take("serial_pointer"),
        take("serial_default_unix"),
        take("serial_default_windows"),
        take("serial_gate_pointer"),
        take("serial_gate_value"),
    )?;
    let usb = build_usb(take("usb_vendor"), take("usb_product"), take("usb_model"))?;
    Ok(DoctorToml {
        class: class.ok_or("missing key `class`")?,
        port: port.ok_or("missing key `port`")?,
        config_gated: config_gated.unwrap_or(false),
        serial,
        usb,
    })
}

/// The serial keys travel as a unit: a pointer without its platform
/// defaults (or vice versa) would make the effective-path fallback a
/// guess, and a gate half-declared is a gate that silently never applies.
fn build_serial(
    pointer: Option<String>,
    default_unix: Option<String>,
    default_windows: Option<String>,
    gate_pointer: Option<String>,
    gate_value: Option<String>,
) -> Result<Option<SerialMeta>, String> {
    let gate = match (gate_pointer, gate_value) {
        (Some(p), Some(v)) => {
            require_pointer("serial_gate_pointer", &p)?;
            Some((p, v))
        }
        (None, None) => None,
        _ => {
            return Err(
                "serial_gate_pointer and serial_gate_value must be declared together".to_string(),
            )
        }
    };
    match (pointer, default_unix, default_windows) {
        (Some(pointer), Some(default_unix), Some(default_windows)) => {
            require_pointer("serial_pointer", &pointer)?;
            Ok(Some(SerialMeta {
                pointer,
                default_unix,
                default_windows,
                gate,
            }))
        }
        (None, None, None) => {
            if gate.is_some() {
                return Err("serial_gate_* requires serial_pointer".to_string());
            }
            Ok(None)
        }
        _ => Err(
            "serial_pointer, serial_default_unix, and serial_default_windows \
                  must be declared together"
                .to_string(),
        ),
    }
}

fn build_usb(
    vendor: Option<String>,
    product: Option<String>,
    model: Option<String>,
) -> Result<Option<UsbMeta>, String> {
    if let Some(vendor) = vendor {
        require_hex4("usb_vendor", &vendor)?;
        if let Some(product) = &product {
            require_hex4("usb_product", product)?;
        }
        Ok(Some(UsbMeta {
            vendor,
            product,
            model,
        }))
    } else {
        if product.is_some() || model.is_some() {
            return Err("usb_product/usb_model require usb_vendor".to_string());
        }
        Ok(None)
    }
}

fn require_pointer(key: &str, value: &str) -> Result<(), String> {
    if value.starts_with('/') {
        Ok(())
    } else {
        Err(format!(
            "{key} must be a JSON pointer starting with `/`, got {value:?}"
        ))
    }
}

/// USB ids are compared against sysfs, which prints four lowercase hex
/// digits — requiring that exact form here avoids a case-folding layer.
fn require_hex4(key: &str, value: &str) -> Result<(), String> {
    if value.len() == 4
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(format!(
            "{key} must be four lowercase hex digits, got {value:?}"
        ))
    }
}

/// Parse a double-quoted string value. No escape sequences — none of the
/// declared values (paths, pointers, hex ids, product substrings) need
/// them, and rejecting keeps the microformat unambiguous.
fn parse_string(value: &str) -> Result<String, String> {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| format!("must be a double-quoted string, got {value}"))?;
    if inner.contains('"') || inner.contains('\\') {
        return Err(format!("must not contain quotes or backslashes: {value}"));
    }
    if inner.is_empty() {
        return Err("must not be empty".to_string());
    }
    Ok(inner.to_string())
}

/// Strip a `#` comment. The microformat's quoted strings (class literals,
/// paths, pointers, hex ids, product substrings) never contain `#` — a
/// value that did would lose its closing quote here and fail the
/// quoted-string parse loudly — so no quote-awareness is needed.
fn strip_comment(line: &str) -> &str {
    match line.split_once('#') {
        Some((before, _)) => before,
        None => line,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_file_shape() {
        let meta = parse(
            "# Catalog metadata for rusty-photon-doctor (docs/services/doctor.md).\n\
             class = \"alpaca\"  # which shared server shape\n\
             port = 11113\n",
        )
        .unwrap();
        assert_eq!(meta.class, ServerClass::Alpaca);
        assert_eq!(meta.port, 11113);
        assert!(!meta.config_gated);
        assert_eq!(meta.serial, None);
        assert_eq!(meta.usb, None);
    }

    #[test]
    fn parses_core_class() {
        let meta = parse("class = \"core\"\nport = 11114\n").unwrap();
        assert_eq!(meta.class, ServerClass::Core);
    }

    #[test]
    fn parses_advertising_class() {
        let meta = parse("class = \"advertising\"\nport = 11115\n").unwrap();
        assert_eq!(meta.class, ServerClass::Advertising);
    }

    #[test]
    fn parses_config_gated() {
        let meta = parse("class = \"core\"\nport = 11170\nconfig_gated = true\n").unwrap();
        assert!(meta.config_gated);
    }

    #[test]
    fn rejects_malformed_config_gated() {
        let err = parse("class = \"core\"\nport = 1\nconfig_gated = yes\n").unwrap_err();
        assert!(err.contains("config_gated must be true or false"), "{err}");
    }

    #[test]
    fn parses_the_full_hardware_identity() {
        let meta = parse(
            "class = \"alpaca\"\n\
             port = 11117\n\
             serial_pointer = \"/transport/port\"\n\
             serial_default_unix = \"/dev/ttyACM0\"\n\
             serial_default_windows = \"COM3\"\n\
             serial_gate_pointer = \"/transport/kind\"\n\
             serial_gate_value = \"usb\"\n\
             usb_vendor = \"0403\"\n\
             usb_product = \"6015\"\n\
             usb_model = \"PPBA\"  # product-string substring\n",
        )
        .unwrap();
        let serial = meta.serial.unwrap();
        assert_eq!(serial.pointer, "/transport/port");
        assert_eq!(serial.default_unix, "/dev/ttyACM0");
        assert_eq!(serial.default_windows, "COM3");
        assert_eq!(
            serial.gate,
            Some(("/transport/kind".to_string(), "usb".to_string()))
        );
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "0403");
        assert_eq!(usb.product.as_deref(), Some("6015"));
        assert_eq!(usb.model.as_deref(), Some("PPBA"));
    }

    #[test]
    fn parses_vendor_only_usb_identity() {
        let meta = parse("class = \"alpaca\"\nport = 11121\nusb_vendor = \"1618\"\n").unwrap();
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "1618");
        assert_eq!(usb.product, None);
        assert_eq!(usb.model, None);
    }

    #[test]
    fn rejects_unknown_class() {
        let err = parse("class = \"driver\"\nport = 1\n").unwrap_err();
        assert!(err.contains("class must be"), "{err}");
    }

    #[test]
    fn rejects_unknown_key() {
        let err = parse("class = \"core\"\nport = 1\nhealth = \"/health\"\n").unwrap_err();
        assert!(err.contains("unknown key `health`"), "{err}");
    }

    #[test]
    fn rejects_missing_keys() {
        assert!(parse("class = \"core\"\n").unwrap_err().contains("port"));
        assert!(parse("port = 1\n").unwrap_err().contains("class"));
    }

    #[test]
    fn rejects_duplicate_keys() {
        let err = parse("port = 1\nport = 2\nclass = \"core\"\n").unwrap_err();
        assert!(err.contains("duplicate key `port`"), "{err}");
        let err =
            parse("class = \"core\"\nport = 1\nusb_vendor = \"1618\"\nusb_vendor = \"03c3\"\n")
                .unwrap_err();
        assert!(err.contains("duplicate key `usb_vendor`"), "{err}");
    }

    #[test]
    fn rejects_out_of_range_port() {
        let err = parse("class = \"core\"\nport = 70000\n").unwrap_err();
        assert!(err.contains("not a u16"), "{err}");
    }

    #[test]
    fn rejects_non_key_value_lines() {
        let err = parse("[section]\nclass = \"core\"\nport = 1\n").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn rejects_partial_serial_declarations() {
        let err =
            parse("class = \"alpaca\"\nport = 1\nserial_pointer = \"/serial/port\"\n").unwrap_err();
        assert!(err.contains("declared together"), "{err}");
        let err = parse(
            "class = \"alpaca\"\nport = 1\n\
             serial_pointer = \"/serial/port\"\n\
             serial_default_unix = \"/dev/ttyUSB0\"\n\
             serial_default_windows = \"COM3\"\n\
             serial_gate_pointer = \"/transport/kind\"\n",
        )
        .unwrap_err();
        assert!(
            err.contains("serial_gate_pointer and serial_gate_value"),
            "{err}"
        );
        let err = parse(
            "class = \"alpaca\"\nport = 1\n\
             serial_gate_pointer = \"/transport/kind\"\n\
             serial_gate_value = \"usb\"\n",
        )
        .unwrap_err();
        assert!(err.contains("requires serial_pointer"), "{err}");
    }

    #[test]
    fn rejects_usb_fields_without_a_vendor() {
        let err = parse("class = \"alpaca\"\nport = 1\nusb_model = \"PPBA\"\n").unwrap_err();
        assert!(err.contains("require usb_vendor"), "{err}");
    }

    #[test]
    fn rejects_malformed_usb_ids() {
        for bad in ["\"403\"", "\"0403f\"", "\"04X3\"", "\"04A3\""] {
            let err = parse(&format!(
                "class = \"alpaca\"\nport = 1\nusb_vendor = {bad}\n"
            ))
            .unwrap_err();
            assert!(err.contains("four lowercase hex digits"), "{bad}: {err}");
        }
    }

    #[test]
    fn rejects_non_pointer_pointers() {
        let err = parse(
            "class = \"alpaca\"\nport = 1\n\
             serial_pointer = \"serial.port\"\n\
             serial_default_unix = \"/dev/ttyUSB0\"\n\
             serial_default_windows = \"COM3\"\n",
        )
        .unwrap_err();
        assert!(err.contains("JSON pointer"), "{err}");
    }

    #[test]
    fn rejects_unquoted_and_empty_string_values() {
        let err = parse("class = \"alpaca\"\nport = 1\nusb_model = PPBA\n").unwrap_err();
        assert!(err.contains("double-quoted"), "{err}");
        let err = parse("class = \"alpaca\"\nport = 1\nusb_model = \"\"\n").unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");
    }
}

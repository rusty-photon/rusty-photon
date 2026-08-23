//! Textual udev-rule inspection: enough parsing to catch the failure that
//! motivated the check, plus the `ATTRS{idVendor}` extraction the catalog
//! parity test joins against.
//!
//! The failure: udev silently drops an entire rule line when a `GROUP=`
//! assignment names a group the host cannot resolve. This is a scanner,
//! not a udev grammar: it reads the two token shapes our shipped rules use
//! and any operator edit of them would keep.

/// The group names assigned anywhere in the rule content (`GROUP="x"` and
/// the final-assignment form `GROUP:="x"`), deduplicated, in order of
/// first appearance. Comment lines are skipped.
#[must_use]
pub fn group_assignments(content: &str) -> Vec<String> {
    scan(content, "GROUP")
}

/// The `idVendor` values the rule matches (`ATTRS{idVendor}=="xxxx"`),
/// deduplicated, in order of first appearance.
#[must_use]
pub fn vendor_matches(content: &str) -> Vec<String> {
    scan(content, "ATTRS{idVendor}")
}

/// Extract quoted values following `<token>`, `<token>:`, `<token>=`, or
/// `<token>==` — i.e. both assignment and match operators, with the
/// whitespace udevd tolerates around them (`GROUP = "x"`).
fn scan(content: &str, token: &str) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let mut rest = line;
        while let Some((_, after_token)) = rest.split_once(token) {
            rest = after_token;
            let after = rest
                .trim_start()
                .trim_start_matches([':', '='])
                .trim_start();
            let Some(quoted) = after.strip_prefix('"') else {
                continue;
            };
            let Some((value, _)) = quoted.split_once('"') else {
                continue;
            };
            if !value.is_empty() && !values.iter().any(|v| v == value) {
                values.push(value.to_string());
            }
        }
    }
    values
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The shipped qhy rule's exact shape.
    const QHY_RULE: &str = r#"# Tighten the SDK's blanket MODE="0666".
SUBSYSTEMS=="usb", ATTRS{idVendor}=="1618", GROUP="rusty-photon", MODE="0660"
ACTION=="add", SUBSYSTEMS=="usb", ATTRS{idVendor}=="1618", RUN+="/bin/sh -c 'echo 200 > /sys/module/usbcore/parameters/usbfs_memory_mb || true'"
"#;

    #[test]
    fn test_extracts_the_shipped_rule_shape() {
        assert_eq!(group_assignments(QHY_RULE), vec!["rusty-photon"]);
        assert_eq!(vendor_matches(QHY_RULE), vec!["1618"]);
    }

    #[test]
    fn test_final_assignment_and_multiple_groups() {
        let content = "KERNEL==\"ttyUSB*\", GROUP:=\"dialout\"\n\
                       SUBSYSTEMS==\"usb\", GROUP=\"plugdev\"\n\
                       SUBSYSTEMS==\"usb\", GROUP=\"plugdev\"\n";
        assert_eq!(group_assignments(content), vec!["dialout", "plugdev"]);
    }

    #[test]
    fn test_whitespace_around_operators_is_tolerated() {
        // udevd accepts spaces around the operator; operator-edited rules
        // that are semantically equivalent must not vanish from the scan.
        let content = "SUBSYSTEMS==\"usb\", GROUP = \"plugdev\"\n\
                       KERNEL==\"ttyUSB*\", GROUP= \"dialout\"\n\
                       ATTRS{idVendor} == \"1618\", MODE=\"0660\"\n";
        assert_eq!(group_assignments(content), vec!["plugdev", "dialout"]);
        assert_eq!(vendor_matches(content), vec!["1618"]);
    }

    #[test]
    fn test_comments_and_group_free_rules_yield_nothing() {
        let content = "# GROUP=\"commented\"\nACTION==\"add\", RUN+=\"/bin/true\"\n";
        assert!(group_assignments(content).is_empty());
        assert!(vendor_matches(content).is_empty());
    }

    #[test]
    fn test_unquoted_or_unterminated_values_are_skipped() {
        assert!(group_assignments("GROUP=plugdev\n").is_empty());
        assert!(group_assignments("GROUP=\"plugdev\n").is_empty());
        assert!(group_assignments("GROUP=\"\"\n").is_empty());
    }
}

//! The doctor report schema (docs/services/doctor.md §Report) — the
//! canonical home since D5, because every service binary serializes it
//! from its `doctor` subcommand and central doctor aggregates it.
//!
//! The schema crosses a binary boundary, so unlike every config shape it
//! parses **permissively**: unknown fields are tolerated, missing ones
//! default, and an unknown status, mode, or fix op from a newer binary
//! degrades to an explicit `Unknown` variant instead of refusing the
//! whole report.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The version of this schema; bumped on incompatible shape changes.
pub const SCHEMA_VERSION: u32 = 1;

/// How the run diagnosed the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// At least one `rusty-photon-*` unit is installed: the full check set.
    Packaged,
    /// No units — a dev checkout; config-only checks.
    #[default]
    ConfigOnly,
    /// A single service's own `doctor` subcommand: its config file plus,
    /// on SDK-linking services, SDK enumeration.
    Service,
    /// A mode this build does not know — a report from a newer binary.
    /// Never emitted, only parsed.
    #[serde(other)]
    Unknown,
}

/// One check's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Ok,
    Warn,
    Fail,
    /// A status this doctor build does not know — a report from a newer
    /// binary. Never emitted, only parsed.
    #[serde(other)]
    Unknown,
}

/// A machine-applicable fix: one primitive JSON-pointer operation against
/// one service's config file.
///
/// Primitive ops keep the schema forward-parseable — an aggregator that
/// does not recognize a newer op simply cannot apply it, instead of
/// misparsing the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum FixOp {
    SetNumber {
        service: String,
        pointer: String,
        value: u64,
    },
    SetString {
        service: String,
        pointer: String,
        value: String,
    },
    /// A whole JSON object written at the pointer — `server.tls`,
    /// `server.auth`, and the client auth blocks the provisioning pass
    /// distributes. Applied only where the target is absent (or `null`):
    /// present blocks are operator intent and are never overwritten.
    SetObject {
        service: String,
        pointer: String,
        value: serde_json::Value,
    },
    RemoveKey {
        service: String,
        pointer: String,
    },
    /// Provisioning action (not a config-pointer op): the self-signed CA
    /// was generated under the pki tree.
    GenerateCa,
    /// Provisioning action: a service certificate pair was issued.
    GenerateCert {
        service: String,
    },
    /// Provisioning action: the observatory credential was minted and its
    /// canonical `pki/credential` copy written.
    MintCredential,
    /// Provisioning action: the ACME wildcard pair for `*.<domain>` was
    /// renewed under the pki tree.
    RenewAcme {
        domain: String,
    },
    /// An op this doctor build does not know — a plan from a newer binary.
    /// Never emitted, only parsed; it cannot be applied.
    #[serde(other)]
    Unknown,
}

impl FixOp {
    /// The service whose config file (or certificate) this op targets
    /// (`None` for host-wide provisioning actions and ops from a newer
    /// binary).
    #[must_use]
    pub fn service(&self) -> Option<&str> {
        match self {
            Self::SetNumber { service, .. }
            | Self::SetString { service, .. }
            | Self::SetObject { service, .. }
            | Self::RemoveKey { service, .. }
            | Self::GenerateCert { service } => Some(service),
            Self::GenerateCa | Self::MintCredential | Self::RenewAcme { .. } | Self::Unknown => {
                None
            }
        }
    }
}

impl std::fmt::Display for FixOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetNumber {
                service,
                pointer,
                value,
            } => write!(f, "{service}.json: set {pointer} to {value}"),
            Self::SetString {
                service,
                pointer,
                value,
            } => write!(f, "{service}.json: set {pointer} to \"{value}\""),
            // The value may carry credential material (a password hash, the
            // client plaintext), so only the pointer is printed.
            Self::SetObject {
                service, pointer, ..
            } => write!(f, "{service}.json: set {pointer}"),
            Self::RemoveKey { service, pointer } => {
                write!(f, "{service}.json: remove {pointer}")
            }
            Self::GenerateCa => write!(f, "pki: generated the CA certificate and key"),
            Self::GenerateCert { service } => {
                write!(f, "pki: issued a certificate pair for {service}")
            }
            Self::MintCredential => {
                write!(f, "pki: minted the observatory credential")
            }
            Self::RenewAcme { domain } => {
                write!(f, "pki: renewed the ACME wildcard pair for *.{domain}")
            }
            Self::Unknown => write!(f, "an operation this doctor build does not know"),
        }
    }
}

/// One fix `--fix` actually wrote, recorded in the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedFix {
    /// The check that planned it.
    #[serde(default)]
    pub check: String,
    #[serde(default = "unknown_fix_op")]
    pub op: FixOp,
}

const fn unknown_fix_op() -> FixOp {
    FixOp::Unknown
}

/// One diagnosis: a stable name, the service it concerns (when
/// service-scoped), the outcome, and a human-readable detail.
///
/// `suggestion` carries a concrete remedy as text where doctor can offer
/// one; `fixes` carries the machine-applicable plan `--fix` applies, where
/// the correct value is derivable rather than a judgment call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<FixOp>,
}

impl Check {
    pub fn ok(name: &str, service: impl Into<Option<String>>, detail: impl Into<String>) -> Self {
        Self::new(name, service, Status::Ok, detail, None)
    }

    pub fn warn(
        name: &str,
        service: impl Into<Option<String>>,
        detail: impl Into<String>,
        suggestion: impl Into<Option<String>>,
    ) -> Self {
        Self::new(name, service, Status::Warn, detail, suggestion.into())
    }

    pub fn fail(
        name: &str,
        service: impl Into<Option<String>>,
        detail: impl Into<String>,
        suggestion: impl Into<Option<String>>,
    ) -> Self {
        Self::new(name, service, Status::Fail, detail, suggestion.into())
    }

    fn new(
        name: &str,
        service: impl Into<Option<String>>,
        status: Status,
        detail: impl Into<String>,
        suggestion: Option<String>,
    ) -> Self {
        Self {
            name: name.to_string(),
            service: service.into(),
            status,
            detail: detail.into(),
            suggestion,
            fixes: Vec::new(),
        }
    }

    /// Attach a machine-applicable fix plan.
    #[must_use]
    pub fn with_fixes(mut self, fixes: Vec<FixOp>) -> Self {
        self.fixes = fixes;
        self
    }
}

/// The whole report. `ok` checks are included: an empty report must never
/// be mistaken for a clean one. On a `--fix` run, `checks` is the post-fix
/// diagnosis and `fixes_applied` records what was written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub schema_version: u32,
    /// The **emitting** binary's version — central doctor's on an
    /// aggregate report, the service binary's on a `mode: service` one.
    /// That asymmetry is what makes version skew visible in aggregation.
    #[serde(default)]
    pub doctor_version: String,
    #[serde(default)]
    pub mode: Mode,
    /// The service that emitted a `mode: service` report; absent on
    /// central doctor's own reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The config directory on a central report; the single config file
    /// on a `mode: service` one.
    #[serde(default)]
    pub config_dir: PathBuf,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes_applied: Vec<AppliedFix>,
    /// The staged op plan a `tls flip-to-acme --dry-run` would apply —
    /// `fixes_applied`'s shape, distinct because nothing was written
    /// (docs/services/doctor.md §The flip orchestrator). Populated by no
    /// other run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<AppliedFix>,
}

impl Report {
    /// A central-doctor report. `version` is the emitting binary's own
    /// `CARGO_PKG_VERSION` — a parameter, not an `env!` here, because this
    /// crate's version is not the binary's.
    #[must_use]
    pub fn new(version: &str, mode: Mode, config_dir: PathBuf, checks: Vec<Check>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            doctor_version: version.to_string(),
            mode,
            service: None,
            config_dir,
            checks,
            fixes_applied: Vec::new(),
            plan: Vec::new(),
        }
    }

    /// A per-service report (`mode: service`), self-identified by the
    /// emitting service's catalog name.
    #[must_use]
    pub fn for_service(
        version: &str,
        service: &str,
        config_path: PathBuf,
        checks: Vec<Check>,
    ) -> Self {
        Self {
            service: Some(service.to_string()),
            ..Self::new(version, Mode::Service, config_path, checks)
        }
    }

    /// Record what a `--fix` run wrote.
    #[must_use]
    pub fn with_fixes_applied(mut self, fixes_applied: Vec<AppliedFix>) -> Self {
        self.fixes_applied = fixes_applied;
        self
    }

    /// Record what a `tls flip-to-acme --dry-run` would write.
    #[must_use]
    pub fn with_plan(mut self, plan: Vec<AppliedFix>) -> Self {
        self.plan = plan;
        self
    }

    /// True when at least one check failed — the exit-1 condition.
    /// [`Status::Unknown`] counts as a failure: it only appears when
    /// aggregating a newer binary's report, and treating an unrecognized
    /// outcome as clean would let the exit code disagree with the rendered
    /// report.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|c| matches!(c.status, Status::Fail | Status::Unknown))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_report_round_trips() {
        let fix = FixOp::SetNumber {
            service: "qhy-focuser".to_string(),
            pointer: "/server/port".to_string(),
            value: 11113,
        };
        let report = Report::new(
            "0.1.0",
            Mode::Packaged,
            PathBuf::from("/etc/rusty-photon"),
            vec![Check::fail(
                "ports.collision",
                Some("qhy-focuser".to_string()),
                "qhy-focuser and dsd-fp2 both resolve to port 11113",
                Some("set a distinct server.port".to_string()),
            )
            .with_fixes(vec![fix.clone()])],
        )
        .with_fixes_applied(vec![AppliedFix {
            check: "ports.collision".to_string(),
            op: fix.clone(),
        }]);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""op":"set-number""#), "{json}");
        assert!(!json.contains(r#""service":null"#), "{json}");
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.doctor_version, "0.1.0");
        assert_eq!(back.mode, Mode::Packaged);
        assert_eq!(back.service, None);
        assert_eq!(back.checks[0].status, Status::Fail);
        assert_eq!(back.checks[0].fixes, vec![fix.clone()]);
        assert_eq!(back.fixes_applied[0].op, fix);
        assert!(back.has_failures());
    }

    #[test]
    fn test_service_report_self_identifies() {
        let report = Report::for_service(
            "0.3.2",
            "ppba-driver",
            PathBuf::from("/etc/rusty-photon/ppba-driver.json"),
            vec![Check::ok("config.full-shape", None, "parses")],
        );
        let json = serde_json::to_string(&report).unwrap();
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, Mode::Service);
        assert_eq!(back.service.as_deref(), Some("ppba-driver"));
        assert_eq!(back.doctor_version, "0.3.2");
        assert!(!back.has_failures());
    }

    #[test]
    fn test_fix_op_display_names_the_file_and_operation() {
        let set_number = FixOp::SetNumber {
            service: "dsd-fp2".to_string(),
            pointer: "/server/port".to_string(),
            value: 11119,
        };
        assert_eq!(
            set_number.to_string(),
            "dsd-fp2.json: set /server/port to 11119"
        );
        assert_eq!(set_number.service(), Some("dsd-fp2"));

        let set_string = FixOp::SetString {
            service: "ui-htmx".to_string(),
            pointer: "/drivers/x/base_url".to_string(),
            value: "http://localhost:11113".to_string(),
        };
        assert_eq!(
            set_string.to_string(),
            "ui-htmx.json: set /drivers/x/base_url to \"http://localhost:11113\""
        );
        assert_eq!(set_string.service(), Some("ui-htmx"));

        let remove = FixOp::RemoveKey {
            service: "sentinel".to_string(),
            pointer: "/services".to_string(),
        };
        assert_eq!(remove.to_string(), "sentinel.json: remove /services");
        assert_eq!(remove.service(), Some("sentinel"));

        assert_eq!(
            FixOp::Unknown.to_string(),
            "an operation this doctor build does not know"
        );
        assert_eq!(FixOp::Unknown.service(), None);
    }

    #[test]
    fn test_provisioning_ops_round_trip_with_kebab_tags() {
        let ops = vec![
            FixOp::SetObject {
                service: "ppba-driver".to_string(),
                pointer: "/server/tls".to_string(),
                value: serde_json::json!({ "cert": "/p/ppba-driver.pem", "key": "/p/ppba-driver-key.pem" }),
            },
            FixOp::GenerateCa,
            FixOp::GenerateCert {
                service: "dsd-fp2".to_string(),
            },
            FixOp::MintCredential,
            FixOp::RenewAcme {
                domain: "observatory.example.com".to_string(),
            },
        ];
        let json = serde_json::to_string(&ops).unwrap();
        for tag in [
            r#""op":"set-object""#,
            r#""op":"generate-ca""#,
            r#""op":"generate-cert""#,
            r#""op":"mint-credential""#,
            r#""op":"renew-acme""#,
        ] {
            assert!(json.contains(tag), "{tag} missing from {json}");
        }
        let back: Vec<FixOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ops);
        assert_eq!(ops[0].service(), Some("ppba-driver"));
        assert_eq!(ops[1].service(), None);
        assert_eq!(ops[2].service(), Some("dsd-fp2"));
        assert_eq!(ops[3].service(), None);
        assert_eq!(ops[4].service(), None);
    }

    #[test]
    fn test_set_object_display_never_prints_the_value() {
        let op = FixOp::SetObject {
            service: "sentinel".to_string(),
            pointer: "/service_auth".to_string(),
            value: serde_json::json!({ "username": "observatory", "password": "s3cret" }),
        };
        let rendered = op.to_string();
        assert_eq!(rendered, "sentinel.json: set /service_auth");
        assert!(!rendered.contains("s3cret"));
        assert_eq!(
            FixOp::GenerateCa.to_string(),
            "pki: generated the CA certificate and key"
        );
        assert_eq!(
            FixOp::GenerateCert {
                service: "rp".to_string()
            }
            .to_string(),
            "pki: issued a certificate pair for rp"
        );
        assert_eq!(
            FixOp::MintCredential.to_string(),
            "pki: minted the observatory credential"
        );
        assert_eq!(
            FixOp::RenewAcme {
                domain: "observatory.example.com".to_string()
            }
            .to_string(),
            "pki: renewed the ACME wildcard pair for *.observatory.example.com"
        );
    }

    #[test]
    fn test_a_fix_op_from_a_newer_binary_degrades_to_unknown() {
        let json = r#"{
            "checks": [ {
                "name": "hardware.usb",
                "fixes": [ { "op": "reload-udev", "service": "zwo-camera" } ]
            } ],
            "fixes_applied": [ { "check": "hardware.usb" } ]
        }"#;
        let report: Report = serde_json::from_str(json).unwrap();
        assert_eq!(report.checks[0].fixes, vec![FixOp::Unknown]);
        assert_eq!(report.checks[0].fixes[0].service(), None);
        assert_eq!(report.fixes_applied[0].op, FixOp::Unknown);
    }

    #[test]
    fn test_report_from_a_newer_binary_degrades_instead_of_refusing() {
        let json = r#"{
            "schema_version": 3,
            "mode": "cluster-wide",
            "checks": [
                { "name": "hardware.usb", "status": "degraded", "novel_field": 1 },
                { "name": "ports.collision", "status": "fail" }
            ],
            "novel_top_level": {}
        }"#;
        let report: Report = serde_json::from_str(json).unwrap();
        assert_eq!(report.mode, Mode::Unknown);
        assert_eq!(report.checks[0].status, Status::Unknown);
        assert_eq!(report.checks[1].status, Status::Fail);
        assert!(report.has_failures());
        assert_eq!(report.doctor_version, "");
    }

    #[test]
    fn test_unknown_statuses_alone_count_as_failures() {
        let json = r#"{ "checks": [ { "name": "x", "status": "degraded" } ] }"#;
        let report: Report = serde_json::from_str(json).unwrap();
        assert!(
            report.has_failures(),
            "an unrecognized outcome must not exit clean"
        );
    }

    #[test]
    fn test_warnings_are_not_failures() {
        let report = Report::new(
            "0.1.0",
            Mode::ConfigOnly,
            PathBuf::new(),
            vec![Check::warn("x", None, "d", None)],
        );
        assert!(!report.has_failures());
    }
}

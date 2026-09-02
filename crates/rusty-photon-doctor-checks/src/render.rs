//! Human-readable report rendering, shared by central doctor and the
//! per-service `doctor` subcommands.
//!
//! Warnings and failures print in full; passing checks are summarized by
//! count — the operator reads this at the rig, so signal density beats
//! completeness (the full list is in --json).

use std::fmt::Write as _;

use crate::report::{Mode, Report, Status};

#[must_use]
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let central_mode = match report.mode {
        Mode::Service => {
            let service = report.service.as_deref().unwrap_or("service");
            let _ = writeln!(
                out,
                "rusty-photon-{service} doctor {} — config: {}",
                report.doctor_version,
                report.config_dir.display()
            );
            None
        }
        Mode::Packaged => Some("packaged"),
        Mode::ConfigOnly => Some("config-only (no rusty-photon units installed)"),
        Mode::Unknown => Some("unknown (report from a newer binary)"),
    };
    if let Some(mode) = central_mode {
        let _ = writeln!(
            out,
            "rusty-photon-doctor {} — mode: {mode}, config dir: {}",
            report.doctor_version,
            report.config_dir.display()
        );
    }
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut fail = 0usize;
    for check in &report.checks {
        let label = match check.status {
            Status::Ok => {
                ok = ok.saturating_add(1);
                continue;
            }
            Status::Warn => {
                warn = warn.saturating_add(1);
                "WARN"
            }
            Status::Fail | Status::Unknown => {
                fail = fail.saturating_add(1);
                "FAIL"
            }
        };
        let scope = check
            .service
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let _ = writeln!(out, "{label} {}{scope}: {}", check.name, check.detail);
        if let Some(suggestion) = &check.suggestion {
            let _ = writeln!(out, "     fix: {suggestion}");
        }
    }
    for planned in &report.plan {
        let _ = writeln!(out, "PLANNED {} — {}", planned.check, planned.op);
    }
    for applied in &report.fixes_applied {
        let _ = writeln!(out, "FIXED {} — {}", applied.check, applied.op);
    }
    let _ = writeln!(out, "summary: {ok} ok, {warn} warn, {fail} fail");
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::report::Check;

    #[test]
    fn test_render_summarizes_ok_and_prints_failures_in_full() {
        let report = Report::new(
            "0.1.0",
            Mode::Packaged,
            "/etc/rusty-photon".into(),
            vec![
                Check::ok("config.server-shape", Some("rp".to_string()), "fine"),
                Check::fail(
                    "ports.collision",
                    Some("qhy-focuser".to_string()),
                    "port clash",
                    Some("change a port".to_string()),
                ),
                Check::warn("urls.sentinel-suffix", None, "no suffix", None),
            ],
        );
        let text = render(&report);
        assert!(
            text.contains("rusty-photon-doctor 0.1.0 — mode: packaged"),
            "{text}"
        );
        assert!(text.contains("summary: 1 ok, 1 warn, 1 fail"), "{text}");
        assert!(text.contains("FAIL ports.collision (qhy-focuser): port clash"));
        assert!(text.contains("fix: change a port"));
        assert!(text.contains("WARN urls.sentinel-suffix: no suffix"));
        assert!(
            !text.contains("config.server-shape"),
            "ok checks are counted, not listed: {text}"
        );
    }

    #[test]
    fn test_render_service_report_header_names_the_service() {
        let report = Report::for_service(
            "0.3.2",
            "ppba-driver",
            "/etc/rusty-photon/ppba-driver.json".into(),
            vec![Check::ok("config.full-shape", None, "parses")],
        );
        let text = render(&report);
        assert!(
            text.contains(
                "rusty-photon-ppba-driver doctor 0.3.2 — config: /etc/rusty-photon/ppba-driver.json"
            ),
            "{text}"
        );
        assert!(text.contains("summary: 1 ok, 0 warn, 0 fail"), "{text}");
    }

    #[test]
    fn test_render_lists_applied_fixes() {
        let report = Report::new("0.1.0", Mode::Packaged, "/etc/rusty-photon".into(), vec![])
            .with_fixes_applied(vec![crate::report::AppliedFix {
                check: "config.retired-keys".to_string(),
                op: crate::report::FixOp::RemoveKey {
                    service: "sentinel".to_string(),
                    pointer: "/services".to_string(),
                },
            }]);
        let text = render(&report);
        assert!(
            text.contains("FIXED config.retired-keys — sentinel.json: remove /services"),
            "{text}"
        );
    }
}

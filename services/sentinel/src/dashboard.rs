//! Web dashboard with JSON API endpoints (hand-rolled HTML + JS polling)
//! plus the Service Restart API (`POST /api/services/{name}/restart`).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::restart::{RestartError, RestartManager};
use crate::state::StateHandle;

/// Flatten a `Duration` to integer milliseconds for the dashboard JS, which
/// does `new Date(last_poll_epoch_ms + polling_interval_ms)` arithmetic.
/// Rounds non-zero sub-millisecond durations up to `1` (so a configured
/// `500us` interval doesn't silently collapse to `0`) and saturates the
/// `u128 → u64` cast (a 584-million-year interval is beyond the
/// representable range — saturate rather than wrap).
fn duration_to_ms_for_js(d: Duration) -> u64 {
    let ms = d.as_millis();
    let rounded = if ms == 0 && !d.is_zero() { 1 } else { ms };
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// Escape a string for interpolation into server-rendered HTML. Names and
/// messages originate from config keys and notification text, so they must
/// not be treated as markup.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Dashboard application state
#[derive(Clone)]
pub struct DashboardState {
    pub state: StateHandle,
    /// The Service Restart API's engine (the supervised-services registry +
    /// shell seam). See `docs/services/sentinel.md` §Service Restart API.
    pub restarts: Arc<RestartManager>,
}

/// Build the dashboard axum router
pub fn build_router(state: StateHandle, restarts: Arc<RestartManager>) -> Router {
    let dashboard_state = DashboardState { state, restarts };

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(status_handler))
        .route("/api/history", get(history_handler))
        .route("/api/services", get(services_handler))
        .route("/api/services/{name}/restart", post(restart_handler))
        .route("/health", get(health_handler))
        .with_state(dashboard_state)
}

async fn index_handler(State(dashboard): State<DashboardState>) -> impl IntoResponse {
    let state = dashboard.state.read().await;
    Html(render_dashboard(
        &monitor_rows(&state.monitors),
        &service_rows(&state.services),
        &history_rows(&state.history),
    ))
}

/// Fill [`DASHBOARD_PAGE`]'s placeholders. Substitution walks the
/// template sections only (rather than chaining `replace` over the
/// whole page, so the scaffold can live in one const with its JS
/// braces undoubled) — inserted row HTML is never rescanned, and field
/// text that happens to contain a placeholder token cannot splice
/// markup into the wrong place.
fn render_dashboard(monitor_rows: &str, service_rows: &str, history_rows: &str) -> String {
    let mut html = String::with_capacity(
        DASHBOARD_PAGE
            .len()
            .saturating_add(monitor_rows.len())
            .saturating_add(service_rows.len())
            .saturating_add(history_rows.len()),
    );
    let mut rest = DASHBOARD_PAGE;
    for (token, rows) in [
        ("%monitor_rows%", monitor_rows),
        ("%service_rows%", service_rows),
        ("%history_rows%", history_rows),
    ] {
        // A missing token is a template-editing bug: skip that block's
        // rows so the literal placeholder stays visible on the page as
        // the signal, instead of splicing rows at the end.
        let Some((head, tail)) = rest.split_once(token) else {
            tracing::debug!(token, "dashboard template is missing a placeholder token");
            continue;
        };
        html.push_str(head);
        html.push_str(rows);
        rest = tail;
    }
    html.push_str(rest);
    html
}

/// One server-rendered `<tr>` per monitor — the same shape the page's
/// JS re-renderer produces from `/api/status`.
fn monitor_rows(monitors: &[crate::state::MonitorStatus]) -> String {
    use std::fmt::Write as _;
    monitors
        .iter()
        .fold(String::new(), |mut acc, m| {
            let (color, bg) = match m.state {
                crate::monitor::MonitorState::Safe => ("#155724", "#d4edda"),
                crate::monitor::MonitorState::Unsafe => ("#721c24", "#f8d7da"),
                crate::monitor::MonitorState::Unknown => ("#383d41", "#e2e3e5"),
            };
            let last_check = if m.last_poll_epoch_ms == 0 {
                "Never".to_string()
            } else {
                format!(
                    r"<script>document.write(new Date({}).toLocaleTimeString())</script>",
                    m.last_poll_epoch_ms
                )
            };
            let next_check = if m.last_poll_epoch_ms == 0 {
                "Pending".to_string()
            } else {
                format!(
                    r"<script>document.write(new Date({}).toLocaleTimeString())</script>",
                    m.last_poll_epoch_ms
                        .saturating_add(duration_to_ms_for_js(m.polling_interval))
                )
            };
            let _ = write!(
                acc,
                r#"<tr style="border-bottom: 1px solid #dee2e6;">
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">
                        <span style="display: inline-block; padding: 0.25em 0.6em; border-radius: 0.25rem; font-size: 0.85em; font-weight: 600; color: {}; background-color: {};">{}</span>
                    </td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                </tr>"#,
                html_escape(&m.name),
                color,
                bg,
                m.state,
                m.consecutive_errors,
                last_check,
                next_check
            );
            acc
        })
}

/// One server-rendered `<tr>` per discovered service — the same shape
/// the page's JS re-renderer produces from `/api/services`.
fn service_rows(services: &[crate::state::ServiceHealthStatus]) -> String {
    use std::fmt::Write as _;
    services
        .iter()
        .fold(String::new(), |mut acc, s| {
            let (color, bg) = match s.health {
                crate::state::ServiceHealth::Up => ("#155724", "#d4edda"),
                crate::state::ServiceHealth::Degraded => ("#856404", "#fff3cd"),
                crate::state::ServiceHealth::Down => ("#721c24", "#f8d7da"),
                crate::state::ServiceHealth::Unknown => ("#383d41", "#e2e3e5"),
            };
            // The degraded service's own words, passed through opaquely
            // (escaped, never interpreted) under the amber badge.
            let health_message = s.health_message.as_ref().map_or_else(String::new, |m| {
                format!(
                    r#"<br><small style="color: #856404;">{}</small>"#,
                    html_escape(m)
                )
            });
            let last_probe = if s.last_probe_epoch_ms == 0 {
                "Never".to_string()
            } else {
                format!(
                    r"<script>document.write(new Date({}).toLocaleTimeString())</script>",
                    s.last_probe_epoch_ms
                )
            };
            let next_restart = s.next_restart_epoch_ms.map_or_else(
                || "&mdash;".to_string(),
                |at| {
                    format!(
                        r"<script>document.write(new Date({at}).toLocaleTimeString())</script>"
                    )
                },
            );
            let run_state = serde_json::to_value(s.run_state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let _ = write!(
                acc,
                r#"<tr style="border-bottom: 1px solid #dee2e6;">
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">
                        <span style="display: inline-block; padding: 0.25em 0.6em; border-radius: 0.25rem; font-size: 0.85em; font-weight: 600; color: {}; background-color: {};">{}</span>{}
                    </td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                </tr>"#,
                html_escape(&s.name),
                html_escape(&run_state),
                color,
                bg,
                s.health,
                health_message,
                s.consecutive_failures,
                s.restarts_in_outage,
                s.total_restarts,
                last_probe,
                next_restart
            );
            acc
        })
}

/// One server-rendered `<tr>` per notification, newest first — the
/// same shape the page's JS re-renderer produces from `/api/history`.
fn history_rows(
    history: &std::collections::VecDeque<crate::notifier::NotificationRecord>,
) -> String {
    use std::fmt::Write as _;
    history.iter().rev().fold(String::new(), |mut acc, h| {
        let status = if h.success { "OK" } else { "Failed" };
        let _ = write!(
            acc,
            r#"<tr style="border-bottom: 1px solid #dee2e6;">
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                    <td style="padding: 0.5rem;">{}</td>
                </tr>"#,
            html_escape(&h.monitor_name),
            html_escape(&h.message),
            html_escape(&h.notifier_type),
            status
        );
        acc
    })
}

/// The dashboard page scaffold. `%monitor_rows%` / `%service_rows%` /
/// `%history_rows%` are substitution placeholders filled by
/// `render_dashboard`; everything else (including the JS re-renderers'
/// `${...}` template literals) is verbatim.
const DASHBOARD_PAGE: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Sentinel Dashboard</title>
    <script>
        function esc(v) {
            return String(v).replace(/[&<>"']/g, c => ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'})[c]);
        }
        function refreshData() {
            fetch('/api/status')
                .then(r => r.json())
                .then(data => {
                    const tbody = document.getElementById('monitor-body');
                    tbody.innerHTML = data.map(m => {
                        const colors = {
                            'Safe': ['#155724', '#d4edda'],
                            'Unsafe': ['#721c24', '#f8d7da'],
                        };
                        const [color, bg] = colors[m.state] || ['#383d41', '#e2e3e5'];
                        const lastCheck = m.last_poll_epoch_ms === 0 ? 'Never' : new Date(m.last_poll_epoch_ms).toLocaleTimeString();
                        const nextCheck = m.last_poll_epoch_ms === 0 ? 'Pending' : new Date(m.last_poll_epoch_ms + m.polling_interval_ms).toLocaleTimeString();
                        return `<tr style="border-bottom: 1px solid #dee2e6;">
                            <td style="padding: 0.5rem;">${esc(m.name)}</td>
                            <td style="padding: 0.5rem;">
                                <span style="display: inline-block; padding: 0.25em 0.6em; border-radius: 0.25rem; font-size: 0.85em; font-weight: 600; color: ${color}; background-color: ${bg};">${m.state}</span>
                            </td>
                            <td style="padding: 0.5rem;">${m.consecutive_errors}</td>
                            <td style="padding: 0.5rem;">${lastCheck}</td>
                            <td style="padding: 0.5rem;">${nextCheck}</td>
                        </tr>`;
                    }).join('');
                });
            fetch('/api/services')
                .then(r => r.json())
                .then(data => {
                    const tbody = document.getElementById('service-body');
                    tbody.innerHTML = data.map(s => {
                        const colors = {
                            'up': ['#155724', '#d4edda'],
                            'degraded': ['#856404', '#fff3cd'],
                            'down': ['#721c24', '#f8d7da'],
                        };
                        const [color, bg] = colors[s.health] || ['#383d41', '#e2e3e5'];
                        const label = s.health.charAt(0).toUpperCase() + s.health.slice(1);
                        const message = s.health_message ? `<br><small style="color: #856404;">${esc(s.health_message)}</small>` : '';
                        const lastProbe = s.last_probe_epoch_ms === 0 ? 'Never' : new Date(s.last_probe_epoch_ms).toLocaleTimeString();
                        const nextRestart = s.next_restart_epoch_ms === null ? '—' : new Date(s.next_restart_epoch_ms).toLocaleTimeString();
                        return `<tr style="border-bottom: 1px solid #dee2e6;">
                            <td style="padding: 0.5rem;">${esc(s.name)}</td>
                            <td style="padding: 0.5rem;">${esc(s.run_state)}</td>
                            <td style="padding: 0.5rem;">
                                <span style="display: inline-block; padding: 0.25em 0.6em; border-radius: 0.25rem; font-size: 0.85em; font-weight: 600; color: ${color}; background-color: ${bg};">${label}</span>${message}
                            </td>
                            <td style="padding: 0.5rem;">${s.consecutive_failures}</td>
                            <td style="padding: 0.5rem;">${s.restarts_in_outage}</td>
                            <td style="padding: 0.5rem;">${s.total_restarts}</td>
                            <td style="padding: 0.5rem;">${lastProbe}</td>
                            <td style="padding: 0.5rem;">${nextRestart}</td>
                        </tr>`;
                    }).join('');
                });
            fetch('/api/history')
                .then(r => r.json())
                .then(data => {
                    const tbody = document.getElementById('history-body');
                    tbody.innerHTML = data.reverse().map(h => {
                        const status = h.success ? 'OK' : 'Failed';
                        return `<tr style="border-bottom: 1px solid #dee2e6;">
                            <td style="padding: 0.5rem;">${esc(h.monitor_name)}</td>
                            <td style="padding: 0.5rem;">${esc(h.message)}</td>
                            <td style="padding: 0.5rem;">${esc(h.notifier_type)}</td>
                            <td style="padding: 0.5rem;">${status}</td>
                        </tr>`;
                    }).join('');
                });
        }
        setInterval(refreshData, 5000);
    </script>
</head>
<body style="font-family: system-ui, sans-serif; max-width: 960px; margin: 0 auto; padding: 1rem;">
    <h1>Sentinel Dashboard</h1>
    <section>
        <h2>Monitors</h2>
        <table style="width: 100%; border-collapse: collapse;">
            <thead>
                <tr style="border-bottom: 2px solid #dee2e6;">
                    <th style="padding: 0.5rem; text-align: left;">Name</th>
                    <th style="padding: 0.5rem; text-align: left;">State</th>
                    <th style="padding: 0.5rem; text-align: left;">Errors</th>
                    <th style="padding: 0.5rem; text-align: left;">Last Check</th>
                    <th style="padding: 0.5rem; text-align: left;">Next Check</th>
                </tr>
            </thead>
            <tbody id="monitor-body">%monitor_rows%</tbody>
        </table>
    </section>
    <section>
        <h2>Discovered Services</h2>
        <table style="width: 100%; border-collapse: collapse;">
            <thead>
                <tr style="border-bottom: 2px solid #dee2e6;">
                    <th style="padding: 0.5rem; text-align: left;">Name</th>
                    <th style="padding: 0.5rem; text-align: left;">State</th>
                    <th style="padding: 0.5rem; text-align: left;">Health</th>
                    <th style="padding: 0.5rem; text-align: left;">Failures</th>
                    <th style="padding: 0.5rem; text-align: left;">Restarts (Outage)</th>
                    <th style="padding: 0.5rem; text-align: left;">Restarts (Total)</th>
                    <th style="padding: 0.5rem; text-align: left;">Last Probe</th>
                    <th style="padding: 0.5rem; text-align: left;">Next Restart</th>
                </tr>
            </thead>
            <tbody id="service-body">%service_rows%</tbody>
        </table>
    </section>
    <section>
        <h2>Notification History</h2>
        <table style="width: 100%; border-collapse: collapse;">
            <thead>
                <tr style="border-bottom: 2px solid #dee2e6;">
                    <th style="padding: 0.5rem; text-align: left;">Monitor</th>
                    <th style="padding: 0.5rem; text-align: left;">Message</th>
                    <th style="padding: 0.5rem; text-align: left;">Notifier</th>
                    <th style="padding: 0.5rem; text-align: left;">Status</th>
                </tr>
            </thead>
            <tbody id="history-body">%history_rows%</tbody>
        </table>
    </section>
</body>
</html>"#;

async fn status_handler(State(dashboard): State<DashboardState>) -> impl IntoResponse {
    let state = dashboard.state.read().await;

    let statuses: Vec<serde_json::Value> = state
        .monitors
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "state": format!("{}", m.state),
                "last_poll_epoch_ms": m.last_poll_epoch_ms,
                "last_change_epoch_ms": m.last_change_epoch_ms,
                "consecutive_errors": m.consecutive_errors,
                // Integer ms on the wire — the dashboard JS does
                // `new Date(last_poll_epoch_ms + polling_interval_ms)` arithmetic.
                // Internally the field is a `Duration`; flatten only at this boundary.
                "polling_interval_ms": duration_to_ms_for_js(m.polling_interval),
            })
        })
        .collect();

    axum::Json(statuses)
}

/// `GET /api/services`: one entry per discovered service (populated by the
/// discovery loop, so entries exist before their first probe). Empty array
/// when nothing is discovered.
async fn services_handler(State(dashboard): State<DashboardState>) -> impl IntoResponse {
    let state = dashboard.state.read().await;

    let services: Vec<serde_json::Value> = state
        .services
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "unit": s.unit,
                // "running" | "failed" | "inert" | "stopped" | "disabled".
                "run_state": s.run_state,
                // "unknown" | "up" | "degraded" | "down" (the enum's
                // lowercase serde form).
                "health": s.health,
                // Opaque display text from a degraded probe's 503 body;
                // null unless health is "degraded".
                "health_message": s.health_message,
                "last_probe_epoch_ms": s.last_probe_epoch_ms,
                "consecutive_failures": s.consecutive_failures,
                "restarts_in_outage": s.restarts_in_outage,
                "total_restarts": s.total_restarts,
                "next_restart_epoch_ms": s.next_restart_epoch_ms,
                // The service's listening port from its config's `server`
                // block; null when discovery could not derive a probe.
                "probe_port": s.probe_port,
                // Integer ms on the wire, like polling_interval_ms above.
                "poll_interval_ms": duration_to_ms_for_js(s.poll_interval),
            })
        })
        .collect();

    axum::Json(services)
}

async fn history_handler(State(dashboard): State<DashboardState>) -> impl IntoResponse {
    let state = dashboard.state.read().await;

    let history: Vec<serde_json::Value> = state
        .history
        .iter()
        .map(|h| {
            serde_json::json!({
                "monitor_name": h.monitor_name,
                "notifier_type": h.notifier_type,
                "message": h.message,
                "success": h.success,
                "error": h.error,
                "timestamp_epoch_ms": h.timestamp_epoch_ms,
            })
        })
        .collect();

    axum::Json(history)
}

/// `POST /api/services/{name}/restart`: run the discovered service's derived
/// restart command (and recovery poll). The command's outcome is a domain
/// result on HTTP 200; addressing errors map to 404 (no discovered service by
/// that name) or 409 (already in flight).
async fn restart_handler(
    State(dashboard): State<DashboardState>,
    Path(name): Path<String>,
) -> Response {
    match dashboard.restarts.restart(&name).await {
        Ok(report) => (StatusCode::OK, axum::Json(report)).into_response(),
        Err(e) => {
            let code = match e {
                RestartError::UnknownService(_) => StatusCode::NOT_FOUND,
                RestartError::AlreadyInFlight(_) => StatusCode::CONFLICT,
            };
            (
                code,
                axum::Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

async fn health_handler() -> impl IntoResponse {
    "OK"
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::monitor::MonitorState;
    use crate::notifier::NotificationRecord;
    use crate::state::new_state_handle;

    use crate::discovery::{DiscoveredService, DiscoveredUnit, RunState, ServiceManager};

    /// A [`ServiceManager`] that accepts every restart — the restart-handler
    /// tests only assert the HTTP mapping, not the platform.
    #[derive(Debug)]
    struct AlwaysOkManager;

    #[async_trait::async_trait]
    impl ServiceManager for AlwaysOkManager {
        async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
            Ok(Vec::new())
        }

        async fn restart(&self, _unit: &str, _budget: Duration) -> crate::Result<()> {
            Ok(())
        }

        async fn recovery_check(&self, _unit: &str) -> Option<bool> {
            None
        }
    }

    fn restart_manager(services: &[&str]) -> Arc<RestartManager> {
        let registry: std::collections::HashMap<String, DiscoveredService> = services
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    DiscoveredService {
                        name: n.to_string(),
                        unit: format!("rusty-photon-{n}"),
                        state: RunState::Running,
                        probe: None,
                    },
                )
            })
            .collect();
        Arc::new(RestartManager::new(
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::new(AlwaysOkManager),
            Duration::from_secs(1),
        ))
    }

    /// Router with an empty discovered-services registry (non-restart tests).
    fn router(state: StateHandle) -> Router {
        build_router(state, restart_manager(&[]))
    }

    #[test]
    fn duration_to_ms_zero_stays_zero() {
        assert_eq!(duration_to_ms_for_js(Duration::ZERO), 0);
    }

    #[test]
    fn duration_to_ms_sub_millisecond_rounds_up_to_one() {
        assert_eq!(duration_to_ms_for_js(Duration::from_micros(500)), 1);
        assert_eq!(duration_to_ms_for_js(Duration::from_nanos(1)), 1);
    }

    #[test]
    fn duration_to_ms_normal_millis_unchanged() {
        assert_eq!(duration_to_ms_for_js(Duration::from_secs(30)), 30_000);
        assert_eq!(duration_to_ms_for_js(Duration::from_mins(1)), 60_000);
    }

    #[test]
    fn duration_to_ms_saturates_on_overflow() {
        // Duration::MAX is ~5.85e11 years — far beyond u64 ms (~5.85e8 years).
        assert_eq!(duration_to_ms_for_js(Duration::MAX), u64::MAX);
    }

    #[test]
    fn html_escape_neutralizes_markup() {
        assert_eq!(
            html_escape(r#"<img src=x onerror="alert('&')">"#),
            "&lt;img src=x onerror=&quot;alert(&#39;&amp;&#39;)&quot;&gt;"
        );
        assert_eq!(html_escape("plate-solver"), "plate-solver");
    }

    #[tokio::test]
    async fn index_escapes_names_and_messages() {
        let state = new_state_handle(
            vec![("<b>mon</b>".to_string(), Duration::from_secs(30))],
            10,
        );
        {
            let mut s = state.write().await;
            s.set_service_health(crate::state::ServiceHealthStatus::unknown(
                "<script>svc</script>".to_string(),
                "rusty-photon-svc".to_string(),
                RunState::Running,
                None,
                Duration::from_secs(30),
            ));
            s.add_notification(NotificationRecord {
                monitor_name: "<script>svc</script>".to_string(),
                notifier_type: "pushover".to_string(),
                message: "restarted <autonomously>".to_string(),
                success: true,
                error: None,
                timestamp_epoch_ms: 1000,
            });
        }
        let app = router(state);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<b>mon</b>"), "monitor name not escaped");
        assert!(
            !html.contains("<script>svc</script>"),
            "service name not escaped"
        );
        assert!(
            !html.contains("restarted <autonomously>"),
            "history message not escaped"
        );
        assert!(html.contains("&lt;b&gt;mon&lt;/b&gt;"));
        assert!(html.contains("&lt;script&gt;svc&lt;/script&gt;"));
        assert!(html.contains("restarted &lt;autonomously&gt;"));
    }

    fn setup_state() -> StateHandle {
        new_state_handle(
            vec![(
                "Test Monitor".to_string(),
                std::time::Duration::from_secs(30),
            )],
            10,
        )
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let state = setup_state();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_returns_json() {
        let state = setup_state();
        {
            let mut s = state.write().await;
            s.update_monitor("Test Monitor", MonitorState::Safe, 1000);
        }
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["name"], "Test Monitor");
        assert_eq!(json[0]["state"], "Safe");
        assert_eq!(json[0]["polling_interval_ms"], 30000);
    }

    #[tokio::test]
    async fn history_returns_json() {
        let state = setup_state();
        {
            let mut s = state.write().await;
            s.add_notification(NotificationRecord {
                monitor_name: "Test Monitor".to_string(),
                notifier_type: "pushover".to_string(),
                message: "alert".to_string(),
                success: true,
                error: None,
                timestamp_epoch_ms: 1000,
            });
        }
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["monitor_name"], "Test Monitor");
        assert!(json[0]["success"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn index_returns_html() {
        let state = setup_state();
        let app = router(state);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Sentinel Dashboard"));
        assert!(html.contains("Last Check"));
        assert!(html.contains("Next Check"));
        assert!(html.contains("Discovered Services"));
        assert!(html.contains("Next Restart"));
    }

    #[tokio::test]
    async fn services_returns_json() {
        let state = new_state_handle(vec![], 10);
        {
            let mut s = state.write().await;
            s.set_service_health(crate::state::ServiceHealthStatus {
                name: "plate-solver".to_string(),
                unit: "rusty-photon-plate-solver".to_string(),
                run_state: RunState::Running,
                health: crate::state::ServiceHealth::Up,
                health_message: None,
                last_probe_epoch_ms: 1000,
                consecutive_failures: 0,
                restarts_in_outage: 0,
                total_restarts: 3,
                next_restart_epoch_ms: None,
                probe_port: Some(11131),
                poll_interval: Duration::from_secs(30),
            });
        }
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/services")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["name"], "plate-solver");
        assert_eq!(json[0]["unit"], "rusty-photon-plate-solver");
        assert_eq!(json[0]["run_state"], "running");
        assert_eq!(json[0]["health"], "up");
        assert_eq!(json[0]["health_message"], serde_json::Value::Null);
        assert_eq!(json[0]["last_probe_epoch_ms"], 1000);
        assert_eq!(json[0]["consecutive_failures"], 0);
        assert_eq!(json[0]["restarts_in_outage"], 0);
        assert_eq!(json[0]["total_restarts"], 3);
        assert_eq!(json[0]["next_restart_epoch_ms"], serde_json::Value::Null);
        assert_eq!(json[0]["probe_port"], 11131);
        assert_eq!(json[0]["poll_interval_ms"], 30000);
    }

    #[tokio::test]
    async fn services_empty_without_supervision() {
        let state = setup_state();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/services")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(json.is_empty());
    }

    fn restart_router(services: &[&str]) -> Router {
        build_router(setup_state(), restart_manager(services))
    }

    async fn post_restart(app: Router, name: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/services/{name}/restart"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn restart_unknown_service_is_404() {
        let app = restart_router(&[]);
        let (status, body) = post_restart(app, "nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("no discovered service named 'nope'"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn restart_ok_reports_domain_outcome_on_200() {
        let app = restart_router(&["svc"]);
        let (status, body) = post_restart(app, "svc").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], "svc");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["recovery"], "skipped");
    }

    #[tokio::test]
    async fn status_empty_monitors() {
        let state = new_state_handle(vec![], 10);
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(json.is_empty());
    }
}

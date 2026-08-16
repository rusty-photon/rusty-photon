//! The capability probe behind the equipment page's managed/foreign tier.
//!
//! One probe per roster device, against the device's own Alpaca server
//! (`docs/services/ui-htmx.md` "Equipment page"): read
//! `GET /api/v1/{type}/{n}/supportedactions`; `config.get` present ⇒ the device
//! is **managed** (it speaks the config-actions protocol and gets a
//! roster-derived config page). Otherwise a reachable
//! `GET /setup/v1/{type}/{n}/setup` ⇒ **setup page**; else **control only**.
//! A 401/403 ⇒ **auth required** (the BFF probes without credentials — rp
//! redacts per-device auth); a transport error or timeout ⇒ **unreachable**.
//! Because `config.*` is self-advertising, any third-party server adopting the
//! convention auto-upgrades to managed — this probe *is* the capability
//! detection, not a hardcoded table.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

/// The capability tier the equipment page renders per device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    /// Speaks `config.*` — gets a "Configure" link (`/config/rp:{kind}:{id}`).
    Managed,
    /// No `config.*`, but serves the standard Alpaca setup page at this URL.
    SetupPage(String),
    /// Reachable, but neither `config.*` nor a setup page.
    ControlOnly,
    /// The device's server answered 401/403 to the credential-less probe.
    AuthRequired,
    /// The device's server did not answer (down, or timed out).
    Unreachable,
}

/// Bounded HTTP GET for probing — its own seam (not [`crate::io::HttpClient`])
/// because probes need a short per-request timeout: a roster full of powered-off
/// devices must not stall the page render.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait ProbeHttp: Send + Sync {
    /// GET `url` with the probe timeout. `Ok((status, body))` on any HTTP
    /// response; `Err` on connect failure / timeout.
    async fn get(&self, url: &str) -> Result<(u16, String), String>;
}

/// Per-probe request timeout. Probes run concurrently, so a page render is
/// bounded by roughly one timeout, not the roster size.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Production [`ProbeHttp`]: a `reqwest` client with rusty-photon-tls CA trust (an
/// observatory runs one CA, so the rp target's CA also vouches for the
/// devices) and a per-request timeout.
pub struct ReqwestProbeHttp {
    client: reqwest::Client,
}

impl ReqwestProbeHttp {
    pub fn new(ca_cert_path: Option<&std::path::Path>) -> Result<Self, String> {
        let client = rusty_photon_tls::client::build_reqwest_client(ca_cert_path)
            .map_err(|e| format!("failed to build probe HTTP client: {e}"))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl ProbeHttp for ReqwestProbeHttp {
    async fn get(&self, url: &str) -> Result<(u16, String), String> {
        tracing::debug!("probe GET {url}");
        let response = self
            .client
            .get(url)
            .timeout(PROBE_TIMEOUT)
            .header(reqwest::header::CONNECTION, "close")
            .send()
            .await
            .map_err(|e| format!("probe GET {url} failed: {e}"))?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        tracing::debug!("probe GET {url} -> {status}");
        Ok((status, body))
    }
}

/// Probe one device and classify its tier. `base_url` is the device's Alpaca
/// server; `device_type`/`device_number` address the device on it.
pub async fn probe_device(
    http: &Arc<dyn ProbeHttp>,
    base_url: &str,
    device_type: &str,
    device_number: u32,
) -> Tier {
    let base = base_url.trim_end_matches('/');
    let actions_url = format!("{base}/api/v1/{device_type}/{device_number}/supportedactions");
    let setup_url = format!("{base}/setup/v1/{device_type}/{device_number}/setup");

    let Ok((status, body)) = http.get(&actions_url).await else {
        return Tier::Unreachable;
    };
    match status {
        200..=299 if supports_config_actions(&body) => return Tier::Managed,
        401 | 403 => return Tier::AuthRequired,
        // Any other status (404 on a device number that doesn't exist, 500, …,
        // or a 2xx without config actions): not managed; fall through to the
        // setup-page check.
        _ => {}
    }
    match http.get(&setup_url).await {
        Ok((200..=299, _)) => Tier::SetupPage(setup_url),
        _ => Tier::ControlOnly,
    }
}

/// Whether a `supportedactions` response body advertises the config actions.
/// The body is the standard Alpaca envelope with a `Value` string array.
fn supports_config_actions(body: &str) -> bool {
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    envelope
        .get("Value")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|actions| {
            actions
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|a| a == "config.get")
        })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn actions_envelope(actions: &[&str]) -> String {
        json!({ "Value": actions, "ErrorNumber": 0, "ErrorMessage": "" }).to_string()
    }

    /// One canned probe response: (URL substring to match, the result).
    type CannedResponse = (&'static str, Result<(u16, String), String>);

    fn probe_http(responses: Vec<CannedResponse>) -> Arc<dyn ProbeHttp> {
        let mut http = MockProbeHttp::new();
        for (url_contains, result) in responses {
            http.expect_get()
                .withf(move |url| url.contains(url_contains))
                .returning(move |_| {
                    let result = result.clone();
                    Box::pin(async move { result })
                });
        }
        Arc::new(http)
    }

    #[tokio::test]
    async fn config_actions_in_supportedactions_is_managed() {
        let http = probe_http(vec![(
            "/supportedactions",
            Ok((
                200,
                actions_envelope(&["config.get", "config.apply", "config.schema"]),
            )),
        )]);
        let tier = probe_device(&http, "http://dev:11119/", "covercalibrator", 0).await;
        assert_eq!(tier, Tier::Managed);
    }

    #[tokio::test]
    async fn no_config_actions_but_setup_page_is_setup_page() {
        let http = probe_http(vec![
            ("/supportedactions", Ok((200, actions_envelope(&["fanon"])))),
            ("/setup", Ok((200, "<html>".to_string()))),
        ]);
        let tier = probe_device(&http, "http://dev:11119", "focuser", 0).await;
        assert_eq!(
            tier,
            Tier::SetupPage("http://dev:11119/setup/v1/focuser/0/setup".to_string())
        );
    }

    #[tokio::test]
    async fn no_config_actions_and_no_setup_page_is_control_only() {
        let http = probe_http(vec![
            ("/supportedactions", Ok((200, actions_envelope(&[])))),
            ("/setup", Ok((404, String::new()))),
        ]);
        let tier = probe_device(&http, "http://dev:11119", "camera", 0).await;
        assert_eq!(tier, Tier::ControlOnly);
    }

    #[tokio::test]
    async fn unauthorized_probe_is_auth_required() {
        let http = probe_http(vec![("/supportedactions", Ok((401, String::new())))]);
        let tier = probe_device(&http, "http://dev:11119", "camera", 0).await;
        assert_eq!(tier, Tier::AuthRequired);
    }

    #[tokio::test]
    async fn connect_failure_is_unreachable() {
        let http = probe_http(vec![(
            "/supportedactions",
            Err("connection refused".to_string()),
        )]);
        let tier = probe_device(&http, "http://dev:11119", "camera", 0).await;
        assert_eq!(tier, Tier::Unreachable);
    }

    #[tokio::test]
    async fn malformed_envelope_falls_through_to_setup_probe() {
        let http = probe_http(vec![
            ("/supportedactions", Ok((200, "not json".to_string()))),
            ("/setup", Ok((500, String::new()))),
        ]);
        let tier = probe_device(&http, "http://dev:11119", "camera", 0).await;
        assert_eq!(tier, Tier::ControlOnly);
    }

    #[tokio::test]
    async fn probe_urls_are_well_formed() {
        // Pin the exact probe URLs — they are the ASCOM Alpaca contract.
        let mut http = MockProbeHttp::new();
        http.expect_get()
            .withf(|url| url == "http://dev:11119/api/v1/telescope/2/supportedactions")
            .returning(|_| Box::pin(async { Ok((200, actions_envelope(&["config.get"]))) }));
        let http: Arc<dyn ProbeHttp> = Arc::new(http);
        let tier = probe_device(&http, "http://dev:11119/", "telescope", 2).await;
        assert_eq!(tier, Tier::Managed);
    }
}

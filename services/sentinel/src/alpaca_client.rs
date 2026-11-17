//! ASCOM Alpaca `SafetyMonitor` client

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::MonitorConfig;
use crate::io::HttpClient;
use crate::monitor::{Monitor, MonitorState};

/// The longest sentinel waits for one request to a monitor's device. A LAN
/// Alpaca server answers in milliseconds; the bound keeps a host that drops
/// the request, or a server that accepts the connection and never answers,
/// from holding the poll loop, or start-up, which awaits each connect.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// ASCOM Alpaca API response for boolean values
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AlpacaBoolResponse {
    value: bool,
    #[allow(dead_code)]
    error_number: i32,
    #[allow(dead_code)]
    error_message: String,
}

/// The error fields every Alpaca answer carries
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AlpacaErrorFields {
    error_number: i32,
    error_message: String,
}

/// Client for an ASCOM Alpaca `SafetyMonitor` device
#[derive(derive_more::Debug)]
pub struct AlpacaSafetyMonitor {
    name: String,
    base_url: String,
    #[debug(skip)]
    polling_interval: Duration,
    #[debug(skip)]
    http: Arc<dyn HttpClient>,
}

impl AlpacaSafetyMonitor {
    pub fn new(config: &MonitorConfig, http: Arc<dyn HttpClient>) -> Self {
        let MonitorConfig::AlpacaSafetyMonitor {
            name,
            host,
            port,
            device_number,
            polling_interval,
            scheme,
            auth: _,
        } = config;

        let base_url = format!("{scheme}://{host}:{port}/api/v1/safetymonitor/{device_number}");

        tracing::debug!("Created AlpacaSafetyMonitor '{}' at {}", name, base_url);

        Self {
            name: name.clone(),
            base_url,
            polling_interval: *polling_interval,
            http,
        }
    }

    /// PUT the device's `Connected` property, bounded by [`REQUEST_TIMEOUT`].
    /// A non-200 status or an ASCOM error in the answer is a failure: the
    /// device refused.
    async fn put_connected(&self, connected: &str) -> crate::Result<()> {
        let url = format!("{}/connected", self.base_url);
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.http
                .put_form(&url, &[("Connected", connected), ("ClientID", "1")]),
        )
        .await
        .map_err(|_| {
            crate::SentinelError::Http(format!("PUT {url} timed out after {REQUEST_TIMEOUT:?}"))
        })??;
        if response.status != 200 {
            return Err(crate::SentinelError::Monitor(format!(
                "PUT {url} answered status {}",
                response.status
            )));
        }
        let answer: AlpacaErrorFields = serde_json::from_str(&response.body).map_err(|e| {
            crate::SentinelError::Monitor(format!("PUT {url} answered an unreadable body: {e}"))
        })?;
        if answer.error_number != 0 {
            return Err(crate::SentinelError::Monitor(format!(
                "PUT {url} refused: ASCOM error {} - {}",
                answer.error_number, answer.error_message
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl Monitor for AlpacaSafetyMonitor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn poll(&self) -> MonitorState {
        let url = format!("{}/issafe", self.base_url);
        tracing::debug!("Polling {} at {}", self.name, url);

        let Ok(response) = tokio::time::timeout(REQUEST_TIMEOUT, self.http.get(&url)).await else {
            tracing::debug!(
                "Polling {} timed out after {:?}",
                self.name,
                REQUEST_TIMEOUT
            );
            return MonitorState::Unknown;
        };
        match response {
            Ok(response) => {
                if response.status != 200 {
                    tracing::debug!(
                        "Non-200 response from {}: status={}",
                        self.name,
                        response.status
                    );
                    return MonitorState::Unknown;
                }

                match serde_json::from_str::<AlpacaBoolResponse>(&response.body) {
                    Ok(parsed) => {
                        if parsed.error_number != 0 {
                            tracing::debug!(
                                "ASCOM error from {}: {} - {}",
                                self.name,
                                parsed.error_number,
                                parsed.error_message
                            );
                            return MonitorState::Unknown;
                        }
                        if parsed.value {
                            MonitorState::Safe
                        } else {
                            MonitorState::Unsafe
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Failed to parse response from {}: {}", self.name, e);
                        MonitorState::Unknown
                    }
                }
            }
            Err(e) => {
                tracing::debug!("Failed to poll {}: {}", self.name, e);
                MonitorState::Unknown
            }
        }
    }

    async fn connect(&self) -> crate::Result<()> {
        tracing::debug!("Connecting {}", self.name);
        self.put_connected("true").await?;
        tracing::debug!("Connected {}", self.name);
        Ok(())
    }

    async fn disconnect(&self) -> crate::Result<()> {
        tracing::debug!("Disconnecting {}", self.name);
        self.put_connected("false").await?;
        tracing::debug!("Disconnected {}", self.name);
        Ok(())
    }

    fn polling_interval(&self) -> Duration {
        self.polling_interval
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::io::HttpResponse;
    use crate::io::MockHttpClient;

    fn test_config() -> MonitorConfig {
        MonitorConfig::AlpacaSafetyMonitor {
            name: "Test Monitor".to_string(),
            host: "localhost".to_string(),
            port: 11111,
            device_number: 0,
            polling_interval: Duration::from_secs(30),
            scheme: "http".to_string(),
            auth: None,
        }
    }

    fn safe_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"Value": true, "ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
        }
    }

    fn unsafe_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"Value": false, "ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
        }
    }

    fn ascom_error_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"Value": false, "ErrorNumber": 1031, "ErrorMessage": "Not connected"}"#
                .to_string(),
        }
    }

    #[tokio::test]
    async fn poll_returns_safe() {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .withf(|url| url.contains("/issafe"))
            .returning(|_| Box::pin(async { Ok(safe_response()) }));

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Safe);
    }

    #[tokio::test]
    async fn poll_returns_unsafe() {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .withf(|url| url.contains("/issafe"))
            .returning(|_| Box::pin(async { Ok(unsafe_response()) }));

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Unsafe);
    }

    #[tokio::test]
    async fn poll_returns_unknown_on_http_error() {
        let mut mock = MockHttpClient::new();
        mock.expect_get().returning(|_| {
            Box::pin(async { Err(crate::SentinelError::Http("connection refused".to_string())) })
        });

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Unknown);
    }

    #[tokio::test]
    async fn poll_returns_unknown_on_non_200() {
        let mut mock = MockHttpClient::new();
        mock.expect_get().returning(|_| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 500,
                    body: "Internal Server Error".to_string(),
                })
            })
        });

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Unknown);
    }

    #[tokio::test]
    async fn poll_returns_unknown_on_ascom_error() {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .returning(|_| Box::pin(async { Ok(ascom_error_response()) }));

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Unknown);
    }

    #[tokio::test]
    async fn poll_returns_unknown_on_invalid_json() {
        let mut mock = MockHttpClient::new();
        mock.expect_get().returning(|_| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 200,
                    body: "not json".to_string(),
                })
            })
        });

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        let state = monitor.poll().await;
        assert_eq!(state, MonitorState::Unknown);
    }

    #[tokio::test]
    async fn connect_sends_put() {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form()
            .withf(|url, params| {
                url.contains("/connected") && params.contains(&("Connected", "true"))
            })
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
                    })
                })
            });

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        monitor.connect().await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_sends_put() {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form()
            .withf(|url, params| {
                url.contains("/connected") && params.contains(&("Connected", "false"))
            })
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
                    })
                })
            });

        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        monitor.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn name_returns_configured_name() {
        let mock = MockHttpClient::new();
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));
        assert_eq!(monitor.name(), "Test Monitor");
    }

    fn put_answering(status: u16, body: &'static str) -> MockHttpClient {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form().returning(move |_, _| {
            Box::pin(async move {
                Ok(HttpResponse {
                    status,
                    body: body.to_string(),
                })
            })
        });
        mock
    }

    #[tokio::test]
    async fn a_connect_answered_with_an_error_status_fails() {
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(put_answering(500, "")));

        let err = monitor.connect().await.unwrap_err();

        assert!(err.to_string().contains("answered status 500"), "{err}");
    }

    #[tokio::test]
    async fn a_connect_answered_with_an_ascom_error_fails() {
        let mock = put_answering(
            200,
            r#"{"ClientTransactionID":1,"ServerTransactionID":2,"ErrorNumber":1280,"ErrorMessage":"Cannot read the monitored file"}"#,
        );
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(mock));

        let err = monitor.connect().await.unwrap_err();

        assert!(
            err.to_string()
                .contains("refused: ASCOM error 1280 - Cannot read the monitored file"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_disconnect_answered_with_an_unreadable_body_fails() {
        let monitor =
            AlpacaSafetyMonitor::new(&test_config(), Arc::new(put_answering(200, "not json")));

        let err = monitor.disconnect().await.unwrap_err();

        assert!(err.to_string().contains("unreadable body"), "{err}");
    }

    /// A device that never answers: every request stays pending.
    fn silent_device() -> MockHttpClient {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .returning(|_| Box::pin(std::future::pending()));
        mock.expect_put_form()
            .returning(|_, _| Box::pin(std::future::pending()));
        mock
    }

    #[tokio::test(start_paused = true)]
    async fn a_poll_the_device_does_not_answer_reads_unknown_at_the_bound() {
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(silent_device()));
        let start = tokio::time::Instant::now();

        let state = tokio::time::timeout(2 * REQUEST_TIMEOUT, monitor.poll())
            .await
            .expect("the poll outwaited its bound");

        assert_eq!(state, MonitorState::Unknown);
        assert_eq!(start.elapsed(), REQUEST_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connect_the_device_does_not_answer_fails_at_the_bound() {
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(silent_device()));
        let start = tokio::time::Instant::now();

        let err = tokio::time::timeout(2 * REQUEST_TIMEOUT, monitor.connect())
            .await
            .expect("the connect outwaited its bound")
            .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(start.elapsed(), REQUEST_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disconnect_the_device_does_not_answer_fails_at_the_bound() {
        let monitor = AlpacaSafetyMonitor::new(&test_config(), Arc::new(silent_device()));
        let start = tokio::time::Instant::now();

        let err = tokio::time::timeout(2 * REQUEST_TIMEOUT, monitor.disconnect())
            .await
            .expect("the disconnect outwaited its bound")
            .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(start.elapsed(), REQUEST_TIMEOUT);
    }
}

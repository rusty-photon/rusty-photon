//! Pushover notification client

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::NotifierConfig;
use crate::io::HttpClient;
use crate::notifier::{Notification, Notifier};

const PUSHOVER_API_URL: &str = "https://api.pushover.net/1/messages.json";

/// The longest a notification waits for the Pushover endpoint. Notifications
/// are sent from a monitor's poll loop, so an endpoint that accepts the
/// connection and stalls would otherwise stop the polling; 10 s leaves room
/// for a rig's uplink to the public API.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Pushover notification sender
#[derive(derive_more::Debug)]
pub struct PushoverNotifier {
    /// Never appear in Debug output: these are secrets.
    #[debug(skip)]
    api_token: String,
    #[debug(skip)]
    user_key: String,
    default_title: String,
    #[debug(skip)]
    default_priority: i8,
    #[debug(skip)]
    default_sound: String,
    /// API endpoint to POST to. Defaults to [`PUSHOVER_API_URL`]; overridden
    /// by the notifier config's `api_url` (stub server in tests, self-hosted relay).
    endpoint: String,
    #[debug(skip)]
    http: Arc<dyn HttpClient>,
}

impl PushoverNotifier {
    pub fn new(config: &NotifierConfig, http: Arc<dyn HttpClient>) -> Self {
        let NotifierConfig::Pushover {
            api_token,
            user_key,
            default_title,
            default_priority,
            default_sound,
            api_url,
        } = config;

        // A blank or whitespace-only override is a misconfiguration; treat it as
        // unset and fall back to the public endpoint rather than POSTing to an
        // empty URL (mirrors how empty env-var secrets are ignored in config.rs).
        let endpoint = match api_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => url.to_string(),
            Some(_) => {
                tracing::debug!("Pushover api_url is blank; using default endpoint");
                PUSHOVER_API_URL.to_string()
            }
            None => PUSHOVER_API_URL.to_string(),
        };

        tracing::debug!(
            "Created PushoverNotifier with title '{}' targeting {}",
            default_title,
            endpoint
        );

        Self {
            api_token: api_token.clone(),
            user_key: user_key.clone(),
            default_title: default_title.clone(),
            default_priority: *default_priority,
            default_sound: default_sound.clone(),
            endpoint,
            http,
        }
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    fn type_name(&self) -> &'static str {
        "pushover"
    }

    async fn notify(&self, notification: &Notification) -> crate::Result<()> {
        let title = if notification.title.is_empty() {
            &self.default_title
        } else {
            &notification.title
        };
        let priority = if notification.priority != 0 {
            notification.priority
        } else {
            self.default_priority
        };
        let sound = notification.sound.as_deref().unwrap_or(&self.default_sound);

        let priority_str = priority.to_string();
        let params = vec![
            ("token", self.api_token.as_str()),
            ("user", self.user_key.as_str()),
            ("title", title),
            ("message", notification.message.as_str()),
            ("priority", &priority_str),
            ("sound", sound),
        ];

        tracing::debug!(
            "Sending Pushover notification: title='{}', priority={}",
            title,
            priority
        );

        let response =
            tokio::time::timeout(NOTIFY_TIMEOUT, self.http.post_form(&self.endpoint, &params))
                .await
                .map_err(|_| {
                    crate::SentinelError::Notifier(format!(
                        "Pushover API did not answer within {NOTIFY_TIMEOUT:?}"
                    ))
                })??;

        if response.status != 200 {
            return Err(crate::SentinelError::Notifier(format!(
                "Pushover API returned status {}: {}",
                response.status, response.body
            )));
        }

        tracing::debug!("Pushover notification sent successfully");
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::io::{HttpResponse, MockHttpClient};

    fn config_with_url(api_url: Option<&str>) -> NotifierConfig {
        NotifierConfig::Pushover {
            api_token: "test-token".to_string(),
            user_key: "test-user".to_string(),
            default_title: "Test Alert".to_string(),
            default_priority: 0,
            default_sound: "pushover".to_string(),
            api_url: api_url.map(str::to_string),
        }
    }

    fn test_config() -> NotifierConfig {
        config_with_url(None)
    }

    /// A mock that asserts `post_form` is called with exactly `expected_url` and
    /// returns a 200, so the only thing under test is which endpoint is used.
    fn mock_expecting_url(expected_url: &'static str) -> MockHttpClient {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form()
            .withf(move |url, _params| url == expected_url)
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"status":1}"#.to_string(),
                    })
                })
            });
        mock
    }

    fn test_notification() -> Notification {
        Notification {
            title: "Alert".to_string(),
            message: "Something happened".to_string(),
            priority: 1,
            sound: Some("siren".to_string()),
        }
    }

    #[tokio::test]
    async fn sends_notification_with_correct_params() {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form()
            .withf(|url, params| {
                url == PUSHOVER_API_URL
                    && params.contains(&("token", "test-token"))
                    && params.contains(&("user", "test-user"))
                    && params.contains(&("title", "Alert"))
                    && params.contains(&("message", "Something happened"))
                    && params.contains(&("priority", "1"))
                    && params.contains(&("sound", "siren"))
            })
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"status":1}"#.to_string(),
                    })
                })
            });

        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        notifier.notify(&test_notification()).await.unwrap();
    }

    #[tokio::test]
    async fn posts_to_configured_api_url_override() {
        let mock = mock_expecting_url("https://relay.example/1/messages.json");
        let config = config_with_url(Some("https://relay.example/1/messages.json"));
        let notifier = PushoverNotifier::new(&config, Arc::new(mock));
        notifier.notify(&test_notification()).await.unwrap();
    }

    #[tokio::test]
    async fn blank_api_url_falls_back_to_default_endpoint() {
        let mock = mock_expecting_url(PUSHOVER_API_URL);
        // Whitespace-only override must be ignored, not POSTed to verbatim.
        let config = config_with_url(Some("   "));
        let notifier = PushoverNotifier::new(&config, Arc::new(mock));
        notifier.notify(&test_notification()).await.unwrap();
    }

    #[tokio::test]
    async fn api_url_override_is_trimmed() {
        let mock = mock_expecting_url("https://relay.example/msg");
        let config = config_with_url(Some("  https://relay.example/msg  "));
        let notifier = PushoverNotifier::new(&config, Arc::new(mock));
        notifier.notify(&test_notification()).await.unwrap();
    }

    #[tokio::test]
    async fn uses_defaults_when_notification_has_empty_title() {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form()
            .withf(|_, params| {
                params.contains(&("title", "Test Alert"))
                    && params.contains(&("priority", "0"))
                    && params.contains(&("sound", "pushover"))
            })
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"status":1}"#.to_string(),
                    })
                })
            });

        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        let notification = Notification {
            title: String::new(),
            message: "msg".to_string(),
            priority: 0,
            sound: None,
        };
        notifier.notify(&notification).await.unwrap();
    }

    #[tokio::test]
    async fn returns_error_on_non_200() {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form().returning(|_, _| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 400,
                    body: r#"{"status":0,"errors":["invalid token"]}"#.to_string(),
                })
            })
        });

        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        let err = notifier.notify(&test_notification()).await.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn returns_error_on_http_failure() {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form().returning(|_, _| {
            Box::pin(async { Err(crate::SentinelError::Http("timeout".to_string())) })
        });

        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        let err = notifier.notify(&test_notification()).await.unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn type_name_is_pushover() {
        let mock = MockHttpClient::new();
        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        assert_eq!(notifier.type_name(), "pushover");
    }

    #[tokio::test(start_paused = true)]
    async fn a_post_the_endpoint_does_not_answer_fails_at_the_bound() {
        let mut mock = MockHttpClient::new();
        mock.expect_post_form()
            .returning(|_, _| Box::pin(std::future::pending()));
        let notifier = PushoverNotifier::new(&test_config(), Arc::new(mock));
        let notification = Notification {
            title: String::new(),
            message: "Roof Monitor changed to Unsafe".to_string(),
            priority: 0,
            sound: None,
        };
        let start = tokio::time::Instant::now();

        let err = tokio::time::timeout(2 * NOTIFY_TIMEOUT, notifier.notify(&notification))
            .await
            .expect("the POST outwaited its bound")
            .unwrap_err();

        assert!(
            err.to_string().contains("did not answer within 10s"),
            "{err}"
        );
        assert_eq!(start.elapsed(), NOTIFY_TIMEOUT);
    }
}

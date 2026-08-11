//! Connection management for PHD2 client
//!
//! This module handles TCP connection establishment, reconnection logic,
//! and message reading from PHD2.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tracing::{debug, info, warn};

use crate::config::ReconnectConfig;
use crate::error::Phd2Error;
use crate::events::{AppState, Phd2Event};
#[cfg(test)]
use crate::io::TcpConnectionFactory;
use crate::io::{ConnectionFactory, LineReader, MessageWriter};
use crate::rpc::RpcResponse;

/// Pending RPC request waiting for response
pub struct PendingRequest {
    pub sender: tokio::sync::oneshot::Sender<std::result::Result<serde_json::Value, Phd2Error>>,
}

/// Internal client connection state
#[derive(Debug, Clone, Default)]
pub struct ConnectionState {
    pub connected: bool,
    pub phd2_version: Option<String>,
    pub app_state: Option<AppState>,
    pub reconnecting: bool,
}

/// Shared state for connection management
///
/// This struct holds all the Arc-wrapped state that needs to be shared
/// between the client, reader task, and reconnection task.
#[derive(Clone)]
pub struct SharedConnectionState {
    pub state: Arc<RwLock<ConnectionState>>,
    pub writer: Arc<Mutex<Option<Box<dyn MessageWriter>>>>,
    pub pending_requests: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    pub event_sender: broadcast::Sender<Phd2Event>,
    pub reader_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub auto_reconnect_enabled: Arc<AtomicBool>,
    pub stop_reconnect: Arc<Notify>,
    pub connection_factory: Arc<dyn ConnectionFactory>,
}

impl SharedConnectionState {
    /// Create a new shared connection state with a TCP connection factory (test only)
    #[cfg(test)]
    pub fn new(auto_reconnect_enabled: bool) -> Self {
        Self::with_factory(
            auto_reconnect_enabled,
            Arc::new(TcpConnectionFactory::new()),
        )
    }

    /// Create a new shared connection state with a custom connection factory
    pub fn with_factory(
        auto_reconnect_enabled: bool,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        Self {
            state: Arc::new(RwLock::new(ConnectionState::default())),
            writer: Arc::new(Mutex::new(None)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            event_sender,
            reader_handle: Arc::new(Mutex::new(None)),
            auto_reconnect_enabled: Arc::new(AtomicBool::new(auto_reconnect_enabled)),
            stop_reconnect: Arc::new(Notify::new()),
            connection_factory,
        }
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        self.state.read().await.connected
    }

    /// Check if reconnecting
    pub async fn is_reconnecting(&self) -> bool {
        self.state.read().await.reconnecting
    }

    /// Get PHD2 version
    pub async fn get_phd2_version(&self) -> Option<String> {
        self.state.read().await.phd2_version.clone()
    }

    /// Get cached app state
    pub async fn get_cached_app_state(&self) -> Option<AppState> {
        self.state.read().await.app_state
    }

    /// Check if auto-reconnect is enabled
    pub fn is_auto_reconnect_enabled(&self) -> bool {
        self.auto_reconnect_enabled.load(Ordering::SeqCst)
    }

    /// Set auto-reconnect enabled state
    pub fn set_auto_reconnect_enabled(&self, enabled: bool) {
        debug!("Setting auto-reconnect enabled: {}", enabled);
        self.auto_reconnect_enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.stop_reconnect.notify_waiters();
        }
    }

    /// Stop ongoing reconnection attempts
    pub async fn stop_reconnection(&self) {
        debug!("Stopping reconnection attempts");
        self.stop_reconnect.notify_waiters();
        let mut state = self.state.write().await;
        state.reconnecting = false;
    }
}

/// Configuration for connection attempts
#[derive(Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub connection_timeout: std::time::Duration,
    pub reconnect: ReconnectConfig,
}

/// Spawn a reconnection task that attempts to reconnect to PHD2
pub fn spawn_reconnect_task(
    config: ConnectionConfig,
    shared: SharedConnectionState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Set reconnecting state
        {
            let mut state_guard = shared.state.write().await;
            state_guard.reconnecting = true;
        }

        let addr = format!("{}:{}", config.host, config.port);
        let interval = config.reconnect.interval;
        let timeout_duration = config.connection_timeout;
        let max_retries = config.reconnect.max_retries;
        let mut attempt = 0u32;

        loop {
            attempt = attempt.saturating_add(1);

            // Check if we should stop reconnecting
            if !shared.auto_reconnect_enabled.load(Ordering::SeqCst) {
                debug!("Auto-reconnect disabled, stopping reconnection attempts");
                let _ = shared.event_sender.send(Phd2Event::ReconnectFailed {
                    reason: "Auto-reconnect disabled".to_string(),
                });
                break;
            }

            // Check if max retries exceeded
            if let Some(max) = max_retries {
                if attempt > max {
                    warn!("Reconnection failed: max retries ({}) exceeded", max);
                    let _ = shared.event_sender.send(Phd2Event::ReconnectFailed {
                        reason: format!("Max retries ({max}) exceeded"),
                    });
                    break;
                }
            }

            // Broadcast reconnecting event
            info!(
                "Attempting to reconnect to PHD2 (attempt {}/{})",
                attempt,
                max_retries.map_or("∞".to_string(), |m| m.to_string())
            );
            let _ = shared.event_sender.send(Phd2Event::Reconnecting {
                attempt,
                max_attempts: max_retries,
            });

            // Wait before attempting connection (unless first attempt)
            if attempt > 1 {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = shared.stop_reconnect.notified() => {
                        debug!("Reconnection stopped by user");
                        let _ = shared.event_sender.send(Phd2Event::ReconnectFailed {
                            reason: "Reconnection cancelled".to_string(),
                        });
                        break;
                    }
                }
            }

            // Attempt connection using the connection factory
            debug!("Attempting connection to {}", addr);
            match shared
                .connection_factory
                .connect(&addr, timeout_duration)
                .await
            {
                Ok(connection_pair) => {
                    debug!("Connection established to PHD2");

                    // Store the writer
                    {
                        let mut writer_guard = shared.writer.lock().await;
                        *writer_guard = Some(connection_pair.writer);
                    }

                    // Update connection state
                    {
                        let mut state_guard = shared.state.write().await;
                        state_guard.connected = true;
                        state_guard.reconnecting = false;
                    }

                    // Start a new reader task
                    let new_reader_handle =
                        spawn_reader_task(connection_pair.reader, config.clone(), shared.clone());

                    // Store the new reader handle
                    {
                        let mut handle = shared.reader_handle.lock().await;
                        *handle = Some(new_reader_handle);
                    }

                    // Broadcast reconnected event
                    info!("Successfully reconnected to PHD2");
                    let _ = shared.event_sender.send(Phd2Event::Reconnected);
                    return;
                }
                Err(e) => {
                    debug!("Connection attempt {} failed: {}", attempt, e);
                }
            }
        }

        // Reconnection failed - update state
        {
            let mut state_guard = shared.state.write().await;
            state_guard.reconnecting = false;
        }
    })
}

/// Spawn a reader task that reads messages from PHD2
pub fn spawn_reader_task(
    mut reader: Box<dyn LineReader>,
    config: ConnectionConfig,
    shared: SharedConnectionState,
) -> tokio::task::JoinHandle<()> {
    let reconnect_handle = Arc::new(Mutex::new(None));

    tokio::spawn(async move {
        let disconnect_reason;

        loop {
            match reader.read_line().await {
                Ok(None) => {
                    debug!("PHD2 connection closed");
                    disconnect_reason = "Connection closed by remote".to_string();
                    break;
                }
                Ok(Some(line)) => {
                    if line.is_empty() {
                        continue;
                    }

                    debug!("Received from PHD2: {}", line);

                    // Try to parse as a response first (has "id" field)
                    if let Ok(response) = serde_json::from_str::<RpcResponse>(&line) {
                        let mut pending = shared.pending_requests.lock().await;
                        if let Some(request) = pending.remove(&response.id) {
                            let result = if let Some(error) = response.error {
                                Err(Phd2Error::RpcError {
                                    code: error.code,
                                    message: error.message,
                                })
                            } else {
                                Ok(response.result.unwrap_or(serde_json::Value::Null))
                            };
                            let _ = request.sender.send(result);
                        }
                    } else if let Ok(event) = serde_json::from_str::<Phd2Event>(&line) {
                        // Handle specific events to update internal state
                        match &event {
                            Phd2Event::Version { phd_version, .. } => {
                                let mut state_guard = shared.state.write().await;
                                state_guard.phd2_version = Some(phd_version.clone());
                                debug!("PHD2 version: {}", phd_version);
                            }
                            Phd2Event::AppState { state: app_state } => {
                                if let Ok(parsed_state) = app_state.parse::<AppState>() {
                                    let mut state_guard = shared.state.write().await;
                                    state_guard.app_state = Some(parsed_state);
                                    debug!("PHD2 app state: {}", parsed_state);
                                }
                            }
                            _ => {}
                        }

                        // Broadcast event to subscribers
                        let _ = shared.event_sender.send(event);
                    } else {
                        debug!("Failed to parse PHD2 message: {}", line);
                    }
                }
                Err(e) => {
                    debug!("Error reading from PHD2: {}", e);
                    disconnect_reason = format!("Read error: {e}");
                    break;
                }
            }
        }

        // Connection lost - update state and notify. Clear the cached
        // version/app-state along with `connected`: they describe the
        // session that just ended, and leaving them set would let a
        // caller polling get_phd2_version()/get_cached_app_state() during
        // the reconnect window mistake stale data for the new session
        // (the same race #603 fixed, one layer down).
        {
            let mut state_guard = shared.state.write().await;
            state_guard.connected = false;
            state_guard.phd2_version = None;
            state_guard.app_state = None;
        }

        // Broadcast connection lost event
        warn!("PHD2 connection lost: {}", disconnect_reason);
        let _ = shared.event_sender.send(Phd2Event::ConnectionLost {
            reason: disconnect_reason.clone(),
        });

        // Clear pending requests
        {
            let mut pending = shared.pending_requests.lock().await;
            pending.clear();
        }

        // Close the writer
        {
            let mut writer_guard = shared.writer.lock().await;
            if let Some(mut w) = writer_guard.take() {
                let _ = w.shutdown().await;
            }
        }

        // Start reconnection if enabled
        if shared.auto_reconnect_enabled.load(Ordering::SeqCst) {
            debug!("Auto-reconnect enabled, starting reconnection task");
            let reconnect_task = spawn_reconnect_task(config, shared);
            let mut handle = reconnect_handle.lock().await;
            *handle = Some(reconnect_task);
        }
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_default() {
        let state = ConnectionState::default();
        assert!(!state.connected);
        assert!(state.phd2_version.is_none());
        assert!(state.app_state.is_none());
        assert!(!state.reconnecting);
    }

    #[test]
    fn test_shared_connection_state_auto_reconnect_enabled() {
        let shared = SharedConnectionState::new(true);
        assert!(shared.is_auto_reconnect_enabled());
    }

    #[test]
    fn test_shared_connection_state_auto_reconnect_disabled() {
        let shared = SharedConnectionState::new(false);
        assert!(!shared.is_auto_reconnect_enabled());
    }

    #[test]
    fn test_shared_connection_state_toggle_auto_reconnect() {
        let shared = SharedConnectionState::new(true);
        assert!(shared.is_auto_reconnect_enabled());

        shared.set_auto_reconnect_enabled(false);
        assert!(!shared.is_auto_reconnect_enabled());

        shared.set_auto_reconnect_enabled(true);
        assert!(shared.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_shared_connection_state_initial_values() {
        let shared = SharedConnectionState::new(true);
        assert!(!shared.is_connected().await);
        assert!(!shared.is_reconnecting().await);
        assert!(shared.get_phd2_version().await.is_none());
        assert!(shared.get_cached_app_state().await.is_none());
    }

    #[tokio::test]
    async fn test_shared_connection_state_update_connected() {
        let shared = SharedConnectionState::new(true);

        {
            let mut state = shared.state.write().await;
            state.connected = true;
        }

        assert!(shared.is_connected().await);
    }

    #[tokio::test]
    async fn test_shared_connection_state_update_version() {
        let shared = SharedConnectionState::new(true);

        {
            let mut state = shared.state.write().await;
            state.phd2_version = Some("2.6.11".to_string());
        }

        assert_eq!(shared.get_phd2_version().await, Some("2.6.11".to_string()));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod mock_tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use crate::client::Phd2Client;
    use crate::config::{Phd2Config, ReconnectConfig};
    use crate::io::{ConnectionFactory, ConnectionPair, LineReader, MessageWriter};
    use async_trait::async_trait;

    // ============================================================================
    // Mock implementations
    // ============================================================================

    struct MockLineReaderWithResponses {
        responses: StdMutex<VecDeque<Option<String>>>,
    }

    impl MockLineReaderWithResponses {
        fn new(responses: Vec<Option<String>>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl LineReader for MockLineReaderWithResponses {
        async fn read_line(&mut self) -> crate::Result<Option<String>> {
            let popped = self.responses.lock().unwrap().pop_front();
            match popped {
                Some(response) => Ok(response),
                // Scripted lines exhausted, but the mock connection stays
                // open — a real TCP socket blocks rather than reporting EOF
                // just because the remote hasn't sent anything new. Tests
                // that want to simulate the remote closing the connection
                // queue an explicit `None` entry instead.
                None => {
                    std::future::pending::<()>().await;
                    unreachable!("pending future never resolves")
                }
            }
        }
    }

    struct MockMessageWriterWithRecorder {
        sent_messages: Arc<StdMutex<Vec<String>>>,
    }

    impl MockMessageWriterWithRecorder {
        fn new(sent_messages: Arc<StdMutex<Vec<String>>>) -> Self {
            Self { sent_messages }
        }
    }

    #[async_trait]
    impl MessageWriter for MockMessageWriterWithRecorder {
        async fn write_message(&mut self, message: &str) -> crate::Result<()> {
            self.sent_messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        async fn shutdown(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    type MockPair = (Vec<Option<String>>, Arc<StdMutex<Vec<String>>>);

    struct MockConnectionFactory {
        pairs: StdMutex<VecDeque<MockPair>>,
        connect_count: StdMutex<u32>,
        fail_connect: StdMutex<bool>,
    }

    impl MockConnectionFactory {
        fn new() -> Self {
            Self {
                pairs: StdMutex::new(VecDeque::new()),
                connect_count: StdMutex::new(0),
                fail_connect: StdMutex::new(false),
            }
        }

        fn add_connection(&self, responses: Vec<Option<String>>) -> Arc<StdMutex<Vec<String>>> {
            let sent_messages = Arc::new(StdMutex::new(Vec::new()));
            self.pairs
                .lock()
                .unwrap()
                .push_back((responses, sent_messages.clone()));
            sent_messages
        }

        fn set_fail_connect(&self, fail: bool) {
            *self.fail_connect.lock().unwrap() = fail;
        }

        fn get_connect_count(&self) -> u32 {
            *self.connect_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl ConnectionFactory for MockConnectionFactory {
        async fn connect(&self, _addr: &str, _timeout: Duration) -> crate::Result<ConnectionPair> {
            *self.connect_count.lock().unwrap() += 1;

            if *self.fail_connect.lock().unwrap() {
                return Err(Phd2Error::ConnectionFailed(
                    "Mock connection failure".to_string(),
                ));
            }

            let mut pairs = self.pairs.lock().unwrap();
            if let Some((responses, sent_messages)) = pairs.pop_front() {
                Ok(ConnectionPair {
                    reader: Box::new(MockLineReaderWithResponses::new(responses)),
                    writer: Box::new(MockMessageWriterWithRecorder::new(sent_messages)),
                })
            } else {
                Err(Phd2Error::ConnectionFailed(
                    "No mock connections available".to_string(),
                ))
            }
        }

        async fn can_connect(&self, _addr: &str) -> bool {
            !*self.fail_connect.lock().unwrap() && !self.pairs.lock().unwrap().is_empty()
        }
    }

    fn version_event() -> String {
        r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#.to_string()
    }

    // ============================================================================
    // Connection state tests
    // ============================================================================

    #[tokio::test]
    async fn test_initial_connection_state() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(!client.is_connected().await);
        assert!(!client.is_reconnecting().await);
    }

    #[tokio::test]
    async fn test_connection_state_after_connect() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);

        let config = Phd2Config {
            connection_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        };
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        assert!(client.is_connected().await);
    }

    #[tokio::test]
    async fn test_connection_state_after_disconnect() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        client.disconnect().await.unwrap();
        assert!(!client.is_connected().await);
    }

    #[tokio::test]
    async fn test_disconnect_clears_state() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        // Wait for version event to be processed
        tokio::time::sleep(Duration::from_millis(50)).await;

        client.disconnect().await.unwrap();
        assert!(client.get_phd2_version().await.is_none());
    }

    // ============================================================================
    // PHD2 version tracking tests
    // ============================================================================

    #[tokio::test]
    async fn test_phd2_version_initially_none() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(client.get_phd2_version().await.is_none());
    }

    #[tokio::test]
    async fn test_phd2_version_set_after_connect() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        // Give time for the reader task to process the version event
        tokio::time::sleep(Duration::from_millis(100)).await;

        let version = client.get_phd2_version().await;
        assert_eq!(version, Some("2.6.11".to_string()));
    }

    // ============================================================================
    // Connection failure tests
    // ============================================================================

    #[tokio::test]
    async fn test_connect_failure() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.set_fail_connect(true);

        let config = Phd2Config {
            connection_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        };
        let client = Phd2Client::with_connection_factory(config, factory);

        let result = client.connect().await;
        assert!(matches!(result, Err(Phd2Error::ConnectionFailed(_))));
        assert!(!client.is_connected().await);
    }

    #[tokio::test]
    async fn test_connect_retries_on_failure() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.set_fail_connect(true);
        let factory_clone = factory.clone();

        let config = Phd2Config {
            connection_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        };
        let client = Phd2Client::with_connection_factory(config, factory);

        let _ = client.connect().await;
        assert_eq!(factory_clone.get_connect_count(), 1);
    }

    // ============================================================================
    // Auto-reconnect configuration tests
    // ============================================================================

    #[tokio::test]
    async fn test_auto_reconnect_default_enabled() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_auto_reconnect_can_be_disabled_in_config() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config {
            reconnect: ReconnectConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(!client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_auto_reconnect_toggle() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(client.is_auto_reconnect_enabled());

        client.set_auto_reconnect_enabled(false);
        assert!(!client.is_auto_reconnect_enabled());

        client.set_auto_reconnect_enabled(true);
        assert!(client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_stop_reconnection_when_not_reconnecting() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        // Should not panic or error when called without an active reconnection
        client.stop_reconnection().await;
        assert!(!client.is_reconnecting().await);
    }

    // ============================================================================
    // Multiple connection tests
    // ============================================================================

    #[tokio::test]
    async fn test_reconnect_after_disconnect() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);
        factory.add_connection(vec![Some(version_event())]);

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        // First connection
        client.connect().await.unwrap();
        assert!(client.is_connected().await);

        // Disconnect
        client.disconnect().await.unwrap();
        assert!(!client.is_connected().await);

        // Second connection
        client.connect().await.unwrap();
        assert!(client.is_connected().await);
    }

    #[tokio::test]
    async fn test_connect_when_already_connected_aborts_previous() {
        let factory = Arc::new(MockConnectionFactory::new());
        factory.add_connection(vec![Some(version_event())]);
        factory.add_connection(vec![Some(version_event())]);
        let factory_clone = factory.clone();

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        client.connect().await.unwrap();

        // Should have made 2 connection attempts
        assert_eq!(factory_clone.get_connect_count(), 2);
    }

    // ============================================================================
    // Event subscription tests
    // ============================================================================

    #[tokio::test]
    async fn test_subscribe_returns_receiver() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        let _receiver = client.subscribe();
        // Just verify we can subscribe without panicking
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        let _receiver1 = client.subscribe();
        let _receiver2 = client.subscribe();
        let _receiver3 = client.subscribe();
        // Multiple subscribers should be allowed
    }

    // ============================================================================
    // Cached app state tests
    // ============================================================================

    #[tokio::test]
    async fn test_cached_app_state_initially_none() {
        let factory = Arc::new(MockConnectionFactory::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(client.get_cached_app_state().await.is_none());
    }

    #[tokio::test]
    async fn test_cached_app_state_updated_by_event() {
        let factory = Arc::new(MockConnectionFactory::new());
        let app_state_event = r#"{"Event":"AppState","State":"Guiding"}"#.to_string();
        factory.add_connection(vec![Some(version_event()), Some(app_state_event)]);

        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.connect().await.unwrap();
        // Wait for events to be processed
        tokio::time::sleep(Duration::from_millis(100)).await;

        let cached_state = client.get_cached_app_state().await;
        assert!(cached_state.is_some());
    }
}

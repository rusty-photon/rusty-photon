//! Mock server tests for PHD2 client
//!
//! These tests use a mock TCP server to test client methods without requiring
//! a real PHD2 instance.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use phd2_guider::{GuideAxis, Phd2Client, Phd2Config, Rect, SettleParams};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

/// A simple mock PHD2 server for testing
struct MockPhd2Server {
    listener: TcpListener,
    port: u16,
}

impl MockPhd2Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        Self { listener, port }
    }

    const fn port(&self) -> u16 {
        self.port
    }

    /// Run the server, handling one connection with predefined responses
    fn run_with_responses(self, responses: Vec<String>) {
        thread::spawn(move || {
            if let Ok((mut stream, _)) = self.listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

                // Send version event immediately on connect
                let version_event =
                    r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#;
                writeln!(stream, "{version_event}").ok();
                stream.flush().ok();

                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut response_iter = responses.into_iter();

                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break, // Connection closed
                        Ok(_) => {
                            if let Some(response) = response_iter.next() {
                                writeln!(stream, "{response}").ok();
                                stream.flush().ok();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }

    /// Run the server with a callback for handling each request
    fn run_with_handler<F>(self, handler: F)
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        thread::spawn(move || {
            if let Ok((mut stream, _)) = self.listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

                // Send version event immediately on connect
                let version_event =
                    r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#;
                writeln!(stream, "{version_event}").ok();
                stream.flush().ok();

                let mut reader = BufReader::new(stream.try_clone().unwrap());

                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let response = handler(&line);
                            writeln!(stream, "{response}").ok();
                            stream.flush().ok();
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }

    /// Run the server and send custom messages after connection, then handle requests
    /// Messages are sent immediately after version event, before waiting for requests
    fn run_with_initial_messages(self, initial_messages: Vec<String>, responses: Vec<String>) {
        thread::spawn(move || {
            if let Ok((mut stream, _)) = self.listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

                // Send version event immediately on connect
                let version_event =
                    r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#;
                writeln!(stream, "{version_event}").ok();
                stream.flush().ok();

                // Send initial messages (events, empty lines, malformed JSON, etc.)
                for msg in initial_messages {
                    writeln!(stream, "{msg}").ok();
                    stream.flush().ok();
                    // Small delay between messages
                    thread::sleep(Duration::from_millis(10));
                }

                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut response_iter = responses.into_iter();

                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if let Some(response) = response_iter.next() {
                                writeln!(stream, "{response}").ok();
                                stream.flush().ok();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }

    /// Run the server that disconnects after a delay (for testing reconnection)
    fn run_and_disconnect_after(self, delay_ms: u64) {
        thread::spawn(move || {
            if let Ok((mut stream, _)) = self.listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(1))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(1))).ok();

                // Send version event
                let version_event =
                    r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#;
                writeln!(stream, "{version_event}").ok();
                stream.flush().ok();

                // Wait then disconnect
                thread::sleep(Duration::from_millis(delay_ms));
                drop(stream);
            }
        });
    }
}

fn create_test_config(port: u16) -> Phd2Config {
    Phd2Config {
        host: "127.0.0.1".to_string(),
        port,
        connection_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

/// Deadline for event waits. Every wait in this file is for a guaranteed
/// event (the mock server always sends it, or a state machine always
/// settles), so the deadline only decides when to declare failure. It is
/// sized for contended CI runners; polls return the moment the condition
/// holds, so healthy runs never feel it.
const EVENT_DEADLINE: Duration = Duration::from_secs(30);

/// Poll `probe` every 10 ms until it holds, panicking at [`EVENT_DEADLINE`].
async fn wait_until<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(EVENT_DEADLINE, async {
        while !probe().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} not observed within {EVENT_DEADLINE:?}"));
}

/// Wait until the client's reader task has processed the version event the
/// mock server sends on connect — the observable proof the connection is
/// fully up (`is_connected` flips earlier, on the socket alone).
async fn wait_connected(client: &Phd2Client) {
    wait_until("version event", || async move {
        client.get_phd2_version().await.is_some()
    })
    .await;
}

// ============================================================================
// Connection Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_connect_and_receive_version() {
    let server = MockPhd2Server::new();
    let port = server.port();
    server.run_with_responses(vec![]);

    let config = create_test_config(port);
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();

    wait_connected(&client).await;

    assert!(client.is_connected().await);
    let version = client.get_phd2_version().await;
    assert_eq!(version, Some("2.6.11".to_string()));

    client.disconnect().await.unwrap();
    assert!(!client.is_connected().await);
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_disconnect_clears_state() {
    let server = MockPhd2Server::new();
    let port = server.port();
    server.run_with_responses(vec![]);

    let config = create_test_config(port);
    let client = Phd2Client::new(config);

    client.connect().await.unwrap();
    wait_connected(&client).await;
    assert!(client.is_connected().await);

    client.disconnect().await.unwrap();
    assert!(!client.is_connected().await);
    assert!(client.get_phd2_version().await.is_none());
}

// ============================================================================
// State and Status Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_app_state() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":"Guiding","id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let state = client.get_app_state().await.unwrap();
    assert_eq!(state, phd2_guider::AppState::Guiding);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_app_state_stopped() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":"Stopped","id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let state = client.get_app_state().await.unwrap();
    assert_eq!(state, phd2_guider::AppState::Stopped);

    client.disconnect().await.unwrap();
}

// ============================================================================
// Equipment and Profile Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_is_equipment_connected() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":true,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let connected = client.is_equipment_connected().await.unwrap();
    assert!(connected);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_profiles() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":[{{"id":1,"name":"Default"}},{{"id":2,"name":"Simulator"}}],"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let profiles = client.get_profiles().await.unwrap();
    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0].id, 1);
    assert_eq!(profiles[0].name, "Default");
    assert_eq!(profiles[1].id, 2);
    assert_eq!(profiles[1].name, "Simulator");

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_current_profile() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":{{"id":1,"name":"Default"}},"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let profile = client.get_current_profile().await.unwrap();
    assert_eq!(profile.id, 1);
    assert_eq!(profile.name, "Default");

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_current_equipment() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"camera":{{"name":"Simulator","connected":true}},"mount":{{"name":"On-camera","connected":true}},"aux_mount":null,"AO":null,"rotator":null}},"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let equipment = client.get_current_equipment().await.unwrap();
    assert!(equipment.camera.is_some());
    assert_eq!(equipment.camera.unwrap().name, "Simulator");

    client.disconnect().await.unwrap();
}

// ============================================================================
// Guiding Control Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_start_guiding() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let settle = SettleParams::default();
    let result = client.start_guiding(&settle, false, None).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_start_guiding_with_roi() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        // Verify ROI is in the request
        assert!(req["params"]["roi"].is_array());
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let settle = SettleParams::default();
    let roi = Rect::new(100, 100, 200, 200);
    let result = client.start_guiding(&settle, true, Some(roi)).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_stop_guiding() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.stop_guiding().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_pause_and_resume() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Test pause (full)
    let result = client.pause(true).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_dither() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let settle = SettleParams::default();
    let result = client.dither(5.0, false, &settle).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Star Selection Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_find_star() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.find_star(None).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_lock_position() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":[256.5,512.3],"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let (x, y) = client.get_lock_position().await.unwrap();
    assert_eq!(x, 256.5);
    assert_eq!(y, 512.3);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_lock_position_no_star() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":null,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.get_lock_position().await;
    assert!(result.is_err());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_set_lock_position() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.set_lock_position(256.5, 512.3, true).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Calibration Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_is_calibrated() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":true,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let calibrated = client.is_calibrated().await.unwrap();
    assert!(calibrated);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_calibration_data() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"calibrated":true,"xAngle":45.5,"xRate":15.2,"xParity":"+","yAngle":135.5,"yRate":14.8,"yParity":"-"}},"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let data = client
        .get_calibration_data(phd2_guider::CalibrationTarget::Mount)
        .await
        .unwrap();
    assert!(data.calibrated);
    assert_eq!(data.x_angle, 45.5);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_clear_calibration() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client
        .clear_calibration(phd2_guider::CalibrationTarget::Both)
        .await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_flip_calibration() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.flip_calibration().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Camera Exposure Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_exposure() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":2000,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let exposure = client.get_exposure().await.unwrap();
    assert_eq!(exposure, 2000);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_set_exposure() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.set_exposure(1500).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_exposure_durations() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":[100,200,500,1000,2000],"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let durations = client.get_exposure_durations().await.unwrap();
    assert_eq!(durations.len(), 5);
    assert_eq!(durations[0], 100);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_camera_frame_size() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":[1280,960],"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let (width, height) = client.get_camera_frame_size().await.unwrap();
    assert_eq!(width, 1280);
    assert_eq!(height, 960);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_capture_single_frame() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.capture_single_frame(Some(2000), None).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Guide Algorithm Parameter Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_algo_param_names() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":["Aggressiveness","HysteresisPercentage","MinMove"],"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let names = client.get_algo_param_names(GuideAxis::Ra).await.unwrap();
    assert_eq!(names.len(), 3);
    assert_eq!(names[0], "Aggressiveness");

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_algo_param() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0.75,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let value = client
        .get_algo_param(GuideAxis::Ra, "Aggressiveness")
        .await
        .unwrap();
    assert_eq!(value, 0.75);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_set_algo_param() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.set_algo_param(GuideAxis::Dec, "MinMove", 0.2).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Camera Cooling Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_ccd_temperature() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":-10.5,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let temp = client.get_ccd_temperature().await.unwrap();
    assert_eq!(temp, -10.5);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_cooler_status() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"temperature":-10.0,"coolerOn":true,"setpoint":-10.0,"power":45.0}},"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let status = client.get_cooler_status().await.unwrap();
    assert_eq!(status.temperature, -10.0);
    assert!(status.cooler_on);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_set_cooler_state() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":0,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.set_cooler_state(true, Some(-15.0)).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Image Operations Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_star_image() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"frame":1,"width":32,"height":32,"star_pos":[16.0,16.0],"pixels":"AAAA"}},"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let image = client.get_star_image(15).await.unwrap();
    assert_eq!(image.frame, 1);
    assert_eq!(image.width, 32);
    assert_eq!(image.height, 32);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_save_image() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(r#"{{"jsonrpc":"2.0","result":"/path/to/image.fits","id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let path = client.save_image().await.unwrap();
    assert_eq!(path, "/path/to/image.fits");

    client.disconnect().await.unwrap();
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_rpc_error_response() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        format!(
            r#"{{"jsonrpc":"2.0","error":{{"code":-32600,"message":"Invalid request"}},"id":{id}}}"#
        )
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.get_app_state().await;
    assert!(result.is_err());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_invalid_response_format() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_handler(|request| {
        let req: serde_json::Value = serde_json::from_str(request).unwrap();
        let id = req["id"].as_u64().unwrap();
        // Return wrong type (number instead of string for app state)
        format!(r#"{{"jsonrpc":"2.0","result":123,"id":{id}}}"#)
    });

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.get_app_state().await;
    assert!(result.is_err());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Additional Method Tests
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_connect_equipment() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.connect_equipment().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_disconnect_equipment() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.disconnect_equipment().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_set_profile() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.set_profile(1).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_start_loop() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.start_loop().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_stop_capture() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.stop_capture().await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_is_paused() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":true,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.is_paused().await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_use_subframes() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":true,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.get_use_subframes().await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_shutdown_phd2() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let result = client.shutdown_phd2().await;
    assert!(result.is_ok());

    // Don't call disconnect since the server should shut down
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_get_cached_app_state_initial() {
    let server = MockPhd2Server::new();
    let port = server.port();

    // Just enough to connect (version event doesn't set app state)
    server.run_with_responses(vec![]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Cached state should be None initially (no AppState events received)
    let cached = client.get_cached_app_state().await;
    // May or may not be set depending on events - just exercise the code path
    let _ = cached;

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_is_reconnecting() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Should not be reconnecting when connected
    let reconnecting = client.is_reconnecting().await;
    assert!(!reconnecting);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_stop_reconnection() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Should be safe to call even when not reconnecting
    client.stop_reconnection().await;

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_find_star_with_roi() {
    let server = MockPhd2Server::new();
    let port = server.port();

    server.run_with_responses(vec![r#"{"jsonrpc":"2.0","result":0,"id":1}"#.to_string()]);

    let client = Phd2Client::new(create_test_config(port));
    client.connect().await.unwrap();
    wait_connected(&client).await;

    let roi = Rect::new(100, 100, 200, 200);
    let result = client.find_star(Some(roi)).await;
    assert!(result.is_ok());

    client.disconnect().await.unwrap();
}

// ============================================================================
// Connection Edge Case Tests (for coverage)
// ============================================================================

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_empty_line_handling() {
    // Test that empty lines are ignored (connection.rs:261)
    let server = MockPhd2Server::new();
    let port = server.port();

    // Send empty lines and then a response
    let initial_messages = vec![
        String::new(),     // Empty line
        "   ".to_string(), // Whitespace only
        "\t".to_string(),  // Tab only
    ];
    let responses = vec![r#"{"jsonrpc":"2.0","result":"Stopped","id":1}"#.to_string()];

    server.run_with_initial_messages(initial_messages, responses);

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    // The garbage lines precede the RPC response in stream order, so a
    // reader that survives them answers the call — no fixed delay needed.
    wait_connected(&client).await;

    // Client should still work after receiving empty lines
    let state = client.get_app_state().await.unwrap();
    assert_eq!(state, phd2_guider::AppState::Stopped);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_app_state_event_processing() {
    // Test AppState event updates cached state (connection.rs:288-293)
    let server = MockPhd2Server::new();
    let port = server.port();

    // Send an AppState event after version
    let initial_messages = vec![r#"{"Event":"AppState","State":"Guiding"}"#.to_string()];
    let responses = vec![];

    server.run_with_initial_messages(initial_messages, responses);

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();

    // The AppState event is guaranteed; wait for it to land in the cache.
    let c = &client;
    wait_until("cached AppState", || async move {
        c.get_cached_app_state().await.is_some()
    })
    .await;
    let cached_state = client.get_cached_app_state().await;
    assert_eq!(cached_state, Some(phd2_guider::AppState::Guiding));

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_malformed_json_handling() {
    // Test that malformed JSON is handled gracefully (connection.rs:301)
    let server = MockPhd2Server::new();
    let port = server.port();

    // Send malformed JSON followed by valid response
    let initial_messages = vec![
        "not valid json at all!!!".to_string(),
        "{malformed: json}".to_string(),
        r#"{"missing":"required_fields"}"#.to_string(),
    ];
    let responses = vec![r#"{"jsonrpc":"2.0","result":"Stopped","id":1}"#.to_string()];

    server.run_with_initial_messages(initial_messages, responses);

    let config = create_test_config(port);
    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    // The garbage lines precede the RPC response in stream order, so a
    // reader that survives them answers the call — no fixed delay needed.
    wait_connected(&client).await;

    // Client should still work after receiving malformed JSON
    let state = client.get_app_state().await.unwrap();
    assert_eq!(state, phd2_guider::AppState::Stopped);

    client.disconnect().await.unwrap();
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_reconnect_max_retries_exceeded() {
    // Test that the reconnect loop's max_retries check is respected.
    // Reconnection only ever starts from the reader task's connection-loss
    // path — a failed *initial* connect never spawns it — so the test must
    // connect first and then lose the server. Dropping the server also
    // frees its port, so every retry genuinely fails.
    let server = MockPhd2Server::new();
    let port = server.port();
    server.run_and_disconnect_after(100);

    let mut config = create_test_config(port);
    config.reconnect.enabled = true;
    config.reconnect.interval = Duration::from_millis(50);
    config.reconnect.max_retries = Some(2); // Only try twice
    config.connection_timeout = Duration::from_secs(1);

    let client = Phd2Client::new(config);

    // Subscribe before connecting so no event can be missed.
    let mut events = client.subscribe();

    client.connect().await.unwrap();
    wait_connected(&client).await;

    // The server drop triggers reconnection, whose retries exhaust; the
    // ReconnectFailed broadcast is the deterministic terminal signal.
    let reason = tokio::time::timeout(EVENT_DEADLINE, async {
        loop {
            if let Ok(phd2_guider::Phd2Event::ReconnectFailed { reason }) = events.recv().await {
                return reason;
            }
        }
    })
    .await
    .expect("no ReconnectFailed event within the deadline");
    assert!(reason.contains("Max retries"), "{reason}");

    let c = &client;
    wait_until("reconnection settled", || async move {
        !c.is_reconnecting().await
    })
    .await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_reconnect_disabled_during_reconnection() {
    // Test that disabling auto-reconnect stops the reconnect loop at its
    // next top-of-loop check. Reconnection only ever starts from the
    // reader task's connection-loss path, so connect first, then lose the
    // server. The retry budget (1000 × 50 ms) keeps the task observably
    // reconnecting for far longer than this test runs, and the disabled
    // check runs at each loop top, so the stop lands within one interval.
    let server = MockPhd2Server::new();
    let port = server.port();
    server.run_and_disconnect_after(100);

    let mut config = create_test_config(port);
    config.reconnect.enabled = true;
    config.reconnect.interval = Duration::from_millis(50);
    config.reconnect.max_retries = Some(1000);
    config.connection_timeout = Duration::from_secs(1);

    let client = Phd2Client::new(config);
    client.connect().await.unwrap();
    wait_connected(&client).await;

    // Wait for the reconnection task to be observably running.
    let c = &client;
    wait_until(
        "reconnection start",
        || async move { c.is_reconnecting().await },
    )
    .await;

    // Disable auto-reconnect while reconnecting
    client.set_auto_reconnect_enabled(false);

    // Wait for the reconnection task to notice and stop.
    wait_until(
        "reconnection stop",
        || async move { !c.is_reconnecting().await },
    )
    .await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)] // Miri can't call socket syscalls
async fn test_server_disconnect_triggers_reconnection() {
    // Test that server disconnect triggers auto-reconnection
    let server = MockPhd2Server::new();
    let port = server.port();

    // Server will disconnect after 200ms
    server.run_and_disconnect_after(200);

    let mut config = create_test_config(port);
    config.reconnect.enabled = true;
    config.reconnect.interval = Duration::from_secs(1);
    config.reconnect.max_retries = Some(2);

    let client = Phd2Client::new(config);
    let mut events = client.subscribe();

    // Connect successfully
    client.connect().await.unwrap();
    wait_connected(&client).await;
    assert!(client.is_connected().await);

    // The server drops the stream after its delay; the ConnectionLost
    // broadcast is the deterministic loss signal (we subscribed before
    // connecting, so it cannot be missed).
    tokio::time::timeout(EVENT_DEADLINE, async {
        loop {
            if let Ok(phd2_guider::Phd2Event::ConnectionLost { .. }) = events.recv().await {
                return;
            }
        }
    })
    .await
    .expect("no ConnectionLost event within the deadline");

    // The connected flag flips before the broadcast, and the dropped
    // listener can never accept a reconnect, so the loss is already
    // observable and final.
    assert!(!client.is_connected().await);

    // Stop reconnection to clean up
    client.stop_reconnection().await;
}

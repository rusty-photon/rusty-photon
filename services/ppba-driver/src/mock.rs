//! Mock PPBA transport for testing without real hardware.
//!
//! Provides a [`TransportFactory`] that hands out a [`FrameTransport`]
//! backed by an in-memory PPBA state machine. Persists state across
//! reconnects so tests can disconnect/reconnect and still observe their
//! prior writes (matches the behaviour of real hardware that doesn't
//! lose its settings when an ASCOM client cycles `Connected`).

// `#[cfg(any(feature = "mock", test))]`-gated test-helper infrastructure
// that never ships in production builds. Excluded from coverage so the
// workspace coverage number reflects only production-shipped code —
// counting these never-shipped mock lines would produce false coverage
// figures.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tracing::debug;

/// In-memory PPBA device state, plus a queue of responses each accepted
/// command appended.
#[derive(Debug, Default)]
struct MockState {
    response_queue: VecDeque<Vec<u8>>,
    device_state: MockDeviceState,
}

#[derive(Debug, Clone)]
struct MockDeviceState {
    quad_12v: bool,
    adjustable: bool,
    dew_a: u8,
    dew_b: u8,
    usb_hub: bool,
    auto_dew: bool,
    voltage: f64,
    current: f64,
    temperature: f64,
    humidity: f64,
    dewpoint: f64,
    power_warning: bool,
    average_amps: f64,
    amp_hours: f64,
    watt_hours: f64,
    uptime: Duration,
}

impl Default for MockDeviceState {
    fn default() -> Self {
        Self {
            quad_12v: true,
            adjustable: false,
            dew_a: 128,
            dew_b: 64,
            usb_hub: false,
            auto_dew: false, // OFF by default so ConformU dew-heater writes pass.
            voltage: 12.5,
            current: 3.2,
            temperature: 25.0,
            humidity: 60.0,
            dewpoint: 15.5,
            power_warning: false,
            average_amps: 2.5,
            amp_hours: 10.5,
            watt_hours: 126.0,
            uptime: Duration::from_hours(1),
        }
    }
}

impl MockDeviceState {
    fn status_response(&self) -> String {
        format!(
            "PPBA:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.voltage,
            self.current,
            self.temperature,
            // Integer percent on the wire; `trunc` prints the same digits
            // the old `as u8` cast produced for any in-range humidity.
            self.humidity.trunc().clamp(0.0, 255.0),
            self.dewpoint,
            i32::from(self.quad_12v),
            i32::from(self.adjustable),
            self.dew_a,
            self.dew_b,
            i32::from(self.auto_dew),
            i32::from(self.power_warning),
            0 // power adjust
        )
    }

    fn power_stats_response(&self) -> String {
        format!(
            "PS:{}:{}:{}:{}",
            self.average_amps,
            self.amp_hours,
            self.watt_hours,
            self.uptime.as_millis()
        )
    }
}

impl MockState {
    fn process_command(&mut self, command_bytes: &[u8]) {
        let command = std::str::from_utf8(command_bytes)
            .unwrap_or_default()
            .trim();
        debug!(
            command,
            quad_12v = self.device_state.quad_12v,
            adjustable = self.device_state.adjustable,
            dew_a = self.device_state.dew_a,
            dew_b = self.device_state.dew_b,
            auto_dew = self.device_state.auto_dew,
            "mock processing command"
        );

        let response = if command == "P#" {
            "PPBA_OK".to_string()
        } else if command == "PA" {
            self.device_state.status_response()
        } else if command == "PS" {
            self.device_state.power_stats_response()
        } else if command == "PV" {
            "1.0.0".to_string()
        } else if let Some(value) = command.strip_prefix("P1:") {
            let state = value == "1";
            self.device_state.quad_12v = state;
            format!("P1:{}", i32::from(state))
        } else if let Some(value) = command.strip_prefix("P2:") {
            let state = value == "1";
            self.device_state.adjustable = state;
            format!("P2:{}", i32::from(state))
        } else if let Some(value) = command.strip_prefix("P3:") {
            if let Ok(pwm) = value.parse::<u8>() {
                self.device_state.dew_a = pwm;
                format!("P3:{pwm}")
            } else {
                "ERR".to_string()
            }
        } else if let Some(value) = command.strip_prefix("P4:") {
            if let Ok(pwm) = value.parse::<u8>() {
                self.device_state.dew_b = pwm;
                format!("P4:{pwm}")
            } else {
                "ERR".to_string()
            }
        } else if let Some(value) = command.strip_prefix("PU:") {
            let state = value == "1";
            self.device_state.usb_hub = state;
            format!("PU:{}", i32::from(state))
        } else if let Some(value) = command.strip_prefix("PD:") {
            let state = value == "1";
            self.device_state.auto_dew = state;
            format!("PD:{}", i32::from(state))
        } else {
            debug!(command, "mock: unknown command");
            "ERR".to_string()
        };

        let mut frame = response.into_bytes();
        frame.push(b'\n');
        self.response_queue.push_back(frame);
    }
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings survive a reconnect cycle.
struct MockFrameTransport {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.state.lock().await.process_command(bytes);
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        let frame = self.state.lock().await.response_queue.pop_front();
        match frame {
            Some(frame) => {
                buf.clear();
                buf.extend_from_slice(&frame);
                Ok(())
            }
            None => Err(TransportError::Eof),
        }
    }
}

/// Mock factory for the PPBA transport.
///
/// Maintains persistent device state across multiple open/close cycles so
/// tests can power-cycle the connection without losing the simulated
/// device's settings — matching the behaviour of real hardware.
#[derive(Clone, Default)]
pub struct MockPpbaTransportFactory {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl TransportFactory for MockPpbaTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock PPBA transport opened");
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open(factory: &MockPpbaTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    #[tokio::test]
    async fn ping_round_trip() {
        let factory = MockPpbaTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"P#\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert_eq!(&buf, b"PPBA_OK\n");
    }

    #[tokio::test]
    async fn status_round_trip() {
        let factory = MockPpbaTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"PA\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"PPBA:"));
        assert!(buf.ends_with(b"\n"));
    }

    #[tokio::test]
    async fn power_stats_round_trip() {
        let factory = MockPpbaTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"PS\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert!(buf.starts_with(b"PS:"));
        assert!(buf.ends_with(b"\n"));
    }

    #[tokio::test]
    async fn set_command_echoes_and_mutates_state() {
        let factory = MockPpbaTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"P1:0\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert_eq!(&buf, b"P1:0\n");

        t.send_frame(b"PA\n").await.unwrap();
        t.recv_frame(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf).unwrap().trim();
        let parts: Vec<&str> = text.split(':').collect();
        assert_eq!(parts[6], "0", "quad_12v should now be off: {text}");
    }

    #[tokio::test]
    async fn state_persists_across_reopens() {
        let factory = MockPpbaTransportFactory::default();
        {
            let mut t = open(&factory).await;
            t.send_frame(b"P1:0\n").await.unwrap();
            let mut buf = Vec::new();
            t.recv_frame(&mut buf).await.unwrap();
        }
        let mut t = open(&factory).await;
        t.send_frame(b"PA\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf).unwrap().trim();
        let parts: Vec<&str> = text.split(':').collect();
        assert_eq!(parts[6], "0");
    }

    #[tokio::test]
    async fn empty_queue_returns_eof() {
        let factory = MockPpbaTransportFactory::default();
        let mut t = open(&factory).await;
        let mut buf = Vec::new();
        let err = t.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof));
    }
}

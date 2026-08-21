//! Error types for the star-adventurer-gti driver.
//!
//! The shared transport-driver common core (`NotConnected`, `ConnectionFailed`,
//! `Io`, `Timeout`, `Serialization`, `InvalidValue`, …) plus its ASCOM
//! classification, `From<TransportError>`, and the `Result` alias are generated
//! by [`rusty_photon_driver::driver_error!`]. Only the mount-specific variants
//! (`Transport`, `Protocol`, `InvalidOperation`, `Parked`, `Config`,
//! `Timekeeping`, `WrongDevice`) are written inline. The codec-layer
//! `From<SessionError<SkywatcherCodecError>>` / `From<SkywatcherCodecError>`
//! impls live in [`crate::codec`] and target this (local) enum.

rusty_photon_driver::driver_error! {
    /// Errors that can arise inside the driver.
    pub enum StarAdvError {
        #[error("transport error: {0}")]
        Transport(String),

        #[error("protocol error: {0}")]
        Protocol(#[from] skywatcher_motor_protocol::ProtocolError),

        #[error("invalid operation: {0}")]
        InvalidOperation(String),

        #[error("parked")]
        Parked,

        #[error("config error: {0}")]
        Config(String),

        /// ERFA-side time-conversion failure. In practice this is
        /// `eraCal2jd` (reached transitively from `Dtf2d`) returning
        /// `-1` for a year outside its calendar floor (`IYMIN = -4799`)
        /// — a host clock set absurdly far in the past. ERFA's
        /// leap-second-table boundary (years before 1960 or beyond
        /// `IYV + 5`) is *not* this error; `Utctai` reports it as
        /// status `1` ("dubious") which the erfars binding maps to
        /// `Ok((value, 1))`, so the LST still computes normally for
        /// any realistic future-shifted clock. The chrono-validated
        /// `Dtf2d` calendar-component checks are unreachable in
        /// practice but are folded into the same variant so the call
        /// site stays a total function.
        #[error("timekeeping error: {0}")]
        Timekeeping(String),

        /// The connect handshake's first wire query (`:e1`) returned a reply
        /// that doesn't look like a Sky-Watcher motor-board-version response.
        /// The most likely cause is that the configured transport target
        /// (serial port or UDP host) points at a different device — a power
        /// box, focuser, or unrelated USB-CDC peripheral sharing the host's
        /// USB bus on the serial path, or the wrong host / wrong network on
        /// the UDP path. See [issue #254][issue].
        ///
        /// [issue]: https://github.com/rusty-photon/rusty-photon/issues/254
        #[error(
            "handshake to {port} returned unexpected data ({reason}); this device may not be a \
             Sky-Watcher motor controller. Common cause: the configured transport target points \
             at the wrong device (e.g. wrong serial port to a focuser / power box / other \
             USB-CDC peripheral, or wrong UDP host). Verify that {port} is the GTi's transport \
             endpoint."
        )]
        WrongDevice { port: String, reason: String },
    }
    ascom {
        Self::Parked => INVALID_WHILE_PARKED,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ascom_alpaca::{ASCOMError, ASCOMErrorCode};

    #[test]
    fn not_connected_maps_to_ascom_not_connected() {
        let err: ASCOMError = StarAdvError::NotConnected.into();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[test]
    fn invalid_value_maps_to_ascom_invalid_value() {
        let err: ASCOMError = StarAdvError::InvalidValue("ra out of range".to_string()).into();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[test]
    fn parked_maps_to_ascom_invalid_while_parked() {
        let err: ASCOMError = StarAdvError::Parked.into();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
    }

    #[test]
    fn other_variants_map_to_invalid_operation() {
        for err in [
            StarAdvError::ConnectionFailed("boom".into()),
            StarAdvError::Transport("eof".into()),
            StarAdvError::Timeout("read".into()),
            StarAdvError::InvalidOperation("double connect".into()),
            StarAdvError::Config("missing".into()),
            StarAdvError::Timekeeping("ERFA Utctai returned -1".into()),
            StarAdvError::WrongDevice {
                port: "/dev/ttyUSB1".into(),
                reason: "test reason".into(),
            },
        ] {
            let mapped: ASCOMError = err.into();
            assert_eq!(mapped.code, ASCOMErrorCode::INVALID_OPERATION);
        }
    }

    #[test]
    fn wrong_device_display_quotes_port_and_suggests_cause() {
        // The operator-facing message must name the port (twice — once
        // in the diagnostic head, once in the verify-the-port trailer)
        // and call out the wrong-device hypothesis. The handshake hook
        // in `manager.rs` constructs this variant only after the `:e1`
        // reply fails framing or whitelist, so the message correctly
        // implies "we tried to identify the device and it wasn't a
        // Sky-Watcher".
        let err = StarAdvError::WrongDevice {
            port: "/dev/ttyUSB1".into(),
            reason: "unknown mount-type byte 0xFF".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/dev/ttyUSB1"), "msg: {msg}");
        assert!(msg.contains("unknown mount-type byte 0xFF"), "msg: {msg}");
        assert!(msg.contains("Sky-Watcher"), "msg: {msg}");
        assert!(msg.contains("wrong device"), "msg: {msg}");
        assert!(msg.contains("transport endpoint"), "msg: {msg}");
    }

    #[test]
    fn wrong_device_display_works_for_udp_target() {
        // The transport-agnostic wording must also fit a UDP
        // misconfiguration (the GTi has built-in WiFi AP at
        // `192.168.4.1:11880`). The label that lands in {port} is
        // produced by `TransportConfig::port_label()` and looks like
        // `192.168.4.1:11880` for v4 / `[fe80::1]:11880` for v6 —
        // neither contains the substring "serial", so the message
        // must not be locked to serial-specific phrasing.
        let err = StarAdvError::WrongDevice {
            port: "192.168.4.1:11880".into(),
            reason: "unknown mount-type byte 0xFF".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("192.168.4.1:11880"), "msg: {msg}");
        assert!(msg.contains("wrong UDP host"), "msg: {msg}");
        assert!(msg.contains("transport endpoint"), "msg: {msg}");
    }

    #[test]
    fn protocol_error_converts_through_from() {
        let pe = skywatcher_motor_protocol::ProtocolError::FrameError("missing CR".to_string());
        let driver: StarAdvError = pe.into();
        assert!(matches!(driver, StarAdvError::Protocol(_)));
    }

    #[test]
    fn io_error_converts_through_from() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        let driver: StarAdvError = io.into();
        assert!(matches!(driver, StarAdvError::Io(_)));
    }
}

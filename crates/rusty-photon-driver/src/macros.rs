//! The [`driver_error!`] macro: define a driver error enum that shares the common
//! transport-driver failure modes and their ASCOM classification.
//!
//! The transport-backed Alpaca drivers share one error core (`NotConnected`,
//! `Io`, `Timeout`, `Communication`, …) and the same `to_ascom_error` /
//! `From<DriverError> for ASCOMError` / `From<TransportError>` impls. This macro
//! defines that core **once** and splices in each driver's device-specific
//! variants, generating a **flat** enum — a common variant is
//! `XxxError::NotConnected` beside the driver's own — that is still a local type
//! (so a driver's own `From<SessionError<XxxCodecError>>` stays orphan-legal).

/// Define a driver error enum sharing the common transport-driver variants.
///
/// Generates a flat `thiserror` enum with the ten common variants —
/// `NotConnected`, `ConnectionFailed(String)`, `SerialPort(String)`,
/// `Timeout(String)`, `Io(std::io::Error)`, `Serialization(serde_json::Error)`,
/// `InvalidResponse(String)`, `ParseError(String)`, `Communication(String)`,
/// `InvalidValue(String)` — plus any device-specific variants written inline, and:
/// `to_ascom_error` + `From<Self> for ASCOMError`, `From<TransportError>`, and a
/// `Result<T>` alias.
///
/// `NotConnected` maps to `NOT_CONNECTED`, `InvalidValue` to `INVALID_VALUE`, and
/// everything else to `INVALID_OPERATION` — unless a device variant is listed in
/// the optional `ascom { <pattern> => <CODE>, … }` block (patterns are written
/// against `Self`, e.g. `Self::Parked => INVALID_WHILE_PARKED`).
///
/// The invoking crate must depend on `thiserror`, `serde_json`, `ascom-alpaca`,
/// and `rusty-photon-shared-transport`.
#[macro_export]
macro_rules! driver_error {
    (
        $(#[$emeta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident
                $( ( $( $(#[$fattr:meta])* $fty:ty ),+ $(,)? ) )?
                $( { $( $(#[$sfattr:meta])* $sfield:ident : $sfty:ty ),+ $(,)? } )?
            ),* $(,)?
        }
        $( ascom { $( $apat:pat => $acode:ident ),* $(,)? } )?
    ) => {
        $(#[$emeta])*
        #[derive(Debug, ::thiserror::Error)]
        $vis enum $name {
            #[error("not connected")]
            NotConnected,
            #[error("connection failed: {0}")]
            ConnectionFailed(::std::string::String),
            #[error("serial port error: {0}")]
            SerialPort(::std::string::String),
            #[error("timeout: {0}")]
            Timeout(::std::string::String),
            #[error("io error: {0}")]
            Io(#[from] ::std::io::Error),
            #[error("serialization error: {0}")]
            Serialization(#[from] ::serde_json::Error),
            #[error("invalid response: {0}")]
            InvalidResponse(::std::string::String),
            #[error("parse error: {0}")]
            ParseError(::std::string::String),
            #[error("device communication error: {0}")]
            Communication(::std::string::String),
            #[error("invalid value: {0}")]
            InvalidValue(::std::string::String),
            $(
                $(#[$vmeta])*
                $variant
                $( ( $( $(#[$fattr])* $fty ),+ ) )?
                $( { $( $(#[$sfattr])* $sfield : $sfty ),+ } )?
            ,)*
        }

        impl $name {
            /// Classify this error into the matching ASCOM error code.
            pub fn to_ascom_error(&self) -> ::ascom_alpaca::ASCOMError {
                match self {
                    $name::NotConnected => ::ascom_alpaca::ASCOMError::new(
                        ::ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED,
                        ::std::string::ToString::to_string(self),
                    ),
                    $name::InvalidValue(_) => ::ascom_alpaca::ASCOMError::new(
                        ::ascom_alpaca::ASCOMErrorCode::INVALID_VALUE,
                        ::std::string::ToString::to_string(self),
                    ),
                    $($(
                        $apat => ::ascom_alpaca::ASCOMError::new(
                            ::ascom_alpaca::ASCOMErrorCode::$acode,
                            ::std::string::ToString::to_string(self),
                        ),
                    )*)?
                    _ => ::ascom_alpaca::ASCOMError::invalid_operation(
                        ::std::string::ToString::to_string(self),
                    ),
                }
            }
        }

        impl ::std::convert::From<$name> for ::ascom_alpaca::ASCOMError {
            fn from(err: $name) -> Self {
                err.to_ascom_error()
            }
        }

        /// Canonical mapping from a shared-transport `TransportError`.
        impl ::std::convert::From<::rusty_photon_shared_transport::TransportError> for $name {
            fn from(t: ::rusty_photon_shared_transport::TransportError) -> Self {
                match t {
                    ::rusty_photon_shared_transport::TransportError::Open(e) => {
                        $name::ConnectionFailed(::std::string::ToString::to_string(&e))
                    }
                    ::rusty_photon_shared_transport::TransportError::Io(e) => $name::Io(e),
                    ::rusty_photon_shared_transport::TransportError::Timeout(d) => {
                        $name::Timeout(::std::format!("transport timeout after {d:?}"))
                    }
                    ::rusty_photon_shared_transport::TransportError::Eof => {
                        $name::Communication(::std::string::String::from("Connection closed"))
                    }
                    ::rusty_photon_shared_transport::TransportError::Framing(s) => {
                        $name::Communication(::std::format!("framing: {s}"))
                    }
                    ::rusty_photon_shared_transport::TransportError::Reconnecting => {
                        $name::Communication(::std::string::String::from(
                            "transport is reconnecting",
                        ))
                    }
                }
            }
        }

        /// Result alias for this driver's operations.
        $vis type Result<T> = ::std::result::Result<T, $name>;
    };
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
// `dead_code`: the sample enums exercise the macro but don't construct every
// common variant; real driver enums are `pub` in a lib so are never so linted.
#[allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable
)]
mod tests {
    use ascom_alpaca::ASCOMErrorCode;
    use rusty_photon_shared_transport::TransportError;
    use std::time::Duration;

    // Exercise every macro feature: no-arg/tuple/struct variants, a `#[from]`
    // device variant, and `ascom { … }` code overrides (including a unit variant).
    #[derive(Debug, thiserror::Error)]
    #[error("inner protocol failure: {0}")]
    struct InnerProtocol(String);

    driver_error! {
        /// Doc comment passes through.
        enum SampleError {
            #[error("widget jammed: {0}")]
            WidgetJammed(u32),
            #[error("parked")]
            Parked,
            #[error("protocol: {0}")]
            Protocol(#[from] InnerProtocol),
            #[error("wrong device on {port}: {reason}")]
            WrongDevice { port: String, reason: String },
        }
        ascom {
            Self::WidgetJammed(_) => INVALID_VALUE,
            Self::Parked => INVALID_WHILE_PARKED,
        }
    }

    #[test]
    fn common_variants_classify() {
        assert_eq!(
            SampleError::NotConnected.to_ascom_error().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            SampleError::InvalidValue("x".into()).to_ascom_error().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            SampleError::Communication("x".into()).to_ascom_error().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[test]
    fn device_variants_use_overrides_else_default() {
        assert_eq!(
            SampleError::WidgetJammed(3).to_ascom_error().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            SampleError::Parked.to_ascom_error().code,
            ASCOMErrorCode::INVALID_WHILE_PARKED
        );
        // Not listed in `ascom { … }` -> default INVALID_OPERATION.
        assert_eq!(
            SampleError::WrongDevice {
                port: "p".into(),
                reason: "r".into()
            }
            .to_ascom_error()
            .code,
            ASCOMErrorCode::INVALID_OPERATION
        );
    }

    #[test]
    fn from_impls_work() {
        // `From<Self> for ASCOMError`
        let ascom: ascom_alpaca::ASCOMError = SampleError::NotConnected.into();
        assert_eq!(ascom.code, ASCOMErrorCode::NOT_CONNECTED);
        // `#[from]` on a common variant
        let io: SampleError = std::io::Error::other("x").into();
        assert!(matches!(io, SampleError::Io(_)));
        // `#[from]` on a device variant
        let proto: SampleError = InnerProtocol("boom".into()).into();
        assert!(matches!(proto, SampleError::Protocol(_)));
    }

    #[test]
    fn from_transport_maps_each_variant() {
        assert!(matches!(
            SampleError::from(TransportError::Open(std::io::Error::other("busy"))),
            SampleError::ConnectionFailed(_)
        ));
        assert!(matches!(
            SampleError::from(TransportError::Timeout(Duration::from_secs(1))),
            SampleError::Timeout(_)
        ));
        assert!(matches!(
            SampleError::from(TransportError::Eof),
            SampleError::Communication(_)
        ));
    }

    #[test]
    fn no_extra_variants_compiles() {
        driver_error! {
            enum BareError {}
        }
        assert_eq!(
            BareError::NotConnected.to_ascom_error().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        let _: Result<()> = Ok(());
    }
}

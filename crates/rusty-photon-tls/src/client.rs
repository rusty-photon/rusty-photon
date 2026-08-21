use std::path::Path;

use tracing::debug;

use crate::error::{Result, TlsError};

/// Build a `reqwest::ClientBuilder` with optional CA certificate trust.
///
/// For callers that need to layer their own timeouts, headers, or other
/// customization before calling `.build()`. [`build_reqwest_client`] is a
/// thin wrapper over this for callers with no further customization.
///
/// When `ca_cert_path` is `Some`, the PEM-encoded CA certificate at that path
/// becomes the client's **only** trusted root (`tls_certs_only`, ADR-002) —
/// this replaces, not adds to, the platform/built-in trust store, so a
/// public-CA `https://` endpoint becomes unreachable through this builder
/// alongside the observatory CA. This allows the client to connect to
/// services using certificates signed by the Rusty Photon CA.
///
/// When `ca_cert_path` is `None`, returns a default builder using the
/// platform trust store.
///
/// # Errors
///
/// Returns a [`TlsError`] if the CA certificate file cannot be read or
/// does not parse as PEM.
pub fn client_builder(ca_cert_path: Option<&Path>) -> Result<reqwest::ClientBuilder> {
    crate::install_default_crypto_provider();

    let mut builder = reqwest::Client::builder();

    if let Some(ca_path) = ca_cert_path {
        debug!("Loading CA certificate from {}", ca_path.display());
        let ca_pem = std::fs::read(ca_path)?;
        let ca_cert = reqwest::Certificate::from_pem(&ca_pem)
            .map_err(|e| TlsError::Pem(format!("failed to parse CA certificate: {e}")))?;
        // Use tls_certs_only to disable built-in/platform root certs.
        // Without this, the platform verifier (e.g. macOS Security framework)
        // rejects our self-signed CA because it is not in the system keychain.
        builder = builder.tls_certs_only([ca_cert]);
    }

    Ok(builder)
}

/// Build a `reqwest::Client` with optional CA certificate trust. See
/// [`client_builder`] for the customizable variant.
///
/// # Errors
///
/// Returns a [`TlsError`] if the CA certificate cannot be read or
/// parsed, or if building the client fails.
pub fn build_reqwest_client(ca_cert_path: Option<&Path>) -> Result<reqwest::Client> {
    client_builder(ca_cert_path)?
        .build()
        .map_err(|e| TlsError::Other(format!("failed to build reqwest client: {e}")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn build_client_without_ca() {
        let client = build_reqwest_client(None).unwrap();
        // Just verify we get a valid client
        drop(client);
    }

    #[test]
    fn build_client_with_valid_ca() {
        // Generate a CA cert first
        let dir = tempfile::tempdir().unwrap();
        crate::test_cert::generate_ca(dir.path()).unwrap();

        let ca_path = dir.path().join("ca.pem");
        let client = build_reqwest_client(Some(&ca_path)).unwrap();
        drop(client);
    }

    #[test]
    fn build_client_with_missing_ca_returns_error() {
        let result = build_reqwest_client(Some(Path::new("/nonexistent/ca.pem")));
        assert!(result.is_err());
    }

    #[test]
    fn client_builder_missing_ca_returns_error() {
        let result = client_builder(Some(Path::new("/nonexistent/ca.pem")));
        assert!(result.is_err());
    }

    #[test]
    fn client_builder_allows_further_customization() {
        let client = client_builder(None)
            .unwrap()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        drop(client);
    }
}

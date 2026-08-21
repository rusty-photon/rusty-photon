//! Throwaway certificate generation for test fixtures.
//!
//! Production issuance — real validity windows, hostname SANs, ACME — is
//! doctor's provisioning module; this generates just enough PKI for a test
//! to serve HTTPS on localhost and a client to trust it. Kept in the
//! serving crate (rather than each test tree) so bdd-infra's `PkiFixture`
//! and this crate's own roundtrip tests share one copy without depending
//! on the doctor binary.

use std::fs;
use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

use crate::error::{Result, TlsError};

/// Generate a self-signed test CA, writing `ca.pem` and `ca-key.pem` to
/// `output_dir` (created if absent). Uses rcgen's default validity window.
///
/// # Errors
///
/// Returns a [`TlsError`] if key generation or self-signing fails, or
/// the I/O error if the directory or files cannot be written.
pub fn generate_ca(output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Rusty Photon Test CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    fs::write(output_dir.join("ca.pem"), cert.pem())?;
    fs::write(output_dir.join("ca-key.pem"), key_pair.serialize_pem())?;
    Ok(())
}

/// Generate a test service certificate signed by the CA, with SANs for
/// `localhost` and the loopback addresses.
///
/// Writes `{service_name}.pem` and `{service_name}-key.pem` to
/// `output_dir` (created if absent).
///
/// # Errors
///
/// Returns a [`TlsError`] if the CA material does not parse or signing
/// fails, or the I/O error if the directory or files cannot be written.
pub fn generate_service_cert(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    service_name: &str,
    output_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, &ca_key)
        .map_err(|e| TlsError::Other(format!("failed to load CA issuer: {e}")))?;

    let mut params = CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|e| TlsError::Other(format!("invalid SAN: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, service_name);
    // Match production shape (doctor's `cert::generate_service_cert`): AKI
    // + SKI, so tests exercise the same strict-verifier-compatible certs
    // that are actually issued (issue #621).
    params.is_ca = IsCa::ExplicitNoCa;
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));

    let service_key = KeyPair::generate()?;
    let service_cert = params.signed_by(&service_key, &issuer)?;

    fs::write(
        output_dir.join(format!("{service_name}.pem")),
        service_cert.pem(),
    )?;
    fs::write(
        output_dir.join(format!("{service_name}-key.pem")),
        service_key.serialize_pem(),
    )?;
    Ok(())
}

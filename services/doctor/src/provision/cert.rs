use std::fs;
use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use rusty_photon_tls::error::{Result, TlsError};
use rusty_photon_tls::permissions::{refuse_symlink, write_restricted};
use tracing::debug;

/// Duration for CA certificate validity (10 years in days).
const CA_VALIDITY_DAYS: i64 = 3650;

/// Duration for service certificate validity (10 years in days).
const SERVICE_VALIDITY_DAYS: i64 = 3650;

/// Validity end for a certificate issued now: `now + days`, with the
/// (unreachable at the 10-year constants above) overflow past
/// representable time surfaced as a config error rather than a panic.
fn validity_end(days: i64) -> Result<time::OffsetDateTime> {
    time::OffsetDateTime::now_utc()
        .checked_add(time::Duration::days(days))
        .ok_or_else(|| {
            TlsError::Config(format!(
                "certificate validity of {days} days overflows the representable date range"
            ))
        })
}

/// Generate a self-signed root CA certificate and key.
///
/// Writes `ca.pem` and `ca-key.pem` to `output_dir`.
/// Creates `output_dir` if it does not exist.
pub fn generate_ca(output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Rusty Photon Observatory CA");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Rusty Photon");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = validity_end(CA_VALIDITY_DAYS)?;

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let cert_path = output_dir.join("ca.pem");
    let key_path = output_dir.join("ca-key.pem");

    refuse_symlink(&cert_path)?;
    fs::write(&cert_path, cert.pem())?;
    write_restricted(&key_path, key_pair.serialize_pem().as_bytes())?;

    debug!("Generated CA certificate: {}", cert_path.display());
    debug!("Generated CA private key: {}", key_path.display());

    Ok(())
}

/// Generate a service certificate signed by the CA.
///
/// The certificate includes SANs for `localhost`, `127.0.0.1`, the system
/// hostname, and any additional SANs provided.
///
/// Writes `{service_name}.pem` and `{service_name}-key.pem` to `output_dir`.
/// Creates `output_dir` if it does not exist.
pub fn generate_service_cert(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    service_name: &str,
    extra_sans: &[String],
    output_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    // Load CA as an Issuer for signing
    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, &ca_key)
        .map_err(|e| TlsError::Other(format!("failed to load CA issuer: {e}")))?;

    // An extra SAN that parses as an address becomes an IP SAN — clients
    // dialing a service by LAN IP verify against IP SANs, not a DNS SAN
    // that happens to look like one.
    let (extra_ips, extra_dns): (Vec<String>, Vec<String>) = extra_sans
        .iter()
        .cloned()
        .partition(|san| san.parse::<std::net::IpAddr>().is_ok());

    // Build service cert params
    let mut params = CertificateParams::new(build_dns_sans(&extra_dns))
        .map_err(|e| TlsError::Other(format!("invalid SAN: {e}")))?;

    params
        .distinguished_name
        .push(DnType::CommonName, service_name);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Rusty Photon");
    // RFC 5280 §4.2.1.1 makes Authority Key Identifier a MUST on CA-issued
    // (non-self-signed) certs, and §4.2.1.2 makes Subject Key Identifier a
    // SHOULD for leaves. `ExplicitNoCa` (vs `NoCa`) is what makes rcgen
    // write the leaf's own SKI; `use_authority_key_identifier_extension`
    // makes it write the AKI, derived from the CA's actual SKI via the
    // `issuer` loaded above. Without both, strict verifiers (e.g. Python
    // 3.13's default `VERIFY_X509_STRICT`) reject the cert (issue #621).
    params.is_ca = IsCa::ExplicitNoCa;
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = validity_end(SERVICE_VALIDITY_DAYS)?;

    // Add IP SANs: both loopbacks always, plus the extra addresses.
    let mut ips = vec![
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    ];
    for extra in &extra_ips {
        if let Ok(ip) = extra.parse::<std::net::IpAddr>() {
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    for ip in ips {
        params.subject_alt_names.push(SanType::IpAddress(ip));
    }

    let service_key = KeyPair::generate()?;
    let service_cert = params.signed_by(&service_key, &issuer)?;

    let cert_path = output_dir.join(format!("{service_name}.pem"));
    let key_path = output_dir.join(format!("{service_name}-key.pem"));

    refuse_symlink(&cert_path)?;
    fs::write(&cert_path, service_cert.pem())?;
    write_restricted(&key_path, service_key.serialize_pem().as_bytes())?;

    debug!(
        "Generated service certificate for '{}': {}",
        service_name,
        cert_path.display()
    );

    Ok(())
}

/// Build the list of DNS SANs for a service certificate.
fn build_dns_sans(extra_sans: &[String]) -> Vec<String> {
    let mut sans = vec!["localhost".to_string()];

    // Add system hostname
    if let Some(hostname) = get_hostname() {
        if hostname != "localhost" {
            sans.push(hostname);
        }
    }

    // Add user-provided extra SANs
    for san in extra_sans {
        if !sans.contains(san) {
            sans.push(san.clone());
        }
    }

    sans
}

/// Get the system hostname, or `None` if it cannot be determined.
fn get_hostname() -> Option<String> {
    hostname::get().ok().and_then(|h| h.into_string().ok())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn generate_ca_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        generate_ca(dir.path()).unwrap();

        assert!(dir.path().join("ca.pem").exists());
        assert!(dir.path().join("ca-key.pem").exists());

        // Verify PEM contents are non-empty and look like PEM
        let cert_pem = fs::read_to_string(dir.path().join("ca.pem")).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        let key_pem = fs::read_to_string(dir.path().join("ca-key.pem")).unwrap();
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));
    }

    #[cfg(unix)]
    #[test]
    fn generate_ca_refuses_a_symlinked_cert_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "existing").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("ca.pem")).unwrap();

        let err = generate_ca(dir.path()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(
            fs::read(&target).unwrap(),
            b"existing",
            "the symlink target must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generate_service_cert_refuses_a_symlinked_cert_path() {
        let dir = tempfile::tempdir().unwrap();
        generate_ca(dir.path()).unwrap();
        let ca_cert = fs::read_to_string(dir.path().join("ca.pem")).unwrap();
        let ca_key = fs::read_to_string(dir.path().join("ca-key.pem")).unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "existing").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("sentinel.pem")).unwrap();

        let err =
            generate_service_cert(&ca_cert, &ca_key, "sentinel", &[], dir.path()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(
            fs::read(&target).unwrap(),
            b"existing",
            "the symlink target must be untouched"
        );
    }

    #[test]
    fn generate_service_cert_creates_files() {
        let ca_dir = tempfile::tempdir().unwrap();
        generate_ca(ca_dir.path()).unwrap();

        let ca_cert_pem = fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(ca_dir.path().join("ca-key.pem")).unwrap();

        let certs_dir = tempfile::tempdir().unwrap();
        generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-service",
            &[],
            certs_dir.path(),
        )
        .unwrap();

        assert!(certs_dir.path().join("test-service.pem").exists());
        assert!(certs_dir.path().join("test-service-key.pem").exists());

        let cert_pem = fs::read_to_string(certs_dir.path().join("test-service.pem")).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn generate_service_cert_with_extra_sans() {
        let ca_dir = tempfile::tempdir().unwrap();
        generate_ca(ca_dir.path()).unwrap();

        let ca_cert_pem = fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(ca_dir.path().join("ca-key.pem")).unwrap();

        let certs_dir = tempfile::tempdir().unwrap();
        generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-service",
            &["observatory.local".to_string()],
            certs_dir.path(),
        )
        .unwrap();

        assert!(certs_dir.path().join("test-service.pem").exists());
    }

    #[test]
    fn generate_service_cert_turns_ip_extra_sans_into_ip_sans() {
        use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

        let ca_dir = tempfile::tempdir().unwrap();
        generate_ca(ca_dir.path()).unwrap();
        let ca_cert_pem = fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(ca_dir.path().join("ca-key.pem")).unwrap();

        let certs_dir = tempfile::tempdir().unwrap();
        generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-service",
            &["observatory.local".to_string(), "192.0.2.7".to_string()],
            certs_dir.path(),
        )
        .unwrap();

        let pem = fs::read_to_string(certs_dir.path().join("test-service.pem")).unwrap();
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed.contents).unwrap();
        let san = cert.subject_alternative_name().unwrap().unwrap();
        let mut dns = Vec::new();
        let mut ips = Vec::new();
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(d) => dns.push((*d).to_string()),
                GeneralName::IPAddress(bytes) => ips.push(bytes.to_vec()),
                _ => {}
            }
        }
        assert!(dns.contains(&"observatory.local".to_string()), "{dns:?}");
        assert!(
            !dns.contains(&"192.0.2.7".to_string()),
            "an address must not be a DNS SAN: {dns:?}"
        );
        assert!(
            ips.contains(&vec![192, 0, 2, 7]),
            "the address must be an IP SAN: {ips:?}"
        );
    }

    #[test]
    fn generate_ca_includes_subject_key_identifier() {
        use x509_parser::extensions::ParsedExtension;
        use x509_parser::oid_registry::OID_X509_EXT_SUBJECT_KEY_IDENTIFIER;
        use x509_parser::prelude::{FromDer, X509Certificate};

        let dir = tempfile::tempdir().unwrap();
        generate_ca(dir.path()).unwrap();

        let pem = fs::read_to_string(dir.path().join("ca.pem")).unwrap();
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed.contents).unwrap();

        let ext = cert
            .get_extension_unique(&OID_X509_EXT_SUBJECT_KEY_IDENTIFIER)
            .unwrap()
            .expect("CA cert must carry a Subject Key Identifier (RFC 5280 §4.2.1.2)");
        assert!(matches!(
            ext.parsed_extension(),
            ParsedExtension::SubjectKeyIdentifier(_)
        ));
        assert!(
            !ext.critical,
            "RFC 5280 §4.2.1.2 requires SKI to be non-critical"
        );
    }

    #[test]
    fn generate_service_cert_includes_authority_and_subject_key_identifiers() {
        use x509_parser::extensions::ParsedExtension;
        use x509_parser::oid_registry::{
            OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER, OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
        };
        use x509_parser::prelude::{FromDer, X509Certificate};

        let ca_dir = tempfile::tempdir().unwrap();
        generate_ca(ca_dir.path()).unwrap();
        let ca_cert_pem = fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(ca_dir.path().join("ca-key.pem")).unwrap();

        let (_, ca_parsed) = x509_parser::pem::parse_x509_pem(ca_cert_pem.as_bytes()).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(&ca_parsed.contents).unwrap();
        let ca_ski_ext = ca_cert
            .get_extension_unique(&OID_X509_EXT_SUBJECT_KEY_IDENTIFIER)
            .unwrap()
            .expect("CA cert must carry a Subject Key Identifier");
        let ParsedExtension::SubjectKeyIdentifier(ca_ski) = ca_ski_ext.parsed_extension() else {
            panic!("expected a SubjectKeyIdentifier extension");
        };

        let certs_dir = tempfile::tempdir().unwrap();
        generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-service",
            &[],
            certs_dir.path(),
        )
        .unwrap();

        let pem = fs::read_to_string(certs_dir.path().join("test-service.pem")).unwrap();
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed.contents).unwrap();

        let ski_ext = cert
            .get_extension_unique(&OID_X509_EXT_SUBJECT_KEY_IDENTIFIER)
            .unwrap()
            .expect("service cert must carry its own Subject Key Identifier");
        assert!(matches!(
            ski_ext.parsed_extension(),
            ParsedExtension::SubjectKeyIdentifier(_)
        ));
        assert!(
            !ski_ext.critical,
            "RFC 5280 §4.2.1.2 requires SKI to be non-critical"
        );

        let aki_ext = cert
            .get_extension_unique(&OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER)
            .unwrap()
            .expect(
                "service cert must carry an Authority Key Identifier pointing at the issuing CA",
            );
        assert!(
            !aki_ext.critical,
            "RFC 5280 §4.2.1.1 requires AKI to be non-critical"
        );
        let ParsedExtension::AuthorityKeyIdentifier(aki) = aki_ext.parsed_extension() else {
            panic!("expected an AuthorityKeyIdentifier extension");
        };
        let aki_key_id = aki
            .key_identifier
            .as_ref()
            .expect("AKI must carry a keyIdentifier")
            .0;
        assert_eq!(
            aki_key_id, ca_ski.0,
            "the service cert's AKI must match the issuing CA's SKI"
        );
    }

    #[test]
    fn build_dns_sans_includes_localhost() {
        let sans = build_dns_sans(&[]);
        assert!(sans.contains(&"localhost".to_string()));
    }

    #[test]
    fn build_dns_sans_deduplicates() {
        let sans = build_dns_sans(&["localhost".to_string()]);
        assert_eq!(
            sans.iter().filter(|s| *s == "localhost").count(),
            1,
            "localhost should appear exactly once"
        );
    }

    #[test]
    fn generated_cert_chain_validates() {
        use rustls::pki_types::CertificateDer;

        rusty_photon_tls::install_default_crypto_provider();

        let ca_dir = tempfile::tempdir().unwrap();
        generate_ca(ca_dir.path()).unwrap();

        let ca_cert_pem = fs::read_to_string(ca_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = fs::read_to_string(ca_dir.path().join("ca-key.pem")).unwrap();

        let certs_dir = tempfile::tempdir().unwrap();
        generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "test-service",
            &[],
            certs_dir.path(),
        )
        .unwrap();

        // Load and parse the service cert + key to verify they are valid
        let cert_pem = fs::read_to_string(certs_dir.path().join("test-service.pem")).unwrap();
        let key_pem = fs::read_to_string(certs_dir.path().join("test-service-key.pem")).unwrap();

        let certs: Vec<CertificateDer> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(certs.len(), 1, "should have exactly one certificate");

        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
            .unwrap()
            .expect("should have a private key");

        // Verify we can build a rustls ServerConfig with these certs
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key);

        assert!(
            server_config.is_ok(),
            "should build valid rustls config: {:?}",
            server_config.err()
        );
    }
}

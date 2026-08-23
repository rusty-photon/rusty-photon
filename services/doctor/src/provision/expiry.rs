//! Certificate expiry and SAN inspection (docs/services/doctor.md
//! §Renewal, the `tls.expiry` check).
//!
//! Reads the leaf certificate — the first PEM block, which is the entity
//! certificate in every chain doctor writes — so the same functions serve
//! the self-signed pairs and the ACME wildcard chain.

use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

/// The leaf certificate's `notAfter`.
///
/// # Errors
///
/// Returns a message if `cert_pem` is not PEM or its first block is not an
/// X.509 certificate.
pub fn not_after(cert_pem: &str) -> Result<time::OffsetDateTime, String> {
    with_leaf(cert_pem, |cert| cert.validity().not_after.to_datetime())
}

/// The leaf certificate's DNS and IP subject alternative names, in order,
/// IPs in string form.
///
/// Empty when the certificate cannot be parsed or carries none — renewal
/// treats an unreadable SAN list as "nothing extra to preserve".
#[must_use]
pub fn sans(cert_pem: &str) -> Vec<String> {
    with_leaf(cert_pem, |cert| {
        let mut sans = Vec::new();
        if let Ok(Some(extension)) = cert.subject_alternative_name() {
            for name in &extension.value.general_names {
                match name {
                    GeneralName::DNSName(dns) => sans.push((*dns).to_string()),
                    // String form: `generate_service_cert` re-parses these
                    // back into IP SANs, so renewal round-trips them.
                    GeneralName::IPAddress(bytes) => match bytes.len() {
                        4 => {
                            let octets: [u8; 4] = (*bytes).try_into().unwrap_or_default();
                            sans.push(std::net::IpAddr::from(octets).to_string());
                        }
                        16 => {
                            let octets: [u8; 16] = (*bytes).try_into().unwrap_or_default();
                            sans.push(std::net::IpAddr::from(octets).to_string());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        sans
    })
    .unwrap_or_default()
}

/// The leaf certificate's raw subject public key bytes — renewal compares
/// them against a key file's public half to catch a pair whose halves no
/// longer match.
///
/// # Errors
///
/// Returns a message if `cert_pem` is not PEM or its first block is not an
/// X.509 certificate.
pub fn public_key(cert_pem: &str) -> Result<Vec<u8>, String> {
    with_leaf(cert_pem, |cert| {
        cert.public_key().subject_public_key.data.to_vec()
    })
}

/// Parse the first PEM block as an X.509 certificate and apply `f`.
fn with_leaf<T>(cert_pem: &str, f: impl FnOnce(&X509Certificate<'_>) -> T) -> Result<T, String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| format!("not PEM: {e}"))?;
    let (_, cert) = X509Certificate::from_der(&pem.contents)
        .map_err(|e| format!("not an X.509 certificate: {e}"))?;
    Ok(f(&cert))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A self-signed certificate with a known validity window and SAN set.
    fn cert_pem(not_after: time::OffsetDateTime, sans: &[&str]) -> String {
        let mut params = rcgen::CertificateParams::new(
            sans.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.not_before = not_after - time::Duration::days(365);
        params.not_after = not_after;
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn test_not_after_reads_the_declared_date() {
        // X.509 truncates to whole seconds; compare at that granularity.
        let expected = time::OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap()
            + time::Duration::days(42);
        let pem = cert_pem(expected, &["localhost"]);
        assert_eq!(not_after(&pem).unwrap(), expected);
    }

    #[test]
    fn test_not_after_rejects_garbage() {
        not_after("not a pem").unwrap_err();
        not_after("-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydA==\n-----END CERTIFICATE-----\n")
            .unwrap_err();
    }

    #[test]
    fn test_sans_lists_dns_names_and_ip_addresses() {
        let expires = time::OffsetDateTime::now_utc() + time::Duration::days(30);
        let pem = cert_pem(expires, &["localhost", "observatory.local"]);
        assert_eq!(
            sans(&pem),
            vec!["localhost".to_string(), "observatory.local".to_string()]
        );
    }

    #[test]
    fn test_sans_of_garbage_is_empty() {
        assert!(sans("not a pem").is_empty());
    }
}

use std::collections::HashMap;

use async_trait::async_trait;
use rusty_photon_tls::error::{Result, TlsError};
use tracing::debug;

use super::cloudflare::RealCloudflareApi;

/// Trait for DNS providers that can create and delete TXT records
/// for ACME DNS-01 challenge validation.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Create a TXT record for the ACME challenge.
    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<()>;

    /// Remove the TXT record after validation completes.
    async fn delete_txt_record(&self, fqdn: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Cloudflare API abstraction (mockable boundary)
// ---------------------------------------------------------------------------

/// Minimal zone info returned by zone lookup.
#[derive(Debug, Clone)]
pub struct ZoneInfo {
    pub id: String,
}

/// Minimal DNS record info returned by record listing.
#[derive(Debug, Clone)]
pub struct RecordInfo {
    pub id: String,
}

/// Trait abstracting Cloudflare API calls so `CloudflareDnsProvider` can be
/// tested without real HTTP calls.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CloudflareApi: Send + Sync {
    /// List zones matching the given domain name.
    async fn list_zones(&self, domain: String) -> Result<Vec<ZoneInfo>>;

    /// Create a TXT DNS record in the given zone.
    async fn create_txt_record_api(
        &self,
        zone_id: String,
        name: String,
        content: String,
    ) -> Result<()>;

    /// List TXT DNS records matching the given name in the zone.
    async fn list_txt_records(&self, zone_id: String, name: String) -> Result<Vec<RecordInfo>>;

    /// Delete a DNS record by ID.
    async fn delete_record(&self, zone_id: String, record_id: String) -> Result<()>;
}

// The implementation talking to the real Cloudflare v4 REST API lives in
// the sibling `cloudflare` module.

// ---------------------------------------------------------------------------
// CloudflareDnsProvider (testable via CloudflareApi trait)
// ---------------------------------------------------------------------------

/// Cloudflare DNS provider using the `CloudflareApi` abstraction.
///
/// The zone ID is resolved once at construction time by walking the
/// domain's parent labels until one matches a zone the API token can
/// see. The domain itself must sit at least one label below the zone
/// apex — the `<service>.<host>.<domain>` pattern — so the wildcard
/// certificate never covers sibling hostnames in the zone.
#[derive(derive_more::Debug)]
pub struct CloudflareDnsProvider {
    #[debug(skip)]
    api: Box<dyn CloudflareApi>,
    zone_id: String,
}

/// The domain and each parent suffix that could be a registered zone
/// (two labels minimum), longest first: the zone enclosing
/// `rig.example.com` is registered as `example.com`.
fn zone_candidates(domain: &str) -> Vec<String> {
    let labels: Vec<&str> = domain.split('.').collect();
    (0..labels.len().saturating_sub(1))
        .filter_map(|i| labels.get(i..))
        .map(|tail| tail.join("."))
        .collect()
}

impl CloudflareDnsProvider {
    /// Create a new Cloudflare DNS provider.
    ///
    /// Connects to the Cloudflare API with the given token and resolves
    /// the zone ID for the specified domain.
    ///
    /// # Errors
    ///
    /// Returns [`TlsError::Config`] if `domain` has an empty label (a
    /// leading, trailing, or doubled dot), has a single label, or is itself
    /// the apex of its zone (the wildcard would cover the whole zone) — no
    /// other shape is validated here — and [`TlsError::DnsProvider`] if the
    /// client cannot be constructed, a zone query fails, or no zone visible
    /// to the token contains the domain.
    pub async fn new(api_token: &str, domain: &str) -> Result<Self> {
        let api = RealCloudflareApi::new(api_token)?;
        Self::with_api(Box::new(api), domain).await
    }

    /// Create a provider with a custom `CloudflareApi` implementation.
    /// Used internally and for testing.
    async fn with_api(api: Box<dyn CloudflareApi>, domain: &str) -> Result<Self> {
        // Zone names never carry empty labels, so a malformed domain
        // would walk only impossible candidates — reject it before any
        // API query, and likewise a single-label domain, which no
        // registrable zone (two labels minimum) can contain.
        if domain.split('.').any(str::is_empty) {
            return Err(TlsError::Config(format!(
                "domain '{domain}' is malformed: a leading, trailing, or doubled dot \
                 produces an empty label, which can never match a Cloudflare zone name"
            )));
        }
        let candidates = zone_candidates(domain);
        if candidates.is_empty() {
            return Err(TlsError::Config(format!(
                "domain '{domain}' has a single label, so no Cloudflare zone can \
                 contain it; ACME domains follow '<host>.<zone>' (e.g. 'rig.example.com')"
            )));
        }
        // Cloudflare's zone name filter is an exact match, so each
        // candidate suffix is its own query, longest first.
        for candidate in &candidates {
            let zones = api.list_zones(candidate.clone()).await?;
            let Some(zone) = zones.first() else {
                continue;
            };
            if candidate == domain {
                return Err(TlsError::Config(format!(
                    "domain '{domain}' is the apex of its Cloudflare zone — the ACME \
                     wildcard '*.{domain}' would cover every hostname in the zone. Use a \
                     host label under the zone (e.g. 'rig.{domain}') so services live at \
                     '<service>.rig.{domain}'"
                )));
            }
            debug!(
                "Resolved Cloudflare zone '{}' (id '{}') for domain '{}'",
                candidate, zone.id, domain
            );
            return Ok(Self {
                api,
                zone_id: zone.id.clone(),
            });
        }

        Err(TlsError::DnsProvider(format!(
            "no Cloudflare zone found for domain '{domain}' (tried: {}); the zone \
             must be visible to the API token",
            candidates.join(", ")
        )))
    }
}

#[async_trait]
impl DnsProvider for CloudflareDnsProvider {
    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<()> {
        debug!("Creating TXT record: {} = {}", fqdn, value);
        self.api
            .create_txt_record_api(self.zone_id.clone(), fqdn.to_string(), value.to_string())
            .await?;
        debug!("Created TXT record for {}", fqdn);
        Ok(())
    }

    async fn delete_txt_record(&self, fqdn: &str) -> Result<()> {
        debug!("Deleting TXT records for {}", fqdn);

        let records = self
            .api
            .list_txt_records(self.zone_id.clone(), fqdn.to_string())
            .await?;

        if records.is_empty() {
            debug!("No TXT records found for {} (already cleaned up)", fqdn);
            return Ok(());
        }

        for record in &records {
            self.api
                .delete_record(self.zone_id.clone(), record.id.clone())
                .await?;
            debug!("Deleted TXT record {} for {}", record.id, fqdn);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// pebble-challtestsrv provider (mock builds only)
// ---------------------------------------------------------------------------

/// DNS provider driving Pebble's `pebble-challtestsrv` management API —
/// the BDD suite's DNS-01 leg. Exists only in `mock` builds: it is a test
/// server's API, never an operator's DNS.
#[cfg(feature = "mock")]
#[derive(Debug)]
pub struct ChalltestsrvDnsProvider {
    /// The management base URL, e.g. `http://127.0.0.1:8055`.
    base_url: String,
    client: reqwest::Client,
}

#[cfg(feature = "mock")]
impl ChalltestsrvDnsProvider {
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn post(&self, endpoint: &str, body: serde_json::Value) -> Result<()> {
        let url = format!("{}/{endpoint}", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                TlsError::DnsProvider(format!("challtestsrv request to {url} failed: {e}"))
            })?;
        if !response.status().is_success() {
            return Err(TlsError::DnsProvider(format!(
                "challtestsrv {url} answered {}",
                response.status()
            )));
        }
        Ok(())
    }
}

#[cfg(feature = "mock")]
#[async_trait]
impl DnsProvider for ChalltestsrvDnsProvider {
    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<()> {
        debug!("challtestsrv set-txt: {} = {}", fqdn, value);
        // challtestsrv hosts are fully-qualified — the trailing dot matters.
        self.post(
            "set-txt",
            serde_json::json!({ "host": format!("{fqdn}."), "value": value }),
        )
        .await
    }

    async fn delete_txt_record(&self, fqdn: &str) -> Result<()> {
        debug!("challtestsrv clear-txt: {}", fqdn);
        self.post(
            "clear-txt",
            serde_json::json!({ "host": format!("{fqdn}.") }),
        )
        .await
    }
}

/// Build a DNS provider from a provider name and credentials.
///
/// Currently supports:
/// - `"cloudflare"` — requires `api_token` in credentials
/// - `"challtestsrv"` (mock builds only) — Pebble's DNS sidecar; the
///   `api_token` credential slot carries its management base URL, which is
///   how `--dns-token` reaches it without a test-only flag
///
/// # Errors
///
/// Returns [`TlsError::Config`] if `provider_name` is unsupported or the
/// provider's `api_token` credential is missing, plus
/// [`CloudflareDnsProvider::new`]'s errors for `"cloudflare"`.
pub async fn build_dns_provider<S: std::hash::BuildHasher + Sync>(
    provider_name: &str,
    credentials: &HashMap<String, String, S>,
    domain: &str,
) -> Result<Box<dyn DnsProvider>> {
    match provider_name {
        "cloudflare" => {
            let api_token = credentials.get("api_token").ok_or_else(|| {
                TlsError::Config(
                    "Cloudflare DNS provider requires 'api_token' in dns_credentials".to_string(),
                )
            })?;
            let provider = CloudflareDnsProvider::new(api_token, domain).await?;
            Ok(Box::new(provider))
        }
        #[cfg(feature = "mock")]
        "challtestsrv" => {
            let base_url = credentials.get("api_token").ok_or_else(|| {
                TlsError::Config(
                    "the challtestsrv DNS provider requires its management base URL \
                     in the 'api_token' credential slot"
                        .to_string(),
                )
            })?;
            Ok(Box::new(ChalltestsrvDnsProvider::new(base_url)))
        }
        other => {
            #[cfg(feature = "mock")]
            let supported = "cloudflare, challtestsrv";
            #[cfg(not(feature = "mock"))]
            let supported = "cloudflare";
            Err(TlsError::Config(format!(
                "unsupported DNS provider: '{other}'. Supported providers: {supported}"
            )))
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn trait_object_safety() {
        fn _assert_object_safe(_: &dyn DnsProvider) {}
    }

    // -----------------------------------------------------------------------
    // build_dns_provider tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn build_dns_provider_unknown_provider_returns_error() {
        let creds = HashMap::from([("api_token".to_string(), "tok".to_string())]);
        let result = build_dns_provider("unknown", &creds, "example.com").await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("unsupported DNS provider"),
            "error should mention unsupported provider: {msg}"
        );
    }

    #[tokio::test]
    async fn build_dns_provider_cloudflare_missing_token_returns_error() {
        let creds = HashMap::new();
        let result = build_dns_provider("cloudflare", &creds, "example.com").await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("api_token"),
            "error should mention missing api_token: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // CloudflareDnsProvider tests (via MockCloudflareApi)
    // -----------------------------------------------------------------------

    fn zone_only_for(apex: &'static str, id: &'static str) -> MockCloudflareApi {
        let mut mock_api = MockCloudflareApi::new();
        mock_api.expect_list_zones().returning(move |name| {
            if name == apex {
                Ok(vec![ZoneInfo { id: id.to_string() }])
            } else {
                Ok(vec![])
            }
        });
        mock_api
    }

    #[test]
    fn zone_candidates_walks_parent_suffixes_longest_first() {
        assert_eq!(
            zone_candidates("svc.rig.example.com"),
            vec!["svc.rig.example.com", "rig.example.com", "example.com"]
        );
    }

    #[test]
    fn zone_candidates_two_label_domain_is_its_own_only_candidate() {
        assert_eq!(zone_candidates("example.com"), vec!["example.com"]);
    }

    #[test]
    fn zone_candidates_single_label_domain_has_no_candidates() {
        assert_eq!(zone_candidates("localhost"), Vec::<String>::new());
    }

    #[tokio::test]
    async fn cloudflare_provider_walks_parent_labels_to_resolve_zone() {
        let mock_api = zone_only_for("example.com", "zone-123");

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        assert_eq!(provider.zone_id, "zone-123");
    }

    #[tokio::test]
    async fn cloudflare_provider_rejects_zone_apex_domain() {
        let mock_api = zone_only_for("example.com", "zone-123");

        let err = CloudflareDnsProvider::with_api(Box::new(mock_api), "example.com")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("apex"), "error should name the apex: {msg}");
        assert!(
            msg.contains("host label"),
            "error should suggest a host label: {msg}"
        );
    }

    #[tokio::test]
    async fn cloudflare_provider_rejects_apex_of_a_subdomain_zone() {
        // Both a subdomain zone and its parent are registered; the longest
        // match wins, so the subdomain zone's apex is still rejected.
        let mut mock_api = MockCloudflareApi::new();
        mock_api.expect_list_zones().returning(|name| {
            let id = match name.as_str() {
                "rig.example.com" => "zone-sub",
                "example.com" => "zone-parent",
                _ => return Ok(vec![]),
            };
            Ok(vec![ZoneInfo { id: id.to_string() }])
        });

        let err = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("apex"), "error: {err}");
    }

    #[tokio::test]
    async fn cloudflare_provider_no_zone_found_names_the_walked_suffixes() {
        let mut mock_api = MockCloudflareApi::new();
        mock_api.expect_list_zones().returning(|_| Ok(vec![]));

        let err = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.missing.com")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no Cloudflare zone found"), "error: {msg}");
        assert!(
            msg.contains("rig.missing.com, missing.com"),
            "error should list the walked suffixes: {msg}"
        );
    }

    #[tokio::test]
    async fn cloudflare_provider_rejects_single_label_domain_without_queries() {
        let mut mock_api = MockCloudflareApi::new();
        mock_api.expect_list_zones().never();

        let err = CloudflareDnsProvider::with_api(Box::new(mock_api), "localhost")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("single label"), "error: {msg}");
        assert!(
            !msg.contains("API token"),
            "a shape problem must not blame the token: {msg}"
        );
    }

    #[tokio::test]
    async fn cloudflare_provider_rejects_domains_with_empty_labels_without_queries() {
        for domain in ["rig.example.com.", ".example.com", "rig..example.com"] {
            let mut mock_api = MockCloudflareApi::new();
            mock_api.expect_list_zones().never();

            let err = CloudflareDnsProvider::with_api(Box::new(mock_api), domain)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("malformed"),
                "'{domain}' should be rejected as malformed: {err}"
            );
        }
    }

    #[tokio::test]
    async fn cloudflare_provider_zone_lookup_error_propagates() {
        let mut mock_api = MockCloudflareApi::new();
        mock_api
            .expect_list_zones()
            .returning(|_| Err(TlsError::DnsProvider("zone list failed".to_string())));

        let err = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("zone list failed"), "error: {err}");
    }

    #[tokio::test]
    async fn cloudflare_provider_creates_txt_record() {
        let mut mock_api = zone_only_for("example.com", "zone-abc");
        mock_api
            .expect_create_txt_record_api()
            .withf(|zone_id, name, content| {
                zone_id == "zone-abc"
                    && name == "_acme-challenge.rig.example.com"
                    && content == "dns-value-xyz"
            })
            .returning(|_, _, _| Ok(()));

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        provider
            .create_txt_record("_acme-challenge.rig.example.com", "dns-value-xyz")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cloudflare_provider_create_record_error_propagates() {
        let mut mock_api = zone_only_for("example.com", "zone-1");
        mock_api
            .expect_create_txt_record_api()
            .returning(|_, _, _| Err(TlsError::DnsProvider("API error".to_string())));

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        let err = provider
            .create_txt_record("_acme-challenge.rig.example.com", "val")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("API error"), "error: {err}");
    }

    #[tokio::test]
    async fn cloudflare_provider_deletes_matching_records() {
        let mut mock_api = zone_only_for("example.com", "zone-del");
        mock_api.expect_list_txt_records().returning(|_, _| {
            Ok(vec![
                RecordInfo {
                    id: "rec-1".to_string(),
                },
                RecordInfo {
                    id: "rec-2".to_string(),
                },
            ])
        });
        mock_api
            .expect_delete_record()
            .times(2)
            .returning(|_, _| Ok(()));

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        provider
            .delete_txt_record("_acme-challenge.rig.example.com")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cloudflare_provider_delete_no_records_is_ok() {
        let mut mock_api = zone_only_for("example.com", "zone-empty");
        mock_api
            .expect_list_txt_records()
            .returning(|_, _| Ok(vec![]));
        mock_api.expect_delete_record().never();

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        provider
            .delete_txt_record("_acme-challenge.rig.example.com")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cloudflare_provider_delete_record_error_propagates() {
        let mut mock_api = zone_only_for("example.com", "zone-err");
        mock_api.expect_list_txt_records().returning(|_, _| {
            Ok(vec![RecordInfo {
                id: "rec-x".to_string(),
            }])
        });
        mock_api
            .expect_delete_record()
            .returning(|_, _| Err(TlsError::DnsProvider("delete failed".to_string())));

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        let err = provider
            .delete_txt_record("_acme-challenge.rig.example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("delete failed"), "error: {err}");
    }

    #[tokio::test]
    async fn cloudflare_provider_list_records_error_propagates() {
        let mut mock_api = zone_only_for("example.com", "zone-le");
        mock_api
            .expect_list_txt_records()
            .returning(|_, _| Err(TlsError::DnsProvider("list failed".to_string())));

        let provider = CloudflareDnsProvider::with_api(Box::new(mock_api), "rig.example.com")
            .await
            .unwrap();
        let err = provider
            .delete_txt_record("_acme-challenge.rig.example.com")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("list failed"), "error: {err}");
    }
}

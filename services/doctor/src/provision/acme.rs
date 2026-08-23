use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_tls::error::{Result, TlsError};
use rusty_photon_tls::permissions::{create_restricted, refuse_symlink, write_restricted};
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::acme_config::{self, AcmeConfig};
use super::dns::DnsProvider;

/// Trait abstracting the ACME protocol operations.
///
/// This allows mocking the ACME client in tests without making real
/// HTTP calls to Let's Encrypt. Uses owned types to be mockall-compatible.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait AcmeClient: Send + Sync {
    /// Create or load an ACME account.
    ///
    /// If `existing_credentials_json` is `Some`, the account is restored
    /// from those credentials. Otherwise a new account is created.
    ///
    /// Returns `Some(credentials_json)` if a new account was created
    /// (caller should persist it), or `None` if an existing account was loaded.
    async fn create_or_load_account(
        &self,
        email: String,
        directory_url: String,
        existing_credentials_json: Option<String>,
    ) -> Result<Option<String>>;

    /// Run the full ACME order flow for a wildcard domain:
    /// create order, solve DNS-01 challenges, finalize, return (`cert_pem`, `key_pem`).
    ///
    /// Must be called after `create_or_load_account`.
    async fn order_certificate(&self, domain: String) -> Result<(String, String)>;
}

/// Real ACME client using `instant-acme`.
///
/// Stores the account handle after `create_or_load_account` so that
/// `order_certificate` can use it. Holds a DNS provider reference for
/// solving DNS-01 challenges, an optional extra trust anchor for the ACME
/// server's own TLS endpoint (private directories such as step-ca or
/// Pebble), and the wait between writing a TXT record and requesting
/// validation.
pub struct RealAcmeClient<'a> {
    dns_provider: &'a dyn DnsProvider,
    acme_root: Option<PathBuf>,
    propagation_wait: Duration,
    account: Arc<Mutex<Option<instant_acme::Account>>>,
}

impl<'a> RealAcmeClient<'a> {
    pub fn new(
        dns_provider: &'a dyn DnsProvider,
        acme_root: Option<PathBuf>,
        propagation_wait: Duration,
    ) -> Self {
        // instant-acme's HTTP client builds rustls configs from the
        // process-default CryptoProvider, which our dependency tree cannot
        // auto-select (both aws-lc-rs and ring are feature-activated).
        rusty_photon_tls::install_default_crypto_provider();
        Self {
            dns_provider,
            acme_root,
            propagation_wait,
            account: Arc::new(Mutex::new(None)),
        }
    }

    /// The account builder, trusting the configured extra root when one is
    /// set.
    fn account_builder(
        &self,
    ) -> std::result::Result<instant_acme::AccountBuilder, instant_acme::Error> {
        self.acme_root.as_ref().map_or_else(
            instant_acme::Account::builder,
            instant_acme::Account::builder_with_root,
        )
    }
}

/// Total attempts for an ACME operation whose request nonce the server
/// rejects.
const BAD_NONCE_ATTEMPTS: u32 = 3;

/// The error for an authorization that offers no DNS-01 challenge —
/// wildcard orders are validated by DNS-01 only.
fn no_dns01_challenge_error() -> TlsError {
    TlsError::Acme("no DNS-01 challenge offered by server".to_string())
}

/// True when the server rejected the request's anti-replay nonce — the one
/// ACME error RFC 8555 (section 6.5) defines as retryable with the fresh
/// nonce the rejection carries.
fn is_bad_nonce(err: &instant_acme::Error) -> bool {
    matches!(
        err,
        instant_acme::Error::Api(problem)
            if problem.r#type.as_deref() == Some("urn:ietf:params:acme:error:badNonce")
    )
}

/// Await `$op`, retrying while the ACME server rejects the request nonce.
///
/// instant-acme already retries a rejected nonce per HTTP request, but each
/// replacement nonce can itself be rejected — Let's Encrypt does this when
/// a request lands on a frontend whose nonce pool has diverged, and Pebble
/// injects it deliberately (5% of valid nonces by default). This adds an
/// operation-level layer on top, so issuance survives an unlucky streak.
/// Any other failure — and a rejection once the attempts are spent — maps
/// to `TlsError::Acme` prefixed with `$error_context`. A macro rather than
/// a function so `$op` can reborrow the order or challenge handle on every
/// attempt; an `AsyncFnMut` closure doing the same trips rustc's
/// higher-ranked lifetime limits under `async_trait`'s boxed futures.
macro_rules! with_bad_nonce_retry {
    ($error_context:expr, $op:expr) => {{
        let mut attempt = 1;
        loop {
            match $op.await {
                Ok(value) => break Ok(value),
                Err(e) if attempt < BAD_NONCE_ATTEMPTS && is_bad_nonce(&e) => {
                    debug!(
                        "ACME server rejected the request nonce \
                         (attempt {attempt} of {BAD_NONCE_ATTEMPTS}): {}; \
                         retrying with a fresh nonce",
                        $error_context
                    );
                    attempt = attempt.saturating_add(1);
                }
                Err(e) => break Err(TlsError::Acme(format!("{}: {e}", $error_context))),
            }
        }
    }};
}

#[async_trait]
impl AcmeClient for RealAcmeClient<'_> {
    async fn create_or_load_account(
        &self,
        email: String,
        directory_url: String,
        existing_credentials_json: Option<String>,
    ) -> Result<Option<String>> {
        use instant_acme::{AccountCredentials, NewAccount};

        if let Some(json) = existing_credentials_json {
            debug!("Loading ACME account from credentials");
            let credentials: AccountCredentials = serde_json::from_str(&json)
                .map_err(|e| TlsError::Acme(format!("failed to parse account credentials: {e}")))?;
            // Restoring an account sends no signed request (only the
            // unsigned directory fetch), so no nonce can be rejected here.
            let account = self
                .account_builder()
                .map_err(|e| TlsError::Acme(format!("failed to create account builder: {e}")))?
                .from_credentials(credentials)
                .await
                .map_err(|e| TlsError::Acme(format!("failed to load account: {e}")))?;
            *self.account.lock().await = Some(account);
            debug!("Loaded existing ACME account");
            Ok(None)
        } else {
            debug!("Creating new ACME account at {}", directory_url);
            let contact = format!("mailto:{email}");
            let new_account = NewAccount {
                contact: &[&contact],
                terms_of_service_agreed: true,
                only_return_existing: false,
            };

            // Builder construction is local and unsigned; probe it eagerly
            // so its failures keep their own context. `create` consumes a
            // builder, so every attempt below constructs a fresh one — safe,
            // because a rejected registration was never processed
            // server-side.
            self.account_builder()
                .map_err(|e| TlsError::Acme(format!("failed to create account builder: {e}")))?;
            let (account, credentials) =
                with_bad_nonce_retry!("failed to create ACME account", async {
                    self.account_builder()?
                        .create(&new_account, directory_url.clone(), None)
                        .await
                })?;

            *self.account.lock().await = Some(account);

            let json = serde_json::to_string_pretty(&credentials)
                .map_err(|e| TlsError::Acme(format!("failed to serialize credentials: {e}")))?;
            info!("Created new ACME account");
            Ok(Some(json))
        }
    }

    async fn order_certificate(&self, domain: String) -> Result<(String, String)> {
        use instant_acme::{AuthorizationStatus, ChallengeType, Identifier, NewOrder, RetryPolicy};

        let mut account_guard = self.account.lock().await;
        let account = account_guard.as_mut().ok_or_else(|| {
            TlsError::Acme(
                "account not initialized — call create_or_load_account first".to_string(),
            )
        })?;

        // Create order for wildcard domain
        let wildcard = format!("*.{domain}");
        let identifiers = [Identifier::Dns(wildcard.clone())];
        let new_order = NewOrder::new(&identifiers);

        debug!("Creating ACME order for {}", wildcard);
        let mut order =
            with_bad_nonce_retry!("failed to create order", account.new_order(&new_order))?;
        drop(account_guard);

        // Solve DNS-01 challenges
        let challenge_fqdn = format!("_acme-challenge.{domain}");

        // Clean up any leftover challenge records from previous runs
        debug!(
            "Cleaning up any existing challenge records for {}",
            challenge_fqdn
        );
        if let Err(e) = self.dns_provider.delete_txt_record(&challenge_fqdn).await {
            debug!("Pre-cleanup warning (non-fatal): {e}");
        }

        // `Authorizations::next()` advances past an authorization before its
        // state fetch can fail, so a rejected nonce there cannot be retried
        // in place — it would silently skip that authorization. Restart the
        // whole pass instead: fetched authorization states are cached on the
        // order, and identifiers whose challenge is already submitted are
        // skipped, so a restart re-sends nothing.
        let mut fetch_attempt = 1;
        let mut ready_identifiers = std::collections::HashSet::new();
        'pass: loop {
            let mut auths = order.authorizations();
            while let Some(auth_result) = auths.next().await {
                let mut auth = match auth_result {
                    Ok(auth) => auth,
                    Err(e) if fetch_attempt < BAD_NONCE_ATTEMPTS && is_bad_nonce(&e) => {
                        debug!(
                            "ACME server rejected the request nonce \
                             (attempt {fetch_attempt} of {BAD_NONCE_ATTEMPTS}): \
                             failed to get authorization; restarting the authorization pass"
                        );
                        fetch_attempt = fetch_attempt.saturating_add(1);
                        continue 'pass;
                    }
                    Err(e) => {
                        return Err(TlsError::Acme(format!("failed to get authorization: {e}")));
                    }
                };

                match auth.status {
                    AuthorizationStatus::Pending => {}
                    // The server reused a still-valid authorization (common on
                    // renewal); there is nothing to prove for it.
                    AuthorizationStatus::Valid => {
                        debug!("Authorization already valid; skipping its challenge");
                        continue;
                    }
                    other => {
                        return Err(TlsError::Acme(format!(
                            "authorization is {other:?} and cannot be completed"
                        )));
                    }
                }

                let identifier = auth.identifier().to_string();
                if ready_identifiers.contains(&identifier) {
                    continue;
                }

                let mut challenge = auth
                    .challenge(ChallengeType::Dns01)
                    .ok_or_else(no_dns01_challenge_error)?;

                let key_auth = challenge.key_authorization();
                let dns_value = key_auth.dns_value();

                debug!(
                    "Setting up DNS-01 challenge for {} with value {}",
                    challenge_fqdn, dns_value
                );

                self.dns_provider
                    .create_txt_record(&challenge_fqdn, &dns_value)
                    .await?;

                debug!(
                    "Waiting {}s for DNS propagation",
                    self.propagation_wait.as_secs()
                );
                tokio::time::sleep(self.propagation_wait).await;

                with_bad_nonce_retry!("failed to set challenge ready", challenge.set_ready())?;

                debug!("Challenge marked as ready");
                ready_identifiers.insert(identifier);
            }
            break;
        }

        debug!("Polling order until ready");
        with_bad_nonce_retry!(
            "order did not become ready",
            order.poll_ready(&RetryPolicy::default())
        )?;

        debug!("Cleaning up DNS challenge record");
        if let Err(e) = self.dns_provider.delete_txt_record(&challenge_fqdn).await {
            debug!("Warning: failed to clean up DNS record: {e}");
        }

        // Finalize. A retry regenerates the key and CSR — the rejected
        // finalize request was never processed, and the discarded key never
        // left this process.
        debug!("Finalizing order (generating CSR)");
        let private_key_pem = with_bad_nonce_retry!("failed to finalize order", order.finalize())?;

        debug!("Polling for certificate");
        let cert_chain_pem = with_bad_nonce_retry!(
            "failed to retrieve certificate",
            order.poll_certificate(&RetryPolicy::default())
        )?;

        Ok((cert_chain_pem, private_key_pem))
    }
}

/// Issue a wildcard certificate via ACME.
///
/// This is the main entry point for certificate issuance:
/// 1. Creates or loads an ACME account (via `AcmeClient`)
/// 2. Persists new account credentials if created
/// 3. Orders the certificate (via `AcmeClient`)
/// 4. Writes the certificate and private key to disk
///
/// # Errors
///
/// Returns the ACME client's own error if the account cannot be created or
/// loaded or the order fails, and [`TlsError::Io`] if the saved account,
/// the pki directory, or the certificate and key files cannot be read,
/// created, or written. [`TlsError::Other`] covers the two symlink
/// refusals: the account file itself, and the temp sibling each
/// certificate or key file is staged in before the rename (the final path
/// is only ever renamed over, so a symlink there is replaced, not
/// refused).
pub async fn issue_certificate(
    config: &AcmeConfig,
    pki_dir: &Path,
    acme_client: &dyn AcmeClient,
) -> Result<()> {
    let account_path = acme_config::acme_account_path(pki_dir);
    let directory_url = config.resolved_directory_url();

    // Load existing credentials if available
    let existing_creds = if account_path.exists() {
        debug!("Loading ACME account from {}", account_path.display());
        Some(std::fs::read_to_string(&account_path)?)
    } else {
        None
    };

    // Create or load account
    let new_creds = acme_client
        .create_or_load_account(config.email.clone(), directory_url, existing_creds)
        .await?;

    // Persist new credentials if account was just created
    if let Some(creds_json) = new_creds {
        std::fs::create_dir_all(pki_dir)?;
        write_restricted(&account_path, creds_json.as_bytes())?;
        info!(
            "Saved ACME account credentials to {}",
            account_path.display()
        );
    }

    // Order certificate
    let (cert_chain_pem, private_key_pem) =
        acme_client.order_certificate(config.domain.clone()).await?;

    // Write certificate and key (flat pki tree — no certs/ subdirectory).
    // Each file lands via write-then-rename so a service hot-reloading the
    // pair mid-write never reads a torn file.
    std::fs::create_dir_all(pki_dir)?;

    let cert_path = acme_config::acme_cert_path(pki_dir);
    let key_path = acme_config::acme_key_path(pki_dir);

    write_atomic(&cert_path, &cert_chain_pem, false)?;
    write_atomic(&key_path, &private_key_pem, true)?;

    info!("Certificate written to {}", cert_path.display());
    info!("Private key written to {}", key_path.display());

    Ok(())
}

/// Write `contents` to a temp sibling and rename it over `path`.
/// `restrict` applies 0600 to the temp file first, so the final file never
/// exists with open permissions. The temp name carries the pid so two
/// doctor runs never stage into each other's file, and the data is synced
/// to disk before the rename so a crash cannot leave `path` truncated —
/// losing the rename itself is safe (the previous complete file remains),
/// so the parent directory is not synced.
fn write_atomic(path: &Path, contents: &str, restrict: bool) -> Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .ok_or_else(|| {
            TlsError::Other(format!("{} has no file name to write to", path.display()))
        })?;
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    {
        use std::io::Write;
        // Key bytes must never be readable through any fd another process
        // could hold: the staged file is born 0600, not chmod'd after. The
        // certificate branch refuses symlinks too — a planted temp-sibling
        // link must not redirect the write.
        let mut file = if restrict {
            create_restricted(&tmp)?
        } else {
            refuse_symlink(&tmp)?;
            std::fs::File::create(&tmp)?
        };
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn test_config() -> AcmeConfig {
        AcmeConfig {
            email: "test@example.com".to_string(),
            domain: "example.com".to_string(),
            dns_provider: "cloudflare".to_string(),
            dns_credentials: std::collections::HashMap::new(),
            staging: true,
            renewal_days_before_expiry: 30,
            post_renewal_hooks: vec![],
            directory_url: None,
            acme_root: None,
            dns_propagation_seconds: 15,
        }
    }

    #[tokio::test]
    async fn issue_certificate_happy_path_new_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config();

        let mut mock_acme = MockAcmeClient::new();
        mock_acme
            .expect_create_or_load_account()
            .returning(|_, _, _| Ok(Some(r#"{"fake":"credentials"}"#.to_string())));
        mock_acme
            .expect_order_certificate()
            .returning(|_| Ok(("CERT-PEM-DATA".to_string(), "KEY-PEM-DATA".to_string())));

        issue_certificate(&config, dir.path(), &mock_acme)
            .await
            .unwrap();

        // Verify account credentials were saved
        let account_path = acme_config::acme_account_path(dir.path());
        assert!(account_path.exists(), "account credentials should be saved");
        let saved = std::fs::read_to_string(&account_path).unwrap();
        assert_eq!(saved, r#"{"fake":"credentials"}"#);

        // Verify cert and key were written
        let cert_path = acme_config::acme_cert_path(dir.path());
        let key_path = acme_config::acme_key_path(dir.path());
        assert_eq!(std::fs::read_to_string(cert_path).unwrap(), "CERT-PEM-DATA");
        assert_eq!(std::fs::read_to_string(key_path).unwrap(), "KEY-PEM-DATA");
    }

    #[tokio::test]
    async fn issue_certificate_loads_existing_account() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config();

        // Pre-create account credentials file
        let account_path = acme_config::acme_account_path(dir.path());
        std::fs::create_dir_all(account_path.parent().unwrap()).unwrap();
        std::fs::write(&account_path, r#"{"existing":"creds"}"#).unwrap();

        let mut mock_acme = MockAcmeClient::new();
        mock_acme
            .expect_create_or_load_account()
            .withf(|_, _, existing| existing.as_deref() == Some(r#"{"existing":"creds"}"#))
            .returning(|_, _, _| Ok(None));
        mock_acme
            .expect_order_certificate()
            .returning(|_| Ok(("CERT".to_string(), "KEY".to_string())));

        issue_certificate(&config, dir.path(), &mock_acme)
            .await
            .unwrap();

        // Verify cert was still written
        let cert_path = acme_config::acme_cert_path(dir.path());
        assert!(cert_path.exists());
    }

    #[tokio::test]
    async fn issue_certificate_acme_account_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config();

        let mut mock_acme = MockAcmeClient::new();
        mock_acme
            .expect_create_or_load_account()
            .returning(|_, _, _| Err(TlsError::Acme("account creation failed".to_string())));

        let err = issue_certificate(&config, dir.path(), &mock_acme)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("account creation failed"),
            "error: {err}"
        );
    }

    #[tokio::test]
    async fn issue_certificate_order_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config();

        let mut mock_acme = MockAcmeClient::new();
        mock_acme
            .expect_create_or_load_account()
            .returning(|_, _, _| Ok(None));
        mock_acme
            .expect_order_certificate()
            .returning(|_| Err(TlsError::Acme("order failed".to_string())));

        let err = issue_certificate(&config, dir.path(), &mock_acme)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("order failed"), "error: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_certificate_sets_key_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = test_config();

        let mut mock_acme = MockAcmeClient::new();
        mock_acme
            .expect_create_or_load_account()
            .returning(|_, _, _| Ok(None));
        mock_acme
            .expect_order_certificate()
            .returning(|_| Ok(("CERT".to_string(), "KEY".to_string())));

        issue_certificate(&config, dir.path(), &mock_acme)
            .await
            .unwrap();

        let key_path = acme_config::acme_key_path(dir.path());
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "key should have 0600 permissions, got {mode:o}"
        );
    }

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_tmp_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme-cert.pem");
        std::fs::write(&path, "OLD").unwrap();
        write_atomic(&path, "NEW", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "NEW");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temp sibling must be renamed away: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_restricts_before_the_rename() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme-key.pem");
        write_atomic(&path, "KEY", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key mode {mode:o}");
    }

    #[test]
    fn write_atomic_rejects_a_path_without_a_file_name() {
        let err = write_atomic(Path::new("/"), "x", false).unwrap_err();
        assert!(err.to_string().contains("no file name"), "{err}");
    }

    fn bad_nonce_error() -> instant_acme::Error {
        instant_acme::Error::Api(instant_acme::Problem {
            r#type: Some("urn:ietf:params:acme:error:badNonce".to_string()),
            detail: Some("JWS has an invalid anti-replay nonce".to_string()),
            status: Some(400),
            subproblems: vec![],
        })
    }

    #[test]
    fn is_bad_nonce_matches_only_the_bad_nonce_problem_type() {
        assert!(is_bad_nonce(&bad_nonce_error()));
        let other_problem = instant_acme::Error::Api(instant_acme::Problem {
            r#type: Some("urn:ietf:params:acme:error:malformed".to_string()),
            detail: None,
            status: Some(400),
            subproblems: vec![],
        });
        assert!(!is_bad_nonce(&other_problem));
        assert!(!is_bad_nonce(&instant_acme::Error::Timeout(None)));
    }

    #[tokio::test]
    async fn bad_nonce_retry_passes_a_first_try_success_through() {
        let mut calls: u32 = 0;
        let result: Result<u32> = with_bad_nonce_retry!("context", async {
            calls += 1;
            Ok::<_, instant_acme::Error>(7)
        });
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn bad_nonce_retry_retries_rejections_until_success() {
        let mut calls: u32 = 0;
        let result: Result<u32> = with_bad_nonce_retry!("context", async {
            calls += 1;
            if calls < BAD_NONCE_ATTEMPTS {
                Err(bad_nonce_error())
            } else {
                Ok(7)
            }
        });
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, BAD_NONCE_ATTEMPTS);
    }

    #[tokio::test]
    async fn bad_nonce_retry_gives_up_once_the_attempts_are_spent() {
        let mut calls: u32 = 0;
        let result: Result<u32> = with_bad_nonce_retry!("failed to renew the order", async {
            calls += 1;
            Err::<u32, _>(bad_nonce_error())
        });
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to renew the order"), "{msg}");
        assert!(msg.contains("badNonce"), "{msg}");
        assert_eq!(calls, BAD_NONCE_ATTEMPTS);
    }

    #[tokio::test]
    async fn bad_nonce_retry_propagates_other_errors_without_retrying() {
        let mut calls: u32 = 0;
        let result: Result<u32> = with_bad_nonce_retry!("failed to create order", async {
            calls += 1;
            Err::<u32, _>(instant_acme::Error::Timeout(None))
        });
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("failed to create order"), "{msg}");
        assert!(msg.contains("timed out"), "{msg}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn no_dns01_challenge_error_names_the_missing_challenge_type() {
        let msg = no_dns01_challenge_error().to_string();
        assert!(msg.contains("no DNS-01 challenge"), "{msg}");
    }

    #[tokio::test]
    async fn real_acme_client_unreadable_root_fails_account_creation_with_builder_context() {
        let dns = super::super::dns::MockDnsProvider::new();
        let client = RealAcmeClient::new(
            &dns,
            Some(PathBuf::from("/nonexistent/acme-root.pem")),
            Duration::from_secs(15),
        );
        let err = client
            .create_or_load_account(
                "test@example.com".to_string(),
                "https://acme.example.com/directory".to_string(),
                None,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to create account builder"), "{msg}");
    }

    #[tokio::test]
    async fn real_acme_client_unreadable_root_fails_account_load_with_builder_context() {
        let dns = super::super::dns::MockDnsProvider::new();
        let client = RealAcmeClient::new(
            &dns,
            Some(PathBuf::from("/nonexistent/acme-root.pem")),
            Duration::from_secs(15),
        );
        // Parseable credentials ("AAAA" is valid URL-safe base64), so the
        // failure comes from the builder, not the credential parse.
        let credentials_json = r#"{"id": "https://acme.example.com/acct/1", "key_pkcs8": "AAAA", "directory": "https://acme.example.com/directory"}"#;
        let err = client
            .create_or_load_account(
                "test@example.com".to_string(),
                "https://acme.example.com/directory".to_string(),
                Some(credentials_json.to_string()),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to create account builder"), "{msg}");
    }

    #[tokio::test]
    async fn real_acme_client_invalid_credentials_returns_parse_error() {
        let dns = super::super::dns::MockDnsProvider::new();
        let client = RealAcmeClient::new(&dns, None, Duration::from_secs(15));
        let result = client
            .create_or_load_account(
                "test@example.com".to_string(),
                "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
                Some("not valid json".to_string()),
            )
            .await;
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parse"),
            "error should mention parse failure: {msg}"
        );
    }
}

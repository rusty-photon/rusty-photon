//! In-process certificate hot-reload (ADR-002 §Certificate Hot-Reloading).
//!
//! [`ReloadableCertResolver`] implements rustls' `ResolvesServerCert` over a
//! cert/key file pair: on a TLS handshake — throttled to at most one check
//! per check interval — it re-stats both files and reloads the pair when an
//! mtime changed. A pair that fails to load, or whose key does not match its
//! certificate (the torn window between the two file writes, guarded by
//! rustls' `keys_match` inside [`CertifiedKey::from_der`]), is skipped with
//! a debug log and the previous certificate keeps serving until the next
//! check. No file watcher, no signals — the mechanism is identical on every
//! platform and covers certs that arrive by any route.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tracing::debug;

use crate::error::{Result, TlsError};

/// How often, at most, a handshake re-stats the configured pair.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_mins(1);

/// A `ResolvesServerCert` that serves the pair at `cert_path`/`key_path`
/// and hot-swaps it when the files change on disk.
pub struct ReloadableCertResolver {
    cert_path: PathBuf,
    key_path: PathBuf,
    check_interval: Duration,
    current: RwLock<Arc<CertifiedKey>>,
    state: Mutex<ReloadState>,
}

struct ReloadState {
    last_check: Instant,
    cert_mtime: Option<SystemTime>,
    key_mtime: Option<SystemTime>,
}

// CertifiedKey carries private-key material, so only the paths and the
// throttle appear.
impl std::fmt::Debug for ReloadableCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadableCertResolver")
            .field("cert_path", &self.cert_path)
            .field("key_path", &self.key_path)
            .field("check_interval", &self.check_interval)
            .finish_non_exhaustive()
    }
}

impl ReloadableCertResolver {
    /// Load the initial pair, failing loudly on bad material — a server
    /// must never start on a pair it cannot serve. Records the files'
    /// mtimes so the first handshake does not immediately reload.
    ///
    /// # Errors
    ///
    /// Returns a [`crate::error::TlsError`] if either file cannot be
    /// read or the pair does not parse as usable certificate/key
    /// material.
    pub fn load(cert_path: impl Into<PathBuf>, key_path: impl Into<PathBuf>) -> Result<Self> {
        crate::install_default_crypto_provider();
        let cert_path = cert_path.into();
        let key_path = key_path.into();
        let (key, cert_mtime, key_mtime) = load_pair(&cert_path, &key_path)?;
        Ok(Self {
            cert_path,
            key_path,
            check_interval: DEFAULT_CHECK_INTERVAL,
            current: RwLock::new(Arc::new(key)),
            state: Mutex::new(ReloadState {
                last_check: Instant::now(),
                cert_mtime,
                key_mtime,
            }),
        })
    }

    /// Override the throttle — `Duration::ZERO` checks on every handshake
    /// (the test affordance).
    #[must_use]
    pub const fn with_check_interval(mut self, check_interval: Duration) -> Self {
        self.check_interval = check_interval;
        self
    }

    /// The throttled reload check. Contention is skipped (another handshake
    /// is already checking). The throttle advances on every path — success
    /// or failure — so a bad pair is retried next interval, not on every
    /// handshake.
    fn maybe_reload(&self) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.last_check.elapsed() < self.check_interval {
            return;
        }
        state.last_check = Instant::now();
        let cert_mtime = mtime(&self.cert_path);
        let key_mtime = mtime(&self.key_path);
        if cert_mtime == state.cert_mtime && key_mtime == state.key_mtime {
            return;
        }
        match load_pair(&self.cert_path, &self.key_path) {
            Ok((key, cert_mtime, key_mtime)) => {
                *self
                    .current
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(key);
                state.cert_mtime = cert_mtime;
                state.key_mtime = key_mtime;
                debug!(cert = %self.cert_path.display(), "reloaded the TLS certificate pair");
            }
            Err(e) => {
                // The recorded mtimes stay stale on purpose: the next
                // interval sees them differ again and retries the load.
                debug!(
                    cert = %self.cert_path.display(),
                    "new TLS pair failed to load ({e}); keeping the previous pair"
                );
            }
        }
    }

    /// The pair currently being served.
    fn current(&self) -> Arc<CertifiedKey> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ResolvesServerCert for ReloadableCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.maybe_reload();
        Some(self.current())
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Read, parse, and validate the pair ([`CertifiedKey::from_der`] runs
/// rustls' `keys_match`, the torn-write guard). Mtimes are taken before the
/// reads, so a write landing in between causes at most one redundant reload
/// on the next check, never a missed one.
fn load_pair(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(CertifiedKey, Option<SystemTime>, Option<SystemTime>)> {
    let cert_mtime = mtime(cert_path);
    let key_mtime = mtime(key_path);
    let cert_pem = std::fs::read_to_string(cert_path)?;
    let key_pem = std::fs::read_to_string(key_path)?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Pem(format!("failed to parse cert PEM: {e}")))?;
    if certs.is_empty() {
        return Err(TlsError::Pem(format!(
            "no certificate found in {}",
            cert_path.display()
        )));
    }
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| TlsError::Pem(format!("failed to parse key PEM: {e}")))?
        .ok_or_else(|| TlsError::Pem(format!("no private key found in {}", key_path.display())))?;
    let provider = CryptoProvider::get_default()
        .ok_or_else(|| TlsError::Other("no default rustls CryptoProvider installed".to_string()))?;
    let certified = CertifiedKey::from_der(certs, key, provider)?;
    Ok((certified, cert_mtime, key_mtime))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Generate a CA plus a `svc` service pair into a fresh tempdir,
    /// backdating the pair's mtimes so any rewrite is a visible change.
    fn stage_pair() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        crate::test_cert::generate_ca(dir.path()).unwrap();
        regenerate_pair(dir.path());
        let cert = dir.path().join("svc.pem");
        let key = dir.path().join("svc-key.pem");
        backdate(&cert);
        backdate(&key);
        (dir, cert, key)
    }

    /// Re-issue the `svc` pair from the staged CA — a fresh keypair, so the
    /// certificate bytes always differ from the previous issue.
    fn regenerate_pair(dir: &Path) {
        let ca_cert = std::fs::read_to_string(dir.join("ca.pem")).unwrap();
        let ca_key = std::fs::read_to_string(dir.join("ca-key.pem")).unwrap();
        crate::test_cert::generate_service_cert(&ca_cert, &ca_key, "svc", dir).unwrap();
    }

    fn backdate(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_hours(1))
            .unwrap();
    }

    fn cert_der(resolver: &ReloadableCertResolver) -> Vec<u8> {
        resolver.current().end_entity_cert().unwrap().to_vec()
    }

    fn file_der(path: &Path) -> Vec<u8> {
        let pem = std::fs::read_to_string(path).unwrap();
        let der = rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .to_vec();
        der
    }

    #[test]
    fn test_load_serves_the_initial_pair() {
        let (_dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key).unwrap();
        assert_eq!(cert_der(&resolver), file_der(&cert));
    }

    #[test]
    fn test_load_fails_loudly_on_bad_initial_material() {
        let (dir, cert, _key) = stage_pair();
        let missing = ReloadableCertResolver::load(&cert, dir.path().join("absent-key.pem"));
        missing.unwrap_err();

        // A mismatched pair (this cert, the CA's key) must fail keys_match.
        let torn = ReloadableCertResolver::load(&cert, dir.path().join("ca-key.pem"));
        torn.unwrap_err();
    }

    #[test]
    fn test_mtime_change_swaps_the_pair_with_zero_interval() {
        let (dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key)
            .unwrap()
            .with_check_interval(Duration::ZERO);
        let before = cert_der(&resolver);

        regenerate_pair(dir.path());
        resolver.maybe_reload();

        let after = cert_der(&resolver);
        assert_ne!(before, after, "the resolver should serve the new pair");
        assert_eq!(after, file_der(&cert));
    }

    #[test]
    fn test_torn_pair_keeps_the_old_key_and_recovers() {
        let (dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key)
            .unwrap()
            .with_check_interval(Duration::ZERO);
        let before = cert_der(&resolver);
        let old_key_pem = std::fs::read_to_string(&key).unwrap();

        // The torn window: the new cert has landed, the key has not.
        regenerate_pair(dir.path());
        let new_key_pem = std::fs::read_to_string(&key).unwrap();
        std::fs::write(&key, &old_key_pem).unwrap();
        resolver.maybe_reload();
        assert_eq!(
            cert_der(&resolver),
            before,
            "a pair failing keys_match must not be served"
        );

        // The key catches up: the next check swaps.
        std::fs::write(&key, &new_key_pem).unwrap();
        resolver.maybe_reload();
        assert_eq!(cert_der(&resolver), file_der(&cert));
        assert_ne!(cert_der(&resolver), before);
    }

    #[test]
    fn test_debug_output_names_paths_but_no_key_material() {
        let (_dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key).unwrap();
        let debug = format!("{resolver:?}");
        assert!(debug.contains("svc.pem"), "{debug}");
        assert!(!debug.contains("PRIVATE"), "{debug}");
    }

    #[test]
    fn test_load_rejects_a_pem_without_certificates() {
        let (_dir, _cert, key) = stage_pair();
        let err = ReloadableCertResolver::load(&key, &key).unwrap_err();
        assert!(err.to_string().contains("no certificate"), "{err}");
    }

    #[test]
    fn test_reload_skips_while_another_handshake_holds_the_state_lock() {
        let (dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key)
            .unwrap()
            .with_check_interval(Duration::ZERO);
        let before = cert_der(&resolver);

        let guard = resolver.state.try_lock().unwrap();
        regenerate_pair(dir.path());
        resolver.maybe_reload();
        drop(guard);

        assert_eq!(
            cert_der(&resolver),
            before,
            "a contended check must skip, not block"
        );
    }

    #[test]
    fn test_garbage_rewrite_keeps_the_old_pair() {
        let (_dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key)
            .unwrap()
            .with_check_interval(Duration::ZERO);
        let before = cert_der(&resolver);

        std::fs::write(&cert, "not a certificate").unwrap();
        resolver.maybe_reload();

        assert_eq!(
            cert_der(&resolver),
            before,
            "an unparseable rewrite must keep the previous pair"
        );
    }

    #[test]
    fn test_a_large_interval_never_reloads() {
        let (dir, cert, key) = stage_pair();
        let resolver = ReloadableCertResolver::load(&cert, &key)
            .unwrap()
            .with_check_interval(Duration::from_hours(1));
        let before = cert_der(&resolver);

        regenerate_pair(dir.path());
        resolver.maybe_reload();

        assert_eq!(
            cert_der(&resolver),
            before,
            "inside the interval the resolver must not even stat the files"
        );
    }
}

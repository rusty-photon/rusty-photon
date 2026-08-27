use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusty_photon_tls::error::{Result, TlsError};
use rusty_photon_tls::permissions::write_restricted;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// ACME configuration stored at `<config-root>/acme.json`, beside the
/// service configs.
///
/// This is standalone and decoupled from any service config, supporting
/// multi-machine deployments where the ACME client runs on one host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmeConfig {
    /// ACME account email for expiry notifications.
    pub email: String,
    /// Base domain (wildcard cert issued for `*.<domain>`).
    pub domain: String,
    /// DNS provider identifier (e.g., `"cloudflare"`).
    pub dns_provider: String,
    /// Provider-specific credentials; values starting with `$` are read from
    /// environment variables.
    pub dns_credentials: HashMap<String, String>,
    /// Use Let's Encrypt staging endpoint (default: `false`).
    #[serde(default)]
    pub staging: bool,
    /// Days before expiry to trigger renewal (default: `30`).
    #[serde(default = "default_renewal_days")]
    pub renewal_days_before_expiry: u32,
    /// Shell commands to run after successful renewal.
    #[serde(default)]
    pub post_renewal_hooks: Vec<String>,
    /// Full ACME directory URL, overriding the Let's Encrypt endpoints —
    /// an internal ACME CA (step-ca), or Pebble in tests.
    #[serde(default)]
    pub directory_url: Option<String>,
    /// Path to a PEM trust anchor for the ACME server's own TLS endpoint
    /// (private directories are not publicly trusted).
    #[serde(default)]
    pub acme_root: Option<String>,
    /// Wait between writing the DNS-01 TXT record and requesting
    /// validation (default: `15`).
    #[serde(default = "default_dns_propagation_seconds")]
    pub dns_propagation_seconds: u64,
}

impl AcmeConfig {
    /// The directory URL the order flow talks to: an explicit
    /// `directory_url` wins over the Let's Encrypt staging/production pair.
    #[must_use]
    pub fn resolved_directory_url(&self) -> String {
        self.directory_url
            .clone()
            .unwrap_or_else(|| directory_url(self.staging).to_string())
    }
}

const fn default_renewal_days() -> u32 {
    30
}

const fn default_dns_propagation_seconds() -> u64 {
    15
}

/// Path to the ACME account credentials file within the PKI directory.
#[must_use]
pub fn acme_account_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("acme-account.json")
}

/// Path to the ACME wildcard certificate file within the (flat) PKI
/// directory.
#[must_use]
pub fn acme_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("acme-cert.pem")
}

/// Path to the ACME wildcard private key file within the (flat) PKI
/// directory.
#[must_use]
pub fn acme_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("acme-key.pem")
}

/// Load ACME configuration from a JSON file.
///
/// # Errors
///
/// Returns [`TlsError::Io`] if the file cannot be read and
/// [`TlsError::Config`] if it is not a valid `AcmeConfig` document.
pub fn load_acme_config(path: &Path) -> Result<AcmeConfig> {
    debug!("Loading ACME config from {}", path.display());
    let content = std::fs::read_to_string(path)?;
    let config: AcmeConfig =
        serde_json::from_str(&content).map_err(|e| TlsError::Config(format!("{e}")))?;
    Ok(config)
}

/// Save ACME configuration to a JSON file with restricted permissions.
///
/// # Errors
///
/// Returns [`TlsError::Io`] if the parent directory cannot be created or
/// the file cannot be created, restricted, or written, and
/// [`TlsError::Other`] if `path` is a symlink. The serialization step
/// cannot fail for this config shape.
pub fn save_acme_config(config: &AcmeConfig, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| TlsError::Config(format!("failed to serialize ACME config: {e}")))?;
    write_restricted(path, json.as_bytes())?;
    debug!("Saved ACME config to {}", path.display());
    Ok(())
}

/// Expand credential values that start with `$` by reading from the
/// process environment, falling back to `renew_env` for a name the
/// process environment doesn't have.
///
/// `renew_env` is parsed from `<config-root>/renew.env` by
/// [`parse_renew_env`]. The process environment always wins, so an
/// operator can override a `renew.env` value transiently. Literal values
/// (not starting with `$`) are passed through unchanged.
///
/// # Errors
///
/// Returns [`TlsError::Config`] naming the variable and the credential key
/// if a `$`-referenced name is found in neither the process environment
/// (unset, or not Unicode) nor `renew_env`.
pub fn resolve_credentials<S1: std::hash::BuildHasher, S2: std::hash::BuildHasher>(
    creds: &HashMap<String, String, S1>,
    renew_env: &HashMap<String, String, S2>,
) -> Result<HashMap<String, String>> {
    let mut resolved = HashMap::new();
    for (key, value) in creds {
        let resolved_value = if let Some(var_name) = value.strip_prefix('$') {
            std::env::var(var_name)
                .ok()
                .or_else(|| renew_env.get(var_name).cloned())
                .ok_or_else(|| {
                    TlsError::Config(format!(
                        "environment variable '{var_name}' not set (referenced by dns_credentials.{key})"
                    ))
                })?
        } else {
            value.clone()
        };
        resolved.insert(key.clone(), resolved_value);
    }
    Ok(resolved)
}

/// Parse `<config-root>/renew.env` — `KEY=VALUE` per line, blank lines and
/// whole-line `#` comments ignored — into a map for [`resolve_credentials`]
/// to consult.
///
/// Returns an empty map, not an error, when the file is absent.
///
/// This is the unattended path for `$VAR`-indirected `dns_credentials`
/// (ADR-002, docs/services/doctor.md §Renewal): `doctor tls renew` runs off
/// a platform scheduler (systemd timer, launchd interval, a Windows
/// scheduled task) whose process has no inherited shell environment, so
/// without this file `$CLOUDFLARE_API_TOKEN` (or any other indirected
/// credential) cannot resolve at 3am. One file read here, beside
/// `acme.json`, works identically on all three platforms instead of three
/// platform-specific env mechanisms (systemd `EnvironmentFile=`, a launchd
/// `EnvironmentVariables` plist key, a Windows machine-level env var). A
/// missing file is not an error — self-signed installs and literal
/// (non-`$`) credentials never need it.
///
/// Returning a map instead of calling `std::env::set_var` keeps this out of
/// the process-global environment: `doctor tls renew` drives its work on a
/// multi-thread Tokio runtime, and mutating the environment while worker
/// threads exist is a data race with any concurrent read.
///
/// # Errors
///
/// Returns [`TlsError::Config`] if the file exists but cannot be read, or
/// if a line is neither blank, a comment, nor `KEY=VALUE` with a non-empty
/// key — the line errors carry the path and line number.
pub fn parse_renew_env(config_dir: &Path) -> Result<HashMap<String, String>> {
    let path = config_dir.join("renew.env");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(TlsError::Config(format!("{}: {e}", path.display()))),
    };
    let mut vars = HashMap::new();
    for (lineno, raw_line) in content.lines().enumerate() {
        // 1-based for the error messages; saturating only in name (a
        // usize's worth of lines cannot arrive from one file read).
        let display_lineno = lineno.saturating_add(1);
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            TlsError::Config(format!(
                "{}:{}: expected KEY=VALUE, found '{raw_line}'",
                path.display(),
                display_lineno
            ))
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(TlsError::Config(format!(
                "{}:{}: empty variable name",
                path.display(),
                display_lineno
            )));
        }
        vars.insert(key.to_string(), value.trim().to_string());
    }
    debug!(path = %path.display(), count = vars.len(), "parsed renew.env");
    Ok(vars)
}

/// Return the ACME directory URL for Let's Encrypt staging or production.
#[must_use]
pub const fn directory_url(staging: bool) -> &'static str {
    if staging {
        "https://acme-staging-v02.api.letsencrypt.org/directory"
    } else {
        "https://acme-v02.api.letsencrypt.org/directory"
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn acme_config_round_trip_serde() {
        let config = AcmeConfig {
            email: "user@example.com".to_string(),
            domain: "observatory.example.com".to_string(),
            dns_provider: "cloudflare".to_string(),
            dns_credentials: HashMap::from([("api_token".to_string(), "tok123".to_string())]),
            staging: true,
            renewal_days_before_expiry: 30,
            post_renewal_hooks: vec!["scp cert pi:~/".to_string()],
            directory_url: Some("https://localhost:14000/dir".to_string()),
            acme_root: Some("/tmp/pebble-ca.pem".to_string()),
            dns_propagation_seconds: 1,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: AcmeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.email, "user@example.com");
        assert_eq!(deserialized.domain, "observatory.example.com");
        assert_eq!(deserialized.dns_provider, "cloudflare");
        assert_eq!(
            deserialized.dns_credentials.get("api_token").unwrap(),
            "tok123"
        );
        assert!(deserialized.staging);
        assert_eq!(deserialized.renewal_days_before_expiry, 30);
        assert_eq!(deserialized.post_renewal_hooks.len(), 1);
        assert_eq!(
            deserialized.directory_url.as_deref(),
            Some("https://localhost:14000/dir")
        );
        assert_eq!(
            deserialized.acme_root.as_deref(),
            Some("/tmp/pebble-ca.pem")
        );
        assert_eq!(deserialized.dns_propagation_seconds, 1);
    }

    #[test]
    fn acme_config_defaults() {
        // The exact shape a pre-D6b acme.json carries — it must keep parsing
        // with the endpoint/trust/propagation knobs defaulted.
        let json = r#"{
            "email": "user@example.com",
            "domain": "example.com",
            "dns_provider": "cloudflare",
            "dns_credentials": {"api_token": "tok"}
        }"#;
        let config: AcmeConfig = serde_json::from_str(json).unwrap();
        assert!(!config.staging);
        assert_eq!(config.renewal_days_before_expiry, 30);
        assert_eq!(config.post_renewal_hooks, Vec::<String>::new());
        assert_eq!(config.directory_url, None);
        assert_eq!(config.acme_root, None);
        assert_eq!(config.dns_propagation_seconds, 15);
    }

    #[test]
    fn resolved_directory_url_prefers_the_explicit_override() {
        let json = r#"{
            "email": "user@example.com",
            "domain": "example.com",
            "dns_provider": "cloudflare",
            "dns_credentials": {"api_token": "tok"},
            "staging": true,
            "directory_url": "https://localhost:14000/dir"
        }"#;
        let mut config: AcmeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.resolved_directory_url(),
            "https://localhost:14000/dir"
        );
        config.directory_url = None;
        assert_eq!(config.resolved_directory_url(), directory_url(true));
        config.staging = false;
        assert_eq!(config.resolved_directory_url(), directory_url(false));
    }

    #[test]
    fn resolve_credentials_expands_env_var() {
        let _guard = EnvVarGuard::set("TEST_ACME_TOKEN_XYZ", "secret123");
        let creds = HashMap::from([("api_token".to_string(), "$TEST_ACME_TOKEN_XYZ".to_string())]);
        let resolved = resolve_credentials(&creds, &HashMap::new()).unwrap();
        assert_eq!(resolved.get("api_token").unwrap(), "secret123");
    }

    #[test]
    fn resolve_credentials_passes_through_literal() {
        let creds = HashMap::from([("api_token".to_string(), "literal-value".to_string())]);
        let resolved = resolve_credentials(&creds, &HashMap::new()).unwrap();
        assert_eq!(resolved.get("api_token").unwrap(), "literal-value");
    }

    #[test]
    fn resolve_credentials_missing_env_var_returns_error() {
        let creds = HashMap::from([(
            "api_token".to_string(),
            "$NONEXISTENT_VAR_FOR_ACME_TEST".to_string(),
        )]);
        let err = resolve_credentials(&creds, &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("NONEXISTENT_VAR_FOR_ACME_TEST"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn resolve_credentials_falls_back_to_renew_env_when_process_env_is_unset() {
        let _guard = EnvVarGuard::unset("RENEW_ENV_TEST_TOKEN");
        let creds = HashMap::from([("api_token".to_string(), "$RENEW_ENV_TEST_TOKEN".to_string())]);
        let renew_env =
            HashMap::from([("RENEW_ENV_TEST_TOKEN".to_string(), "from-file".to_string())]);

        let resolved = resolve_credentials(&creds, &renew_env).unwrap();

        assert_eq!(resolved.get("api_token").unwrap(), "from-file");
    }

    #[test]
    fn resolve_credentials_prefers_process_env_over_renew_env() {
        let _guard = EnvVarGuard::set("RENEW_ENV_TEST_PRESET", "from-environment");
        let creds = HashMap::from([(
            "api_token".to_string(),
            "$RENEW_ENV_TEST_PRESET".to_string(),
        )]);
        let renew_env =
            HashMap::from([("RENEW_ENV_TEST_PRESET".to_string(), "from-file".to_string())]);

        let resolved = resolve_credentials(&creds, &renew_env).unwrap();

        assert_eq!(resolved.get("api_token").unwrap(), "from-environment");
    }

    /// Restores its key's prior value (or absence) in the process
    /// environment on drop, including during an unwinding panic (e.g. a
    /// failed `assert_eq!`) — plain `remove_var` calls at the end of a
    /// test body never run in that case and leak the mutation into later
    /// tests sharing the process.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn parse_renew_env_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse_renew_env(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn parse_renew_env_parses_key_value_lines_ignoring_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("renew.env"),
            "# a comment\n\nRENEW_ENV_TEST_TOKEN=from-file\n",
        )
        .unwrap();

        let vars = parse_renew_env(dir.path()).unwrap();

        assert_eq!(vars.get("RENEW_ENV_TEST_TOKEN").unwrap(), "from-file");
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn parse_renew_env_rejects_a_line_without_equals() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("renew.env"), "not-a-valid-line\n").unwrap();

        let err = parse_renew_env(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("KEY=VALUE"),
            "error should explain the expected shape: {err}"
        );
    }

    #[test]
    fn parse_renew_env_rejects_an_empty_variable_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("renew.env"), "=some-value\n").unwrap();

        let err = parse_renew_env(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("empty variable name"),
            "error should name the problem: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parse_renew_env_surfaces_a_non_notfound_read_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("renew.env"), "KEY=value\n").unwrap();
        // Strip search permission from the parent dir so read_to_string
        // fails with EACCES rather than NotFound — the loop must
        // distinguish "absent" (tolerated) from "unreadable" (an error).
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = parse_renew_env(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        // Ok means running privileged (e.g. root): DAC checks bypassed, so
        // read_to_string still succeeded.
        if let Err(e) = result {
            assert!(
                e.to_string().contains("renew.env"),
                "error should name the file: {e}"
            );
        }
    }

    #[test]
    fn directory_url_staging() {
        let url = directory_url(true);
        assert!(url.contains("staging"), "staging URL: {url}");
    }

    #[test]
    fn directory_url_production() {
        let url = directory_url(false);
        assert!(!url.contains("staging"), "production URL: {url}");
        assert!(url.contains("acme-v02"), "production URL: {url}");
    }

    #[test]
    fn path_helpers_return_flat_pki_paths() {
        let pki_dir = Path::new("/var/lib/rusty-photon/.config/rusty-photon/pki");
        assert_eq!(
            acme_account_path(pki_dir),
            PathBuf::from("/var/lib/rusty-photon/.config/rusty-photon/pki/acme-account.json")
        );
        assert_eq!(
            acme_cert_path(pki_dir),
            PathBuf::from("/var/lib/rusty-photon/.config/rusty-photon/pki/acme-cert.pem")
        );
        assert_eq!(
            acme_key_path(pki_dir),
            PathBuf::from("/var/lib/rusty-photon/.config/rusty-photon/pki/acme-key.pem")
        );
    }

    #[test]
    fn load_acme_config_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.json");
        let json = r#"{
            "email": "user@example.com",
            "domain": "example.com",
            "dns_provider": "cloudflare",
            "dns_credentials": {"api_token": "tok"}
        }"#;
        std::fs::write(&path, json).unwrap();
        let config = load_acme_config(&path).unwrap();
        assert_eq!(config.email, "user@example.com");
    }

    #[test]
    fn load_acme_config_invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.json");
        std::fs::write(&path, "not json").unwrap();
        let result = load_acme_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn save_and_load_acme_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.json");

        let config = AcmeConfig {
            email: "test@example.com".to_string(),
            domain: "test.example.com".to_string(),
            dns_provider: "cloudflare".to_string(),
            dns_credentials: HashMap::from([("api_token".to_string(), "tok".to_string())]),
            staging: true,
            renewal_days_before_expiry: 15,
            post_renewal_hooks: vec![],
            directory_url: None,
            acme_root: None,
            dns_propagation_seconds: 15,
        };

        save_acme_config(&config, &path).unwrap();
        let loaded = load_acme_config(&path).unwrap();
        assert_eq!(loaded.email, "test@example.com");
        assert_eq!(loaded.domain, "test.example.com");
        assert!(loaded.staging);
        assert_eq!(loaded.renewal_days_before_expiry, 15);
    }
}

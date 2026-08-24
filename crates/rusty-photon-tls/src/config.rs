use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// TLS configuration for a service endpoint.
///
/// When present in a service config, the service will serve over HTTPS.
/// When absent (`None`), the service runs plain HTTP.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TlsConfig {
    /// Path to the PEM-encoded certificate file
    pub cert: String,
    /// Path to the PEM-encoded private key file
    pub key: String,
}

/// Expand a leading `~` to the user's home directory.
#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(not(any(unix, windows)))]
compile_error!(
    "rusty-photon supports unix and windows targets only; please open a GitHub issue at \
     https://github.com/rusty-photon/rusty-photon/issues naming the platform you need"
);

/// Returns the user's home directory, or `None` if it cannot be determined.
fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
}

impl TlsConfig {
    /// Resolve cert and key paths, expanding `~` to the home directory.
    #[must_use]
    pub fn resolved_cert_path(&self) -> PathBuf {
        expand_tilde(&self.cert)
    }

    /// Resolve cert and key paths, expanding `~` to the home directory.
    #[must_use]
    pub fn resolved_key_path(&self) -> PathBuf {
        expand_tilde(&self.key)
    }
}

/// CA cert and key filenames within the PKI directory.
#[must_use]
pub fn ca_cert_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("ca.pem")
}

#[must_use]
pub fn ca_key_path(pki_dir: &Path) -> PathBuf {
    pki_dir.join("ca-key.pem")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tls_config_from_json() {
        let json = r#"{"cert": "/path/to/cert.pem", "key": "/path/to/key.pem"}"#;
        let config: TlsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.cert, "/path/to/cert.pem");
        assert_eq!(config.key, "/path/to/key.pem");
    }

    #[test]
    fn optional_tls_config_defaults_to_none() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            tls: Option<TlsConfig>,
        }

        let json = r"{}";
        let w: Wrapper = serde_json::from_str(json).unwrap();
        assert!(w.tls.is_none());
    }

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/pki/ca.pem");
        // Should not start with ~ after expansion (assuming HOME is set)
        if std::env::var_os("HOME").is_some() || std::env::var_os("USERPROFILE").is_some() {
            assert!(!expanded.starts_with("~"));
        }
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let path = "/absolute/path/to/cert.pem";
        assert_eq!(expand_tilde(path), PathBuf::from(path));
    }

    #[test]
    fn resolved_paths_expand_tilde() {
        let config = TlsConfig {
            cert: "~/pki/rp.pem".to_string(),
            key: "~/pki/rp-key.pem".to_string(),
        };
        if std::env::var_os("HOME").is_some() || std::env::var_os("USERPROFILE").is_some() {
            assert!(!config.resolved_cert_path().starts_with("~"));
            assert!(!config.resolved_key_path().starts_with("~"));
        }
    }
}

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Image-cache bounds (rp.md § Image and Document Cache → Lifetime and
/// Eviction). Pi-5-friendly defaults.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagingConfig {
    #[serde(default = "default_cache_max_mib")]
    pub cache_max_mib: usize,
    #[serde(default = "default_cache_max_images")]
    pub cache_max_images: usize,
}

impl Default for ImagingConfig {
    fn default() -> Self {
        Self {
            cache_max_mib: default_cache_max_mib(),
            cache_max_images: default_cache_max_images(),
        }
    }
}

const fn default_cache_max_mib() -> usize {
    1024
}

const fn default_cache_max_images() -> usize {
    8
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    #[test]
    fn imaging_config_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "imaging": {"cache_max_mib": 256, "cache_max_images": 4},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.imaging.cache_max_mib, 256);
        assert_eq!(config.imaging.cache_max_images, 4);
    }

    #[test]
    fn imaging_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "imaging": {"cache_max_mib": 256, "cache_ttl_secs": 60},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("cache_ttl_secs"), "{err}");
    }
}

//! Helpers for launching rp (or any plugin) from a JSON config Value.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::scratch::scratch_dir;
use crate::ServiceHandle;

/// Per-process counter so each call to [`write_temp_config_file`] produces a
/// distinct path inside this process's [`scratch_dir`], matching
/// [`RpConfigBuilder::build`](super::config::RpConfigBuilder::build) for
/// `data_directory` / `session_state_file`. Cross-process uniqueness is the
/// scratch directory's job.
static CONFIG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write a `serde_json::Value` to a uniquely-named file in this process's
/// scratch directory and return its path as a `String`.
///
/// The `prefix` disambiguates configs across services (e.g. `"rp-test-config"`
/// vs `"calibrator-flats-config"`); the monotonic sequence keeps concurrent
/// calls apart even under coarse system clocks.
pub async fn write_temp_config_file(prefix: &str, config: &Value) -> String {
    let seq = CONFIG_SEQ.fetch_add(1, Ordering::Relaxed);
    let config_path = scratch_dir()
        .join(format!("{prefix}-{seq}.json"))
        .to_string_lossy()
        .to_string();
    tokio::fs::write(&config_path, serde_json::to_string_pretty(config).unwrap())
        .await
        .unwrap_or_else(|e| panic!("failed to write temp config '{config_path}': {e}"));
    config_path
}

/// Start rp with the given config. Returns the [`ServiceHandle`].
///
/// The caller is responsible for calling [`wait_for_rp_healthy`] afterwards
/// if they need to block until rp is serving requests.
pub async fn start_rp(config: &Value) -> ServiceHandle {
    let config_path = write_temp_config_file("rp-test-config", config).await;
    ServiceHandle::start("rp", &config_path).await
}

/// Poll `GET <rp_base_url>/health` until it returns 200, up to 30 seconds.
/// Returns `true` if rp became healthy, `false` on timeout.
pub async fn wait_for_rp_healthy(rp_base_url: &str) -> bool {
    let client = reqwest::Client::new();
    let url = format!("{rp_base_url}/health");
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().as_u16() == 200 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_temp_config_file_produces_readable_json() {
        let config = serde_json::json!({ "foo": "bar", "n": 42 });
        let path = write_temp_config_file("bdd-infra-test", &config).await;

        let bytes = tokio::fs::read(&path).await.unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, config);
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn write_temp_config_file_paths_are_unique_across_calls() {
        let config = serde_json::json!({ "k": 1 });
        let a = write_temp_config_file("bdd-infra-unique", &config).await;
        let b = write_temp_config_file("bdd-infra-unique", &config).await;
        assert_ne!(a, b);
        let _ = tokio::fs::remove_file(&a).await;
        let _ = tokio::fs::remove_file(&b).await;
    }

    #[tokio::test]
    async fn write_temp_config_file_writes_into_the_scratch_dir() {
        let path = write_temp_config_file("bdd-infra-scratch", &serde_json::json!({})).await;
        let path = std::path::PathBuf::from(path);
        assert_eq!(path.parent().unwrap(), scratch_dir());
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("bdd-infra-scratch-"));
        let _ = tokio::fs::remove_file(&path).await;
    }
}

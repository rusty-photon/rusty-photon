//! rp's non-config REST surface: live equipment status.
//!
//! The equipment page joins `GET /api/equipment` (live `{id, connected}` per
//! device) with rp's *config* (the authoritative roster, read through the
//! [`ConfigClient`](crate::driver_client::ConfigClient) REST transport); the
//! stream page reads the same endpoint for its LED panel and to learn
//! whether rp is reachable. This module owns the wire mirror of that
//! endpoint (`rp.md` "REST Endpoints") behind the mockable [`RpApi`] trait.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::driver_client::ConfigClientError;
use crate::io::HttpClient;

/// Live status of one configured device — the per-entry shape of
/// `GET /api/equipment` (`rp.md`).
///
/// It carries the operator-supplied config `id` plus the connection
/// state; addresses and settings live in rp's config, not here.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStatus {
    pub id: String,
    pub connected: bool,
}

/// The singular mount's status — no `id` (rp's mount is one-per-observatory).
#[derive(Debug, Clone, Deserialize)]
pub struct MountStatus {
    pub connected: bool,
}

/// `GET /api/equipment`: live connection state per configured device, mirroring
/// the config's equipment shape. All ten keys are always present; `mount` is
/// `null` when none is configured.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EquipmentStatus {
    #[serde(default)]
    pub cameras: Vec<DeviceStatus>,
    #[serde(default)]
    pub filter_wheels: Vec<DeviceStatus>,
    #[serde(default)]
    pub cover_calibrators: Vec<DeviceStatus>,
    #[serde(default)]
    pub focusers: Vec<DeviceStatus>,
    #[serde(default)]
    pub safety_monitors: Vec<DeviceStatus>,
    #[serde(default)]
    pub switches: Vec<DeviceStatus>,
    #[serde(default)]
    pub rotators: Vec<DeviceStatus>,
    #[serde(default)]
    pub observing_conditions: Vec<DeviceStatus>,
    #[serde(default)]
    pub domes: Vec<DeviceStatus>,
    #[serde(default)]
    pub mount: Option<MountStatus>,
}

impl EquipmentStatus {
    /// The live `connected` flag for a device by equipment kind + config id
    /// (`None` when rp doesn't know the device — e.g. a roster entry added to
    /// the config after rp last started).
    #[must_use]
    pub fn connected(&self, kind_key: &str, id: &str) -> Option<bool> {
        if kind_key == "mount" {
            return self.mount.as_ref().map(|m| m.connected);
        }
        let list = match kind_key {
            "cameras" => &self.cameras,
            "filter_wheels" => &self.filter_wheels,
            "cover_calibrators" => &self.cover_calibrators,
            "focusers" => &self.focusers,
            "safety_monitors" => &self.safety_monitors,
            "switches" => &self.switches,
            "rotators" => &self.rotators,
            "observing_conditions" => &self.observing_conditions,
            "domes" => &self.domes,
            _ => return None,
        };
        list.iter().find(|d| d.id == id).map(|d| d.connected)
    }
}

/// rp's non-config REST surface, behind a trait so page handlers can be unit
/// tested with canned responses (mirrors the `ConfigClient` seam).
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait RpApi: Send + Sync {
    /// `GET /api/equipment` — live connection state of the configured roster.
    async fn equipment_status(&self) -> Result<EquipmentStatus, ConfigClientError>;
}

/// Production [`RpApi`] over the shared [`HttpClient`] (rusty-photon-tls CA trust +
/// optional Basic auth — the same client the `RestConfigClient` uses).
pub struct RestRpApi {
    http: Arc<dyn HttpClient>,
    base_url: String,
}

impl RestRpApi {
    pub fn new(http: Arc<dyn HttpClient>, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, ConfigClientError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http
            .get(&url)
            .await
            .map_err(|e| ConfigClientError::Transport(e.to_string()))?;
        if !(200..300).contains(&response.status) {
            let detail = response.body.chars().take(200).collect::<String>();
            return Err(ConfigClientError::Transport(format!(
                "HTTP {} from {url}: {detail}",
                response.status
            )));
        }
        serde_json::from_str(&response.body).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }
}

#[async_trait]
impl RpApi for RestRpApi {
    async fn equipment_status(&self) -> Result<EquipmentStatus, ConfigClientError> {
        self.get_json("/api/equipment").await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::io::{HttpResponse, MockHttpClient};
    use serde_json::json;

    fn api_returning(url_suffix: &'static str, body: serde_json::Value) -> RestRpApi {
        let mut http = MockHttpClient::new();
        http.expect_get()
            .withf(move |url| url.ends_with(url_suffix))
            .returning(move |_| {
                let body = body.to_string();
                Box::pin(async move { Ok(HttpResponse { status: 200, body }) })
            });
        RestRpApi::new(Arc::new(http), "http://rp:11115/")
    }

    #[tokio::test]
    async fn equipment_status_parses_all_kinds_and_null_mount() {
        let api = api_returning(
            "/api/equipment",
            json!({
                "cameras": [ { "id": "main-cam", "connected": true } ],
                "filter_wheels": [],
                "cover_calibrators": [ { "id": "flat", "connected": false } ],
                "focusers": [],
                "safety_monitors": [],
                "mount": null
            }),
        );
        let status = api.equipment_status().await.unwrap();
        assert_eq!(status.connected("cameras", "main-cam"), Some(true));
        assert_eq!(status.connected("cover_calibrators", "flat"), Some(false));
        assert_eq!(status.connected("cameras", "unknown"), None);
        assert_eq!(status.connected("mount", "mount"), None);
    }

    #[tokio::test]
    async fn equipment_status_maps_the_singular_mount() {
        let api = api_returning(
            "/api/equipment",
            json!({
                "cameras": [], "filter_wheels": [], "cover_calibrators": [],
                "focusers": [], "safety_monitors": [],
                "mount": { "connected": true }
            }),
        );
        let status = api.equipment_status().await.unwrap();
        assert_eq!(status.connected("mount", "anything"), Some(true));
    }

    #[tokio::test]
    async fn non_2xx_is_a_transport_error() {
        let mut http = MockHttpClient::new();
        http.expect_get().returning(|_| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 503,
                    body: "unavailable".to_string(),
                })
            })
        });
        let api = RestRpApi::new(Arc::new(http), "http://rp:11115");
        let err = api.equipment_status().await.unwrap_err();
        assert!(matches!(err, ConfigClientError::Transport(_)), "{err:?}");
    }

    #[test]
    fn unknown_kind_key_is_none() {
        let status = EquipmentStatus::default();
        assert_eq!(status.connected("rotators", "x"), None);
    }
}

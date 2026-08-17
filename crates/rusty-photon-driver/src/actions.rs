//! The generic ASCOM config-action dispatch shared by every driver.
//!
//! [`dispatch`] routes a vendor `config.get` / `config.apply` / `config.schema`
//! action against a driver's [`ConfigActionCtx`], delegating the actual work to
//! the driver-agnostic [`rusty_photon_config::actions`] functions and mapping
//! their outcomes (and [`ApplyError`](rusty_photon_config::actions::ApplyError))
//! to `ASCOMResult`. The fire-after-response reload — the one piece that must run
//! *after* the apply response flushes so the very server serving the request can
//! be torn down — is spawned here on a short delay.

use std::path::PathBuf;
use std::time::Duration;

use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use rusty_photon_config::actions::{self, ApplyStatus, ConfigAction, ConfigurableDriver};
use rusty_photon_service_lifecycle::ReloadSignal;
use strum::VariantArray;
use tracing::debug;

/// Delay before firing the in-process reload, so the `config.apply` HTTP response
/// flushes before the server that served it is torn down by the reload.
pub const RELOAD_AFTER_RESPONSE_DELAY: Duration = Duration::from_millis(100);

/// State the config actions need, cloned across a driver's device(s).
///
/// Generic over the [`ConfigurableDriver`] so the `effective` config and
/// `overrides` carry the driver's own types (`overrides` is zero-sized for
/// drivers whose `Overrides = ()`).
pub struct ConfigActionCtx<D: ConfigurableDriver> {
    /// Effective config (file + CLI overrides) this server was built with.
    pub effective: D::Config,
    /// Where `config.apply` persists; what reload re-reads.
    pub path: PathBuf,
    /// CLI overrides, so config actions distinguish file vs. override layers.
    pub overrides: D::Overrides,
    /// Fired (after the response flushes) to trigger the in-process reload.
    pub reload: ReloadSignal,
}

// Manual `Clone` (not derived): the derive would bound `D: Clone` rather than the
// associated `D::Config`/`D::Overrides` that the fields actually require.
impl<D: ConfigurableDriver> Clone for ConfigActionCtx<D>
where
    D::Config: Clone,
    D::Overrides: Clone,
{
    fn clone(&self) -> Self {
        Self {
            effective: self.effective.clone(),
            path: self.path.clone(),
            overrides: self.overrides.clone(),
            reload: self.reload.clone(),
        }
    }
}

/// The supported-actions list for a device that may or may not carry a config
/// context (ctx-less focused-test devices advertise none).
pub fn supported_actions<D: ConfigurableDriver>(ctx: &Option<ConfigActionCtx<D>>) -> Vec<String> {
    if ctx.is_some() {
        ConfigAction::VARIANTS
            .iter()
            .map(|action| action.name().to_string())
            .collect()
    } else {
        Vec::new()
    }
}

/// Dispatch a vendor config action against the shared config context.
///
/// Unknown actions and a missing context both surface as
/// `ACTION_NOT_IMPLEMENTED` (the device advertises no config actions without a
/// context). Apply fires the in-process reload *after* returning when the apply
/// classified at least one reloadable field (`status: applying`).
pub async fn dispatch<D: ConfigurableDriver>(
    ctx: &Option<ConfigActionCtx<D>>,
    action: String,
    parameters: String,
) -> ASCOMResult<String> {
    let Some(parsed) = ConfigAction::from_name(&action) else {
        return Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("unknown action {action:?}"),
        ));
    };
    let ctx = ctx.as_ref().ok_or_else(|| {
        ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            "config actions are not configured for this device instance",
        )
    })?;

    match parsed {
        ConfigAction::Get => {
            let response = actions::config_get::<D>(&ctx.effective, &ctx.overrides)
                .map_err(|e| serialization_error(&e))?;
            serde_json::to_string(&response).map_err(|e| serialization_error(&e))
        }
        ConfigAction::Schema => {
            let response = actions::config_schema::<D>();
            serde_json::to_string(&response).map_err(|e| serialization_error(&e))
        }
        ConfigAction::Apply => {
            let response =
                actions::config_apply::<D>(&ctx.path, &ctx.overrides, &ctx.effective, &parameters)
                    .map_err(|e| crate::error::apply_error_to_ascom(&e))?;

            if matches!(response.status, ApplyStatus::Applying) {
                let reload = ctx.reload.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(RELOAD_AFTER_RESPONSE_DELAY).await;
                    debug!("firing in-process reload after config.apply");
                    reload.notify();
                });
            }

            serde_json::to_string(&response).map_err(|e| serialization_error(&e))
        }
    }
}

/// Map an internal (de)serialization failure to an ASCOM operation error. Used
/// for `config.get`'s redaction round-trip and the response-encoding steps that
/// should never fail in practice.
fn serialization_error(e: &serde_json::Error) -> ASCOMError {
    ASCOMError::new(
        ASCOMErrorCode::INVALID_OPERATION,
        format!("config: serialization error: {e}"),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rusty_photon_config::actions::FieldError;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
    struct FakeConfig {
        name: String,
        value: i64,
    }

    struct FakeDriver;

    impl ConfigurableDriver for FakeDriver {
        type Config = FakeConfig;
        type Overrides = ();

        fn normalize(_config: &mut FakeConfig) {}

        fn validate(config: &FakeConfig) -> Vec<FieldError> {
            if config.name.is_empty() {
                vec![FieldError {
                    path: "name".into(),
                    msg: "must not be empty".into(),
                }]
            } else {
                Vec::new()
            }
        }

        fn secret_pointers() -> &'static [&'static str] {
            &[]
        }

        fn override_paths(_overrides: &()) -> Vec<String> {
            Vec::new()
        }

        fn apply_overrides(_config: &mut FakeConfig, _overrides: &()) {}
    }

    fn ctx_with(effective: FakeConfig, path: PathBuf) -> ConfigActionCtx<FakeDriver> {
        ConfigActionCtx {
            effective,
            path,
            overrides: (),
            reload: ReloadSignal::new(),
        }
    }

    #[tokio::test]
    async fn unknown_action_is_not_implemented() {
        let ctx = Some(ctx_with(
            FakeConfig::default(),
            PathBuf::from("/tmp/unused"),
        ));
        let err = dispatch::<FakeDriver>(&ctx, "frobnicate".into(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn action_name_matching_is_case_sensitive() {
        let ctx = Some(ctx_with(
            FakeConfig::default(),
            PathBuf::from("/tmp/unused"),
        ));
        for wrong_case in ["Config.Get", "CONFIG.GET", "config.Get"] {
            let err = dispatch::<FakeDriver>(&ctx, wrong_case.into(), String::new())
                .await
                .unwrap_err();
            assert_eq!(
                err.code,
                ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
                "{wrong_case:?} must not dispatch"
            );
        }
    }

    #[tokio::test]
    async fn missing_ctx_is_not_implemented() {
        let none: Option<ConfigActionCtx<FakeDriver>> = None;
        let err = dispatch::<FakeDriver>(&none, "config.get".into(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }

    #[test]
    fn supported_actions_some_lists_all_none_empty() {
        let ctx = Some(ctx_with(
            FakeConfig::default(),
            PathBuf::from("/tmp/unused"),
        ));
        // These strings go out on the wire in SupportedActions.
        assert_eq!(
            supported_actions::<FakeDriver>(&ctx),
            vec![
                "config.get".to_string(),
                "config.apply".to_string(),
                "config.schema".to_string(),
            ]
        );
        let none: Option<ConfigActionCtx<FakeDriver>> = None;
        assert!(supported_actions::<FakeDriver>(&none).is_empty());
    }

    #[tokio::test]
    async fn get_returns_effective_config() {
        let ctx = Some(ctx_with(
            FakeConfig {
                name: "panel".into(),
                value: 7,
            },
            PathBuf::from("/tmp/unused"),
        ));
        let json = dispatch::<FakeDriver>(&ctx, "config.get".into(), String::new())
            .await
            .unwrap();
        assert!(json.contains("\"panel\""));
        assert!(json.contains("\"overrides\""));
    }

    #[tokio::test]
    async fn schema_returns_schema_and_tiers() {
        let ctx = Some(ctx_with(
            FakeConfig::default(),
            PathBuf::from("/tmp/unused"),
        ));
        let json = dispatch::<FakeDriver>(&ctx, "config.schema".into(), String::new())
            .await
            .unwrap();
        assert!(json.contains("\"schema\""));
        assert!(json.contains("\"read_only_fields\""));
    }

    #[tokio::test]
    async fn apply_invalid_returns_status_invalid_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let ctx = Some(ctx_with(
            FakeConfig {
                name: "panel".into(),
                value: 1,
            },
            path.clone(),
        ));
        // Empty name fails FakeDriver::validate.
        let submitted = serde_json::json!({ "name": "", "value": 2 }).to_string();
        let json = dispatch::<FakeDriver>(&ctx, "config.apply".into(), submitted)
            .await
            .unwrap();
        assert!(json.contains("\"invalid\""));
        assert!(!path.exists(), "invalid apply must not persist");
    }

    #[tokio::test]
    async fn apply_malformed_json_is_invalid_value() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Some(ctx_with(
            FakeConfig::default(),
            dir.path().join("config.json"),
        ));
        let err = dispatch::<FakeDriver>(&ctx, "config.apply".into(), "{ not json".into())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn apply_valid_persists_and_fires_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let reload = ReloadSignal::new();
        let ctx = Some(ConfigActionCtx::<FakeDriver> {
            effective: FakeConfig {
                name: "panel".into(),
                value: 1,
            },
            path: path.clone(),
            overrides: (),
            reload: reload.clone(),
        });
        let submitted = serde_json::json!({ "name": "panel", "value": 42 }).to_string();
        let json = dispatch::<FakeDriver>(&ctx, "config.apply".into(), submitted)
            .await
            .unwrap();
        assert!(json.contains("\"applying\"") || json.contains("\"ok\""));
        assert!(path.exists(), "valid apply must persist");
        // The reload fires after RELOAD_AFTER_RESPONSE_DELAY; give it a window.
        tokio::time::timeout(Duration::from_secs(5), reload.recv())
            .await
            .expect("reload should fire after a valid apply");
    }
}

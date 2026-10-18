//! Pegasus Scops OAG focuser device implementation.
//!
//! Implements the ASCOM Alpaca `Device` + `Focuser` traits. Connection state is
//! the device's `Session<ScopsCodec>` slot — when it's `Some`, we hold a live
//! handle to the shared transport; when it's `None`, we don't. The session
//! existing **is** the "Connected" state.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Focuser};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use async_trait::async_trait;
use rusty_photon_setup::SetupPages;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::ScopsCodec;
use crate::config::FocuserConfig;
use crate::error::ScopsOagError;
use crate::manager::FocuserManager;
use crate::setup::{ScopsFocuserDriver, DEVICE_TYPE};

/// Guard macro that returns `NOT_CONNECTED` if the device is not connected.
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|connected| connected) {
            debug!("Focuser device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// Pegasus Scops OAG focuser device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct ScopsFocuserDevice {
    config: FocuserConfig,
    /// `Some` between successful connect and explicit disconnect. The session
    /// existing is the truth — no second-source bool to desync.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<ScopsCodec>>>>,
    #[debug(skip)]
    manager: Arc<FocuserManager>,
    /// The setup pages behind `/setup/v1/focuser/0/setup`: `Some` when the
    /// driver was built with a config source (the normal path through
    /// `ServerBuilder`); `None` for focused unit-test devices that don't
    /// exercise the page.
    #[debug(skip)]
    pages: Option<Arc<SetupPages<ScopsFocuserDriver>>>,
}

impl ScopsFocuserDevice {
    #[must_use]
    pub fn new(config: FocuserConfig, manager: Arc<FocuserManager>) -> Self {
        Self {
            config,
            session: Arc::new(RwLock::new(None)),
            manager,
            pages: None,
        }
    }

    /// Attach the shared setup pages, so the device's setup page renders and
    /// saves the `focuser` section.
    #[must_use]
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<ScopsFocuserDriver>>) -> Self {
        self.pages = Some(pages);
        self
    }

    /// Borrow the held session for one request. Returns `NOT_CONNECTED` if
    /// the device's session slot is empty.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the session reference borrows the read guard, which is deliberately held across the device I/O so a disconnect's write lock waits out in-flight commands"
    )]
    async fn with_session<F, T>(&self, f: F) -> ASCOMResult<T>
    where
        F: AsyncFnOnce(&Session<ScopsCodec>) -> Result<T, ScopsOagError>,
    {
        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;
        Ok(f(session).await?)
    }
}

#[async_trait]
impl Device for ScopsFocuserDevice {
    fn static_name(&self) -> &str {
        &self.config.name
    }

    fn unique_id(&self) -> &str {
        &self.config.unique_id
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.config.description.clone())
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.session.read().await.is_some() && self.manager.is_available())
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the write lock deliberately spans the whole check-and-modify so two concurrent connects cannot both observe an empty slot and double-acquire"
    )]
    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // The write lock spans the whole check-and-modify so two concurrent
        // `Connected=true` requests can't both observe `None` and both call
        // `acquire()` (issue #250).
        let mut slot = self.session.write().await;
        match (connected, slot.is_some()) {
            (true, false) => {
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(ScopsOagError::from)?;
                *slot = Some(session);
                debug!("Focuser device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    session.close().await.map_err(ScopsOagError::from)?;
                }
                debug!("Focuser device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(
            "Pegasus Scops OAG Focuser - ASCOM Alpaca driver for the Pegasus Astro Scops OAG"
                .to_string(),
        )
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    /// No actions: the configuration lives on the setup pages. An unknown
    /// action is `ActionNotImplemented`, not the generic `NotImplemented`
    /// the crate defaults to.
    async fn action(&self, action: String, _parameters: String) -> ASCOMResult<String> {
        Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("this device has no action {action:?}"),
        ))
    }

    /// The device's setup page, `/setup/v1/focuser/0/setup`, for `GET` and
    /// `POST`, through the shared handler. A device built without the pages
    /// (a focused unit test) has none.
    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SetupResponse> + Send + '_>> {
        match &self.pages {
            Some(pages) => {
                rusty_photon_driver::device_setup_request(pages, DEVICE_TYPE, 0, request)
            }
            None => Box::pin(async {
                SetupResponse::text(
                    axum::http::StatusCode::NOT_FOUND,
                    "This device was built without its setup pages",
                )
            }),
        }
    }
}

#[async_trait]
impl Focuser for ScopsFocuserDevice {
    async fn absolute(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn is_moving(&self) -> ASCOMResult<bool> {
        ensure_connected!(self);

        // Actively refresh status so move-completion is observable without
        // waiting up to one polling interval.
        let cached = self.manager.get_cached_state().await;
        if cached.is_moving {
            self.with_session(async |session| self.manager.refresh_status(session).await)
                .await?;
        }
        Ok(self.manager.get_cached_state().await.is_moving)
    }

    async fn max_increment(&self) -> ASCOMResult<u32> {
        Ok(self.config.max_step)
    }

    async fn max_step(&self) -> ASCOMResult<u32> {
        Ok(self.config.max_step)
    }

    async fn position(&self) -> ASCOMResult<i32> {
        ensure_connected!(self);
        let state = self.manager.get_cached_state().await;
        let position = state.position.ok_or_else(|| {
            ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "Position not yet available",
            )
        })?;
        // The cache/wire type is i64; ASCOM `Position` is i32. A report outside
        // the i32 range can only be firmware nonsense — surface it rather than
        // silently wrapping.
        i32::try_from(position).map_err(|_| {
            ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                format!("Device-reported position {position} exceeds the ASCOM i32 range"),
            )
        })
    }

    async fn step_size(&self) -> ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_temp_comp(&self, _temp_comp: bool) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp_available(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn temperature(&self) -> ASCOMResult<f64> {
        // The Scops OAG has no temperature probe — see docs/services/pa-scops-oag.md.
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn halt(&self) -> ASCOMResult<()> {
        ensure_connected!(self);
        self.with_session(async |session| self.manager.abort(session).await)
            .await
    }

    async fn move_(&self, position: i32) -> ASCOMResult<()> {
        ensure_connected!(self);

        // Compare in i64: `max_step` is u32 and may exceed `i32::MAX`, where a
        // `as i32` cast would wrap negative and reject every move.
        if position < 0 || i64::from(position) > i64::from(self.config.max_step) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "Position {} out of range [0, {}]",
                    position, self.config.max_step
                ),
            ));
        }

        self.with_session(async |session| {
            self.manager
                .move_absolute(session, i64::from(position))
                .await
        })
        .await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mock::MockScopsTransportFactory;
    use crate::protocol::Command;
    use rusty_photon_shared_transport::TransportFactory;

    #[tokio::test]
    async fn position_errors_instead_of_wrapping_beyond_i32() {
        let factory: Arc<dyn TransportFactory> = Arc::new(MockScopsTransportFactory::default());
        let config = Config::default();
        let manager = FocuserManager::new(&config, factory);
        let device = ScopsFocuserDevice::new(config.focuser, Arc::clone(&manager));
        device.set_connected(true).await.unwrap();

        // Drive the mock past the i32 range out-of-band (`W:` syncs the counter
        // without moving the motor), then force the shared cache to pick it up.
        let session = manager.transport().acquire().await.unwrap();
        let beyond_i32 = i64::from(i32::MAX) + 1;
        session
            .request(Command::SyncPosition {
                position: beyond_i32,
            })
            .await
            .unwrap();
        manager.refresh_status(&session).await.unwrap();

        let err = device.position().await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.to_string().contains("exceeds the ASCOM i32 range"),
            "unexpected error: {err}"
        );
        session.close().await.unwrap();
    }
}

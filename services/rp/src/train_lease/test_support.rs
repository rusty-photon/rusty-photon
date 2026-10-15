//! A fake PHD2 camera facade for the train lease's tests: a Camera
//! that answers the three sweep-lease Actions, records every call, and
//! refuses an acquire or a renewal as its fields say.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMResult};

use super::facade;

/// A camera that answers the facade's sweep-lease Actions, every call
/// recorded; a renewal fails as the flags say.
#[derive(Debug, Default)]
pub struct FakeFacade {
    /// When set, the acquire is refused with this `InvalidOperation`
    /// text, as a facade whose PHD2 is not `Stopped` refuses it.
    pub refuse_acquire: Mutex<Option<String>>,
    /// When set, a renewal is refused: the lease was ended by a stop.
    pub lost: AtomicBool,
    /// When set, a renewal fails as one that did not reach the facade.
    pub unreachable: AtomicBool,
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakeFacade {
    /// The parameters of every call of `action`, in arrival order.
    pub fn calls_of(&self, action: &str) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == action)
            .map(|(_, parameters)| parameters.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl Device for FakeFacade {
    fn static_name(&self) -> &'static str {
        "facade"
    }
    fn unique_id(&self) -> &'static str {
        "facade"
    }
    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn set_connected(&self, _: bool) -> ASCOMResult<()> {
        Ok(())
    }
    async fn description(&self) -> ASCOMResult<String> {
        Ok("facade".into())
    }
    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("facade".into())
    }
    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok("0".into())
    }
    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(vec![
            facade::ACQUIRE.into(),
            facade::RENEW.into(),
            facade::RELEASE.into(),
        ])
    }
    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        let parsed = serde_json::from_str(&parameters).unwrap();
        self.calls.lock().unwrap().push((action.clone(), parsed));
        if action == facade::ACQUIRE {
            if let Some(refusal) = self.refuse_acquire.lock().unwrap().clone() {
                return Err(ASCOMError::invalid_operation(refusal));
            }
        }
        if action == facade::RENEW && self.unreachable.load(Ordering::SeqCst) {
            return Err(ASCOMError::new(
                ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED,
                "connection refused",
            ));
        }
        if action == facade::RENEW && self.lost.load(Ordering::SeqCst) {
            return Err(ASCOMError::invalid_operation(
                "it was ended by a guiding stop",
            ));
        }
        Ok("{}".into())
    }
}

/// The facade's capture surface plays no part in the lease.
#[async_trait::async_trait]
impl Camera for FakeFacade {
    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn has_shutter(&self) -> ASCOMResult<bool> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn max_adu(&self) -> ASCOMResult<u32> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn start_x(&self) -> ASCOMResult<u32> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn start_y(&self) -> ASCOMResult<u32> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn set_start_x(&self, _: u32) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn set_start_y(&self, _: u32) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
    async fn start_exposure(&self, _: Duration, _: bool) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
}

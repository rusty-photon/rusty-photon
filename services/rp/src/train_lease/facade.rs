//! The PHD2 camera facade's sweep lease, as a train lease takes it
//! (rp.md § Train Lease, step 2; phd2-guider.md § The sweep lease).
//!
//! The lease is three ASCOM `Action`s on the facade's Camera, each
//! taking its parameters as a JSON object. rp names the token, so a
//! lease whose acquire got no answer can still be released.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::Camera;
use ascom_alpaca::ASCOMErrorCode;
use tracing::debug;

pub const ACQUIRE: &str = "rustyphoton:sweep-lease-acquire";
pub const RENEW: &str = "rustyphoton:sweep-lease-renew";
pub const RELEASE: &str = "rustyphoton:sweep-lease-release";

/// The TTL rp asks for. Renewing every [`RENEW_INTERVAL`] leaves one
/// renewal free to fail: the next still lands about 20 s before the
/// facade's lease would lapse.
pub const TTL_S: u64 = 60;

/// How often rp renews a facade lease it holds.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// How long a renewal or the closing release may take.
///
/// A renewal that has not answered by then counts as one that did not
/// reach the facade, and is tried again at the next tick.
pub const CALL_BOUND: Duration = Duration::from_secs(10);

/// A fresh token: 32 lowercase hex digits.
#[must_use]
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Whether the camera is the facade, with a `phd2-guider` that has the
/// lease: its `SupportedActions` lists [`ACQUIRE`].
///
/// # Errors
///
/// The `SupportedActions` read's failure, as text.
pub async fn offers_lease(cam: &dyn Camera) -> Result<bool, String> {
    let actions = cam.supported_actions().await.map_err(|e| e.to_string())?;
    Ok(actions.iter().any(|action| action == ACQUIRE))
}

/// Ask the facade for its sweep lease under `token`.
///
/// # Errors
///
/// The facade's refusal, or the call's failure, as text.
pub async fn acquire(cam: &dyn Camera, token: &str) -> Result<(), String> {
    let parameters = serde_json::json!({ "token": token, "ttl_s": TTL_S }).to_string();
    cam.action(ACQUIRE.to_string(), parameters)
        .await
        .map(|answer| debug!(%answer, "the camera facade granted its sweep lease"))
        .map_err(|e| e.to_string())
}

/// What one renewal came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Renewal {
    Renewed,
    /// The facade refused it: the token no longer holds its lease (a
    /// guiding stop, a disconnect, a restart). Carries the facade's
    /// reason.
    Lost(String),
    /// It did not reach the facade, or got no answer in time; the next
    /// tick tries again and the facade's TTL decides.
    Unreached(String),
}

/// Restart the facade lease's TTL.
pub async fn renew(cam: &Arc<dyn Camera>, token: &str) -> Renewal {
    let parameters = serde_json::json!({ "token": token }).to_string();
    match tokio::time::timeout(CALL_BOUND, cam.action(RENEW.to_string(), parameters)).await {
        Ok(Ok(_)) => Renewal::Renewed,
        // The facade answers a token that does not hold its lease with
        // `InvalidOperation`; a transport failure reaches rp as an
        // unspecified error.
        Ok(Err(e)) if e.code == ASCOMErrorCode::INVALID_OPERATION => {
            Renewal::Lost(e.message.to_string())
        }
        Ok(Err(e)) => Renewal::Unreached(e.to_string()),
        Err(_) => Renewal::Unreached(format!("no answer within {CALL_BOUND:?}")),
    }
}

/// End the facade lease. Best effort: the facade never refuses a
/// release, and one that does not reach it leaves the lease to lapse at
/// its TTL.
pub async fn release(cam: &Arc<dyn Camera>, token: &str) {
    let parameters = serde_json::json!({ "token": token }).to_string();
    match tokio::time::timeout(CALL_BOUND, cam.action(RELEASE.to_string(), parameters)).await {
        Ok(Ok(answer)) => debug!(%answer, "released the camera facade's sweep lease"),
        Ok(Err(e)) => {
            debug!(error = %e, "the camera facade's sweep lease could not be released; it lapses at its TTL");
        }
        Err(_) => {
            debug!(bound = ?CALL_BOUND, "the camera facade did not answer the release; its sweep lease lapses at its TTL");
        }
    }
}

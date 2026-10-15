//! In-process stub of `phd2-guider`'s camera facade, as far as rp's
//! train lease reaches it (rp.md § Train Lease).
//!
//! It serves one Camera at device number 0 whose `SupportedActions`
//! lists the three sweep-lease Actions and whose `Action` answers them
//! the way the facade does (phd2-guider.md § The sweep lease): an
//! acquire is granted while PHD2 reads `Stopped` and no lease is held,
//! a renewal succeeds only for the held token, and a release answers
//! `{"released": true}` for the held token and `{"released": false}`
//! for any other. The scenario scripts PHD2's state and can end the
//! held lease the way a `guiding/stop` does. Every Action call is
//! recorded, so a scenario can assert the token rp named, the TTL it
//! asked for, and that the release named the same token.
//!
//! The stub takes no exposures: rp's captures under a lease run against
//! the simulator's camera in an imaging train, and the facade's own
//! capture path has its own suite in `phd2-guider`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{json, Value};

/// The three Actions the facade lists, in the order it lists them.
pub const SWEEP_LEASE_ACTIONS: [&str; 3] = [
    "rustyphoton:sweep-lease-acquire",
    "rustyphoton:sweep-lease-renew",
    "rustyphoton:sweep-lease-release",
];

/// ASCOM `INVALID_OPERATION` (0x40B), the facade's refusal of an
/// acquire or a renewal.
const INVALID_OPERATION: u32 = 0x40B;
/// ASCOM `ACTION_NOT_IMPLEMENTED` (0x40C), its answer to an Action it
/// does not have.
const ACTION_NOT_IMPLEMENTED: u32 = 0x40C;
/// The TTL the facade grants when the acquire names none.
const DEFAULT_TTL_S: u64 = 60;

/// One `Action` call the stub answered.
#[derive(Clone, Debug)]
pub struct FacadeCall {
    pub action: String,
    /// The call's `Parameters`, parsed; `Value::Null` when they were not
    /// JSON.
    pub parameters: Value,
}

#[derive(Debug)]
struct FacadeState {
    connected: AtomicBool,
    phd2_state: Mutex<String>,
    /// The held lease's token and TTL.
    held: Mutex<Option<(String, u64)>>,
    calls: Mutex<Vec<FacadeCall>>,
}

/// A running stub facade. Dropping it stops the server.
pub struct FacadeCameraStub {
    addr: SocketAddr,
    state: Arc<FacadeState>,
    handle: tokio::task::JoinHandle<()>,
}

impl FacadeCameraStub {
    /// Serve the stub on a loopback port the OS assigns, with PHD2
    /// reading `Stopped` and no lease held.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stub facade's listener");
        let addr = listener.local_addr().expect("the stub facade's address");
        let state = Arc::new(FacadeState {
            connected: AtomicBool::new(false),
            phd2_state: Mutex::new("Stopped".to_string()),
            held: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        });
        let app = router(state.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            state,
            handle,
        }
    }

    /// The Alpaca base URL rp's `cameras[].alpaca_url` names.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Script the state PHD2 reports to the next acquire.
    ///
    /// # Panics
    ///
    /// Panics when the state lock is poisoned.
    pub fn set_phd2_state(&self, phd2_state: &str) {
        *self.state.phd2_state.lock().expect("stub state lock") = phd2_state.to_string();
    }

    /// End the held lease the way a `guiding/stop` does: the next
    /// renewal of its token is refused.
    ///
    /// # Panics
    ///
    /// Panics when the state lock is poisoned.
    pub fn end_lease(&self) {
        *self.state.held.lock().expect("stub state lock") = None;
    }

    /// The token of the lease the stub holds, if any.
    ///
    /// # Panics
    ///
    /// Panics when the state lock is poisoned.
    #[must_use]
    pub fn held_token(&self) -> Option<String> {
        self.state
            .held
            .lock()
            .expect("stub state lock")
            .as_ref()
            .map(|(token, _)| token.clone())
    }

    /// Every `Action` call answered so far, in arrival order.
    ///
    /// # Panics
    ///
    /// Panics when the state lock is poisoned.
    #[must_use]
    pub fn calls(&self) -> Vec<FacadeCall> {
        self.state.calls.lock().expect("stub state lock").clone()
    }
}

impl Drop for FacadeCameraStub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn router(state: Arc<FacadeState>) -> Router {
    let get_connected = state.clone();
    let put_connected = state.clone();
    Router::new()
        .route(
            "/management/v1/configureddevices",
            get(|| async {
                Json(json!({
                    "Value": [{
                        "DeviceName": "PHD2 camera facade (stub)",
                        "DeviceType": "Camera",
                        "DeviceNumber": 0,
                        "UniqueID": "bdd-phd2-facade-stub-0"
                    }],
                    "ErrorNumber": 0,
                    "ErrorMessage": ""
                }))
            }),
        )
        .route(
            "/api/v1/camera/0/connected",
            get(move || {
                let state = get_connected.clone();
                async move { value(&json!(state.connected.load(Ordering::SeqCst))) }
            })
            .put(move |form: axum::Form<Vec<(String, String)>>| {
                let state = put_connected.clone();
                async move {
                    let requested =
                        field(&form.0, "Connected").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                    state.connected.store(requested, Ordering::SeqCst);
                    Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
                }
            }),
        )
        .route(
            "/api/v1/camera/0/supportedactions",
            get(|| async { value(&json!(SWEEP_LEASE_ACTIONS)) }),
        )
        .route(
            "/api/v1/camera/0/action",
            put(move |form: axum::Form<Vec<(String, String)>>| {
                let state = state.clone();
                async move { action(&state, &form.0) }
            }),
        )
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
    form.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Answer one sweep-lease Action as the facade does, recording it.
fn action(state: &FacadeState, form: &[(String, String)]) -> Json<Value> {
    let name = field(form, "Action").unwrap_or_default().to_string();
    let parameters: Value =
        serde_json::from_str(field(form, "Parameters").unwrap_or_default()).unwrap_or(Value::Null);
    state
        .calls
        .lock()
        .expect("stub state lock")
        .push(FacadeCall {
            action: name.clone(),
            parameters: parameters.clone(),
        });
    let token = parameters
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let phd2_state = state.phd2_state.lock().expect("stub state lock").clone();
    let mut held = state.held.lock().expect("stub state lock");
    let answer = match name.as_str() {
        "rustyphoton:sweep-lease-acquire" if held.is_some() => Err((
            INVALID_OPERATION,
            "a sweep lease is already held".to_string(),
        )),
        "rustyphoton:sweep-lease-acquire" if phd2_state != "Stopped" => Err((
            INVALID_OPERATION,
            format!("PHD2 is {phd2_state}; the camera facade captures only while PHD2 is Stopped"),
        )),
        "rustyphoton:sweep-lease-acquire" => {
            let ttl_s = parameters
                .get("ttl_s")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TTL_S);
            *held = Some((token.clone(), ttl_s));
            Ok(json!({ "token": token, "ttl_s": ttl_s }))
        }
        "rustyphoton:sweep-lease-renew" => match held.as_ref() {
            Some((held_token, ttl_s)) if *held_token == token => {
                Ok(json!({ "token": token, "ttl_s": ttl_s }))
            }
            _ => Err((
                INVALID_OPERATION,
                "this token holds no sweep lease: it was ended by a guiding stop".to_string(),
            )),
        },
        "rustyphoton:sweep-lease-release" => {
            let released = held.as_ref().is_some_and(|(t, _)| *t == token);
            if released {
                *held = None;
            }
            Ok(json!({ "released": released }))
        }
        _ => Err((
            ACTION_NOT_IMPLEMENTED,
            format!("the camera facade has no Action {name}"),
        )),
    };
    drop(held);
    match answer {
        Ok(answer) => value(&json!(answer.to_string())),
        Err((number, message)) => error(number, &message),
    }
}

fn value(value: &Value) -> Json<Value> {
    Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
}

fn error(number: u32, message: &str) -> Json<Value> {
    Json(json!({ "ErrorNumber": number, "ErrorMessage": message }))
}

//! In-process stub of a power box's Alpaca `Switch` device.
//!
//! rp's power-switching scenarios (rp.md § Power Switching) need a power
//! box whose outputs they can read and whose writes they can observe,
//! without a serial device behind it. The stub serves one `Switch` at
//! device number 0 with a fixed list of named, writable on/off outputs,
//! all on at start, with the wire shape rp's Alpaca client speaks: reads
//! answer through the `{Value, ErrorNumber, ErrorMessage}` envelope, and
//! any switch read or write issued before `Connected = true` answers
//! ASCOM `NOT_CONNECTED` (0x407), as a real driver does.
//!
//! Every `Connected` read is counted. rp's reconnect supervisor reads it
//! once per pass, after every device ahead of the power box in its walk,
//! so a scenario waits on the count to know the supervisor has passed
//! another device, instead of sleeping.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use axum::extract::Query;
use axum::routing::{get, put};
use axum::{Form, Json, Router};
use serde_json::json;

/// ASCOM `NOT_CONNECTED` error number (0x407).
const NOT_CONNECTED_ERROR_NUMBER: u32 = 0x407;
/// ASCOM `INVALID_VALUE` error number (0x401), for a switch id past the
/// last output.
const INVALID_VALUE_ERROR_NUMBER: u32 = 0x401;

#[derive(Debug)]
struct PowerBoxState {
    connected: AtomicBool,
    names: Vec<String>,
    on: RwLock<Vec<bool>>,
    connected_reads: AtomicU32,
}

impl PowerBoxState {
    /// The name of switch `id`; empty past the last output, which
    /// [`switch_id`] refuses before any read.
    fn name(&self, id: usize) -> &str {
        self.names.get(id).map_or("", String::as_str)
    }

    /// Whether switch `id` is on; off past the last output.
    fn is_on(&self, id: usize) -> bool {
        self.on
            .read()
            .expect("power box lock poisoned")
            .get(id)
            .copied()
            .unwrap_or(false)
    }

    fn set(&self, id: usize, on: bool) {
        if let Some(slot) = self
            .on
            .write()
            .expect("power box lock poisoned")
            .get_mut(id)
        {
            *slot = on;
        }
    }
}

/// A running stub power box. Dropping it shuts the server down
/// best-effort.
#[derive(Debug)]
pub struct PowerBoxStub {
    port: u16,
    state: Arc<PowerBoxState>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PowerBoxStub {
    /// Serve a power box whose outputs carry `names`, in switch-id
    /// order, every one of them on.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    #[must_use]
    pub fn start(names: &[&str]) -> Self {
        let state = Arc::new(PowerBoxState {
            connected: AtomicBool::new(false),
            names: names.iter().map(ToString::to_string).collect(),
            on: RwLock::new(vec![true; names.len()]),
            connected_reads: AtomicU32::new(0),
        });
        let listener = bind_loopback().expect("failed to bind the power box stub");
        let port = listener
            .local_addr()
            .expect("power box stub has no local addr")
            .port();
        let app = router(&state);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("power box stub failed");
        });
        Self {
            port,
            state,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// The base URL rp's `alpaca_url` config should point at.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Whether the output named `name` is on.
    ///
    /// # Panics
    ///
    /// Panics if the box has no output of that name.
    #[must_use]
    pub fn is_on(&self, name: &str) -> bool {
        let index = self
            .state
            .names
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("the stub power box has no output named {name:?}"));
        self.state.is_on(index)
    }

    /// How many `Connected` reads the box has served.
    #[must_use]
    pub fn connected_reads(&self) -> u32 {
        self.state.connected_reads.load(Ordering::SeqCst)
    }
}

impl Drop for PowerBoxStub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn bind_loopback() -> std::io::Result<tokio::net::TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    socket.listen(64)
}

/// A request parameter by name, matched without regard to case as
/// Alpaca's parameter names are.
fn param<'a>(params: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The switch id a request names, or the Alpaca error to answer it with.
fn switch_id(
    state: &PowerBoxState,
    params: &HashMap<String, String>,
) -> Result<usize, Json<serde_json::Value>> {
    if !state.connected.load(Ordering::SeqCst) {
        return Err(error_response(
            NOT_CONNECTED_ERROR_NUMBER,
            "NOT_CONNECTED: the device is not connected",
        ));
    }
    param(params, "Id")
        .and_then(|id| id.parse::<usize>().ok())
        .filter(|id| *id < state.names.len())
        .ok_or_else(|| error_response(INVALID_VALUE_ERROR_NUMBER, "INVALID_VALUE: no such switch"))
}

/// One `GET` route that reads switch `Id` through `read`.
fn switch_read(
    state: &Arc<PowerBoxState>,
    read: fn(&PowerBoxState, usize) -> serde_json::Value,
) -> axum::routing::MethodRouter {
    let state = state.clone();
    get(move |Query(params): Query<HashMap<String, String>>| {
        let state = state.clone();
        async move {
            match switch_id(&state, &params) {
                Ok(id) => value_response(&read(&state, id)),
                Err(e) => e,
            }
        }
    })
}

fn router(state: &Arc<PowerBoxState>) -> Router {
    let devices_body = json!({
        "Value": [{
            "DeviceName": "Stub Power Box",
            "DeviceType": "Switch",
            "DeviceNumber": 0,
            "UniqueID": "bdd-power-box-stub-0"
        }],
        "ErrorNumber": 0,
        "ErrorMessage": ""
    });
    let get_connected = state.clone();
    let put_connected = state.clone();
    let max_switch = state.clone();
    let set_switch = state.clone();
    Router::new()
        .route(
            "/management/v1/configureddevices",
            get(move || {
                let body = devices_body.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/api/v1/switch/0/connected",
            get(move || {
                let state = get_connected.clone();
                async move {
                    state.connected_reads.fetch_add(1, Ordering::SeqCst);
                    value_response(&json!(state.connected.load(Ordering::SeqCst)))
                }
            })
            .put(move |Form(form): Form<HashMap<String, String>>| {
                let state = put_connected.clone();
                async move {
                    let requested =
                        param(&form, "Connected").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                    state.connected.store(requested, Ordering::SeqCst);
                    ok_response()
                }
            }),
        )
        .route(
            "/api/v1/switch/0/maxswitch",
            get(move || {
                let state = max_switch.clone();
                async move {
                    if state.connected.load(Ordering::SeqCst) {
                        value_response(&json!(state.names.len()))
                    } else {
                        error_response(
                            NOT_CONNECTED_ERROR_NUMBER,
                            "NOT_CONNECTED: the device is not connected",
                        )
                    }
                }
            }),
        )
        .route(
            "/api/v1/switch/0/getswitchname",
            switch_read(state, |s, id| json!(s.name(id))),
        )
        .route(
            "/api/v1/switch/0/canwrite",
            switch_read(state, |_, _| json!(true)),
        )
        .route(
            "/api/v1/switch/0/getswitch",
            switch_read(state, |s, id| json!(s.is_on(id))),
        )
        .route(
            "/api/v1/switch/0/getswitchvalue",
            switch_read(state, |s, id| json!(if s.is_on(id) { 1.0 } else { 0.0 })),
        )
        .route(
            "/api/v1/switch/0/setswitch",
            put(move |Form(form): Form<HashMap<String, String>>| {
                let state = set_switch.clone();
                async move {
                    match switch_id(&state, &form) {
                        Ok(id) => {
                            let on = param(&form, "State")
                                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                            state.set(id, on);
                            ok_response()
                        }
                        Err(e) => e,
                    }
                }
            }),
        )
}

fn ok_response() -> Json<serde_json::Value> {
    Json(json!({ "ErrorNumber": 0, "ErrorMessage": "" }))
}

fn value_response(value: &serde_json::Value) -> Json<serde_json::Value> {
    Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
}

fn error_response(number: u32, message: &str) -> Json<serde_json::Value> {
    Json(json!({ "ErrorNumber": number, "ErrorMessage": message }))
}

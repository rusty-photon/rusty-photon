//! Steps for the powered-off device scenarios (docs/services/doctor.md
//! §Powered-off devices): a stub power box serving the Alpaca switch
//! members doctor reads, and a count of what it was asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use cucumber::{given, then};
use serde_json::{json, Value};

use crate::world::DoctorWorld;

/// What the stub power box serves at `/api/v1/switch/0`: its outputs by
/// switch id, and every request it was sent.
#[derive(Debug, Default)]
pub struct PowerBoxStub {
    outputs: Mutex<Vec<(String, bool)>>,
    requests: AtomicUsize,
    /// Answer every request with HTTP 500.
    failing: bool,
}

impl PowerBoxStub {
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

type Reply = Result<Json<Value>, StatusCode>;

fn value(value: &Value) -> Json<Value> {
    Json(json!({ "Value": value, "ErrorNumber": 0, "ErrorMessage": "" }))
}

fn answer(stub: &PowerBoxStub, member: &str, query: &HashMap<String, String>) -> Reply {
    stub.requests.fetch_add(1, Ordering::SeqCst);
    if stub.failing {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let outputs = stub.outputs.lock().expect("stub outputs");
    let output = || {
        query
            .get("Id")
            .and_then(|id| id.parse::<usize>().ok())
            .and_then(|id| outputs.get(id))
            .ok_or(StatusCode::BAD_REQUEST)
    };
    match member {
        "maxswitch" => Ok(value(&json!(outputs.len()))),
        "getswitchname" => Ok(value(&json!(output()?.0))),
        "getswitch" => Ok(value(&json!(output()?.1))),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

/// Start the stub on both loopback families, since doctor dials it by the
/// `localhost` the staged config names (`crate::loopback`).
async fn start(world: &mut DoctorWorld, failing: bool) -> Arc<PowerBoxStub> {
    let stub = Arc::new(PowerBoxStub {
        failing,
        ..PowerBoxStub::default()
    });
    let (port, listeners) = crate::loopback::bind_loopback_pair().await;
    for listener in listeners {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        world.stub_shutdowns.push(shutdown_tx);
        let router = axum::Router::new()
            .route(
                "/api/v1/switch/0/{member}",
                axum::routing::get(
                    |State(stub): State<Arc<PowerBoxStub>>,
                     axum::extract::Path(member): axum::extract::Path<String>,
                     Query(query): Query<HashMap<String, String>>| async move {
                        answer(&stub, &member, &query)
                    },
                ),
            )
            .with_state(Arc::clone(&stub));
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    shutdown_rx.await.ok();
                })
                .await
                .expect("stub power box serve");
        });
    }
    world.power_box = Some((port, Arc::clone(&stub)));
    stub
}

#[given(expr = "a stub power box whose output {string} reads {word}")]
async fn power_box_output(world: &mut DoctorWorld, name: String, state: String) {
    let on = match state.as_str() {
        "on" => true,
        "off" => false,
        other => panic!("an output reads on or off, not {other}"),
    };
    let stub = match &world.power_box {
        Some((_, stub)) => Arc::clone(stub),
        None => start(world, false).await,
    };
    stub.outputs.lock().expect("stub outputs").push((name, on));
}

#[given("a stub power box that answers HTTP 500")]
async fn failing_power_box(world: &mut DoctorWorld) {
    start(world, true).await;
}

#[then("the stub power box was asked nothing")]
fn power_box_asked_nothing(world: &mut DoctorWorld) {
    let (_, stub) = world.power_box.as_ref().expect("no stub power box started");
    assert_eq!(stub.requests(), 0, "doctor read the power box");
}

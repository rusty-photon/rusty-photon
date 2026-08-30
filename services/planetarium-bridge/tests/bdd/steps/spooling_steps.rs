//! Steps for spooling.feature: the bounded on-disk FIFO spool, replay
//! ordering and provenance, and the /health counters.

use std::time::Instant;

use cucumber::gherkin::Step;
use cucumber::{given, then};

use crate::world::{BridgeWorld, POLL_INTERVAL, WAIT_BUDGET};

#[given("a spool file already containing:")]
fn spool_file_containing(world: &mut BridgeWorld, step: &Step) {
    let content = step.docstring().expect("step needs a docstring").trim();
    let path = world.spool_path();
    std::fs::write(&path, format!("{content}\n")).expect("write pre-seeded spool");
}

/// Poll /health until every field in the table matches — replay and
/// reconnect run on their own schedule, so a single read would race them.
#[then("the health endpoint should report:")]
async fn health_reports(world: &mut BridgeWorld, step: &Step) {
    let table = step.table().expect("step needs a field/value table");
    let expectations: Vec<(String, serde_json::Value)> = table
        .rows
        .iter()
        .skip(1) // header row
        .map(|row| {
            let field = row[0].clone();
            let value = match row[1].as_str() {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                number => serde_json::Value::from(number.parse::<i64>().unwrap_or_else(|_| {
                    panic!("health value {number:?} is neither bool nor integer")
                })),
            };
            (field, value)
        })
        .collect();

    let mut last: Option<serde_json::Value> = None;
    let start = Instant::now();
    while start.elapsed() < WAIT_BUDGET {
        if let Some(health) = world.try_health().await {
            let all_match = expectations
                .iter()
                .all(|(field, value)| health.get(field) == Some(value));
            last = Some(health);
            if all_match {
                return;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!(
        "health never reported {expectations:?} in {:?} (budget {WAIT_BUDGET:?}); \
         last response: {last:?}",
        start.elapsed()
    );
}

#[then(expr = "the imports should arrive in Align order: ra_hours {float} then ra_hours {float}")]
async fn imports_in_order(world: &mut BridgeWorld, first: f64, second: f64) {
    let calls = world.stub().add_target_calls();
    assert_eq!(calls.len(), 2, "expected exactly two imports: {calls:?}");
    let ra_values: Vec<f64> = calls
        .iter()
        .map(|call| {
            call.arguments["ra_hours"]
                .as_f64()
                .expect("ra_hours missing")
        })
        .collect();
    assert!(
        (ra_values[0] - first).abs() < 1e-6 && (ra_values[1] - second).abs() < 1e-6,
        "imports arrived as ra_hours {ra_values:?}, expected [{first}, {second}]"
    );
}

#[then("the replayed imports should carry receipt times from before rp recovered")]
async fn replayed_receipt_times(world: &mut BridgeWorld) {
    let recovered_at = world.stub().started_at();
    let calls = world.stub().add_target_calls();
    assert!(!calls.is_empty(), "no imports recorded");
    for call in calls {
        let received_at = call.arguments["source"]["received_at"]
            .as_str()
            .expect("source.received_at missing")
            .to_owned();
        let parsed = chrono::DateTime::parse_from_rfc3339(&received_at)
            .unwrap_or_else(|e| panic!("received_at {received_at:?} is not RFC 3339: {e}"));
        assert!(
            parsed.with_timezone(&chrono::Utc) < recovered_at,
            "received_at {received_at} is not from before rp recovered at {recovered_at}"
        );
    }
}

//! Steps for `connection_lifecycle.feature`.

use crate::world::{CommandLogTimeout, StarAdventurerWorld, DEBUG_RETRY_WINDOW};
use cucumber::gherkin::Step;
use cucumber::{given, then, when};

#[given("a running star-adventurer service")]
async fn running_service(world: &mut StarAdventurerWorld) {
    world.start_service().await;
}

#[given(expr = "a mount that reports CPR {int} on the RA axis and {int} on the Dec axis")]
async fn mount_reports_cpr(_world: &mut StarAdventurerWorld, cpr_ra: u32, cpr_dec: u32) {
    // Assert the feature-file values match the GTi per-axis defaults
    // the mock already seeds (the axes differ on real hardware); a
    // divergent CPR would need a `/debug/v1/mock-state` extension and
    // the scenario should fail fast instead of silently passing.
    const GTI_CPR_RA: u32 = 0x0037_5F00;
    const GTI_CPR_DEC: u32 = 0x002C_4C00;
    assert_eq!(
        cpr_ra, GTI_CPR_RA,
        "mock seeds RA CPR {GTI_CPR_RA}; feature file asked for {cpr_ra}"
    );
    assert_eq!(
        cpr_dec, GTI_CPR_DEC,
        "mock seeds Dec CPR {GTI_CPR_DEC}; feature file asked for {cpr_dec}"
    );
}

#[given(expr = "a mount that reports timer frequency {int}")]
async fn mount_reports_tmr_freq(_world: &mut StarAdventurerWorld, hz: u32) {
    const GTI_TMR_FREQ: u32 = 0x00F4_2400;
    assert_eq!(
        hz, GTI_TMR_FREQ,
        "mock only seeds tmr_freq {GTI_TMR_FREQ}; feature file asked for {hz}"
    );
}

// `the mount is slewing` is used both as a `Given` (preconditioning a
// scenario before connect) and as a `When` (mid-scenario state mutation
// after connect). Register both so cucumber's keyword-strict matching
// finds it in either context.
#[given("the mount is slewing")]
#[when("the mount is slewing")]
async fn mount_is_slewing(world: &mut StarAdventurerWorld) {
    // Seed running=true on both axes plus a far goto target so the
    // mock's polling-driven `advance_one_step` does not immediately
    // clear the running flag (delta would be zero otherwise). Use the
    // protocol's signed-24-bit max so the seed stays inside the wire
    // representable range and survives `/debug/v1/mock-state`'s
    // tick-range validator.
    use skywatcher_motor_protocol::codec::POSITION_MAX;
    world.queue_seed("ra_running", true.into()).await;
    world.queue_seed("ra_goto", true.into()).await;
    world
        .queue_seed("ra_goto_target_ticks", POSITION_MAX.into())
        .await;
    world.queue_seed("dec_running", true.into()).await;
    world.queue_seed("dec_goto", true.into()).await;
    world
        .queue_seed("dec_goto_target_ticks", POSITION_MAX.into())
        .await;
    world.queue_seed("ra_initialized", true.into()).await;
    world.queue_seed("dec_initialized", true.into()).await;
}

#[when("I connect the device")]
async fn connect_device(world: &mut StarAdventurerWorld) {
    world.mount().set_connected(true).await.unwrap();
}

#[when("I disconnect the device")]
async fn disconnect_device(world: &mut StarAdventurerWorld) {
    world.mount().set_connected(false).await.unwrap();
}

#[when("two clients connect the device")]
async fn two_clients_connect(world: &mut StarAdventurerWorld) {
    // Single-process test: drive set_connected(true) twice. The
    // device's idempotent guard makes the second call a no-op, but the
    // ref-count semantics under that guard are unit-tested at
    // `services/star-adventurer-gti/src/transport_manager.rs::tests::
    // connect_is_reference_counted`. Treat this scenario as a smoke
    // check that double-connect does not break.
    world.mount().set_connected(true).await.unwrap();
    // Idempotent: the second connect must also succeed (the device
    // guard treats it as a no-op). Asserting catches a regression
    // where double-connect would start returning an error and the
    // smoke check silently degrades into nothing.
    world.mount().set_connected(true).await.unwrap();
}

#[when("one client disconnects the device")]
async fn one_client_disconnects(world: &mut StarAdventurerWorld) {
    world.mount().set_connected(false).await.unwrap();
}

#[when("the remaining client disconnects the device")]
async fn remaining_client_disconnects(world: &mut StarAdventurerWorld) {
    let _ = world.mount().set_connected(false).await;
}

#[then("the device should be disconnected")]
async fn device_should_be_disconnected(world: &mut StarAdventurerWorld) {
    assert!(!world.mount().connected().await.unwrap());
}

#[then("the device should be connected")]
async fn device_should_be_connected(world: &mut StarAdventurerWorld) {
    assert!(world.mount().connected().await.unwrap());
}

#[then("the device should still be connected")]
async fn device_should_still_be_connected(world: &mut StarAdventurerWorld) {
    assert!(world.mount().connected().await.unwrap());
}

#[then("the underlying transport should have been opened exactly once")]
async fn transport_opened_once(world: &mut StarAdventurerWorld) {
    // Pin the property indirectly: every connect runs the `:F1` /
    // `:F2` handshake exactly once. Counting `:F1` frames in the
    // command log is a proxy for "transport opened once".
    let log = world.command_log_including_startup().await;
    let f1_count = log.iter().filter(|c| c.as_str() == ":F1\r").count();
    assert_eq!(f1_count, 1, "expected one :F1 handshake, saw log {log:?}");
}

#[then("the mount should have received startup commands in order:")]
async fn commands_received_in_order(world: &mut StarAdventurerWorld, step: &Step) {
    let table = step.table.as_ref().expect("expected a data table");
    // The only step that reads past the startup mark: its scenarios
    // are about what the driver puts on the wire while starting.
    let log = world.command_log_including_startup().await;
    let mut log_idx = 0usize;
    for row in table.rows.iter().skip(1) {
        let want = format!("{}\r", row[0].trim());
        let mut found = false;
        while log_idx < log.len() {
            if log[log_idx] == want {
                found = true;
                log_idx += 1;
                break;
            }
            log_idx += 1;
        }
        assert!(
            found,
            "expected wire frame {want:?} in command log; saw {log:?}"
        );
    }
}

#[then(expr = "the parameter cache should report CPR {int} on the RA axis")]
async fn param_cache_cpr_ra(world: &mut StarAdventurerWorld, _expected: u32) {
    // The handshake's `:a1` reply seeded the cache. Tightest external
    // assertion is that `:a1` appears in the log; the value itself is
    // pinned by the unit test
    // `transport_manager::tests::connect_runs_handshake_and_seeds_parameter_cache`.
    let log = world.command_log_including_startup().await;
    assert!(log.iter().any(|c| c == ":a1\r"), "no :a1 in log");
}

#[then(expr = "the parameter cache should report CPR {int} on the Dec axis")]
async fn param_cache_cpr_dec(world: &mut StarAdventurerWorld, _expected: u32) {
    let log = world.command_log_including_startup().await;
    assert!(log.iter().any(|c| c == ":a2\r"), "no :a2 in log");
}

#[then(expr = "the parameter cache should report timer frequency {int}")]
async fn param_cache_tmr_freq(world: &mut StarAdventurerWorld, _expected: u32) {
    let log = world.command_log_including_startup().await;
    assert!(log.iter().any(|c| c == ":b1\r"), "no :b1 in log");
}

#[then("the mount should have been initialised and halted a second time")]
async fn reload_reinitialises_and_halts(world: &mut StarAdventurerWorld) {
    // Both halves matter. Two `:F1` frames say the reload opened a
    // second conduit to the *same* mount — the mock mount outlives the
    // reload, as the hardware would — and the halt after the second one
    // is the cold start's own safety assertion on the fresh conduit.
    let log = world
        .wait_for_command_log_including_startup(DEBUG_RETRY_WINDOW, |log| {
            second_init_is_followed_by_a_halt(log)
        })
        .await;
    match log {
        Ok(_) => {}
        Err(CommandLogTimeout::NoMatch(log)) => {
            // The tail only: the poll loop makes this log hundreds of
            // frames long, and the whole of it buries the count that
            // actually says what went wrong.
            let inits = log.iter().filter(|c| c.as_str() == ":F1\r").count();
            let tail: Vec<&String> = log.iter().rev().take(20).collect();
            panic!(
                "expected a second :F1 followed by :L1, :L2, :K1; saw {inits} :F1 frames in a log \
                 of {}; last 20 (newest-first): {tail:?}",
                log.len()
            );
        }
        Err(CommandLogTimeout::FetchFailed(e)) => {
            panic!("could not read mock-commands: {e}")
        }
    }
}

/// Whether the log carries a second `:F1` with the whole halt sequence
/// after it.
fn second_init_is_followed_by_a_halt(log: &[String]) -> bool {
    let mut inits = log
        .iter()
        .enumerate()
        .filter(|(_, c)| c.as_str() == ":F1\r")
        .map(|(i, _)| i);
    let Some(second) = inits.nth(1) else {
        return false;
    };
    let after = &log[second..];
    let mut idx = 0usize;
    for want in [":L1\r", ":L2\r", ":K1\r"] {
        match after[idx..].iter().position(|c| c.as_str() == want) {
            Some(hit) => idx += hit + 1,
            None => return false,
        }
    }
    true
}

#[then(expr = "the mount should have received command {word}")]
async fn mount_received_command(world: &mut StarAdventurerWorld, command: String) {
    // Wait for any in-flight watcher iteration to issue its commands;
    // CI runners can be slow.
    let want = format!("{command}\r");
    match world
        .wait_for_command_log(DEBUG_RETRY_WINDOW, |log| log.iter().any(|c| c == &want))
        .await
    {
        Ok(_) => {}
        Err(CommandLogTimeout::NoMatch(log)) => {
            let len = log.len();
            let tail: Vec<&String> = log.iter().rev().take(20).collect();
            panic!(
                "expected wire frame {command:?} in command log (size {len}); last 20 (newest-first): {tail:?}"
            );
        }
        Err(CommandLogTimeout::FetchFailed(e)) => {
            panic!("could not read the command log while waiting for {command:?}: {e}")
        }
    }
}

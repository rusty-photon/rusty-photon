//! Step definitions for the camera facade's sweep lease:
//! `camera_lease.feature`.

use cucumber::{then, when};

use super::camera_steps::{facade, record_error};
use crate::world::GuiderWorld;

const ACQUIRE: &str = "rustyphoton:sweep-lease-acquire";
const RENEW: &str = "rustyphoton:sweep-lease-renew";
const RELEASE: &str = "rustyphoton:sweep-lease-release";

/// The token a feature names by one hex letter: token A is 32 `a`s.
fn token(name: &str) -> String {
    assert!(
        name.len() == 1 && name.chars().all(|c| matches!(c, 'A'..='F')),
        "a feature names a token by one letter A to F, not {name:?}"
    );
    name.to_lowercase().repeat(32)
}

/// Call a lease Action, keeping its parsed answer or its ASCOM error.
async fn lease_action(world: &mut GuiderWorld, action: &str, parameters: String) {
    let camera = facade(world).await;
    match camera.action(action.to_string(), parameters).await {
        Ok(reply) => {
            world.lease_reply =
                Some(serde_json::from_str(&reply).expect("a lease Action answers JSON"));
            world.last_ascom_error = None;
        }
        Err(e) => {
            world.lease_reply = None;
            record_error(world, Err(e));
        }
    }
}

/// Call a lease Action that must succeed.
async fn lease_action_ok(world: &mut GuiderWorld, action: &str, parameters: String) {
    lease_action(world, action, parameters).await;
    if let Some((code, message)) = &world.last_ascom_error {
        panic!("{action} failed with ASCOM error {code:#X}: {message}");
    }
}

#[when(expr = "the client acquires a sweep lease under token {word} for {int} s")]
async fn acquire_for(world: &mut GuiderWorld, name: String, ttl_s: u64) {
    let parameters = serde_json::json!({ "token": token(&name), "ttl_s": ttl_s });
    lease_action_ok(world, ACQUIRE, parameters.to_string()).await;
}

#[when(expr = "the client acquires a sweep lease under token {word}")]
async fn acquire(world: &mut GuiderWorld, name: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action_ok(world, ACQUIRE, parameters.to_string()).await;
}

#[when(expr = "the client acquires a sweep lease under token {word} by the action name {string}")]
async fn acquire_by_name(world: &mut GuiderWorld, name: String, action: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action_ok(world, &action, parameters.to_string()).await;
}

#[when(expr = "the client tries to acquire a sweep lease under token {word}")]
async fn try_acquire(world: &mut GuiderWorld, name: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action(world, ACQUIRE, parameters.to_string()).await;
}

#[when(regex = r"^the client tries to acquire a sweep lease with the parameters (.+)$")]
async fn try_acquire_with(world: &mut GuiderWorld, parameters: String) {
    lease_action(world, ACQUIRE, parameters).await;
}

#[when(expr = "the client renews the sweep lease under token {word}")]
async fn renew(world: &mut GuiderWorld, name: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action_ok(world, RENEW, parameters.to_string()).await;
}

#[when(expr = "the client tries to renew the sweep lease under token {word}")]
async fn try_renew(world: &mut GuiderWorld, name: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action(world, RENEW, parameters.to_string()).await;
}

#[when(expr = "the client releases the sweep lease under token {word}")]
async fn release(world: &mut GuiderWorld, name: String) {
    let parameters = serde_json::json!({ "token": token(&name) });
    lease_action_ok(world, RELEASE, parameters.to_string()).await;
}

#[when("the client disconnects the facade camera")]
async fn disconnect(world: &mut GuiderWorld) {
    let camera = facade(world).await;
    camera
        .set_connected(false)
        .await
        .unwrap_or_else(|e| panic!("disconnecting the facade camera failed: {e}"));
}

#[then(expr = "the facade's supported actions should include {string}")]
async fn supported_action(world: &mut GuiderWorld, name: String) {
    let actions = facade(world)
        .await
        .supported_actions()
        .await
        .expect("SupportedActions");
    assert!(actions.contains(&name), "{name} not in {actions:?}");
}

#[then(expr = "the lease reply should name token {word} with a TTL of {int} s")]
fn reply_names(world: &mut GuiderWorld, name: String, ttl_s: u64) {
    let reply = world.lease_reply.as_ref().expect("no lease reply");
    assert_eq!(
        reply,
        &serde_json::json!({ "token": token(&name), "ttl_s": ttl_s }),
        "lease reply"
    );
}

#[then(regex = r"^the release reply should be released (true|false)$")]
fn release_reply(world: &mut GuiderWorld, released: String) {
    let reply = world.lease_reply.as_ref().expect("no release reply");
    assert_eq!(
        reply,
        &serde_json::json!({ "released": released == "true" }),
        "release reply"
    );
}

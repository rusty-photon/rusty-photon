//! Steps for `stream_page.feature` — the activity-stream page markup and the
//! `/stream/events` SSE proxy driven over plain HTTP (SSE is server bytes; no
//! browser needed — the browser-side swap behaviour is proven by the
//! `@browser` `sse.feature` spike).

use cucumber::{given, then, when};

use crate::dom;
use crate::world::UiWorld;

#[when("I open the stream page")]
async fn open_stream(world: &mut UiWorld) {
    world.get("/stream").await;
}

#[then(regex = r#"^the page declares an SSE connection to "([^"]+)"$"#)]
fn declares_sse(world: &mut UiWorld, url: String) {
    let connect = dom::attr(&world.last_body, "[hx-ext=\"sse\"]", "sse-connect");
    assert_eq!(
        connect.as_deref(),
        Some(url.as_str()),
        "{}",
        world.last_body
    );
}

#[then(regex = r#"^the feed region prepends "([\w]+)" events$"#)]
fn feed_prepends(world: &mut UiWorld, event: String) {
    assert_eq!(
        dom::attr(&world.last_body, "#feed", "sse-swap").as_deref(),
        Some(event.as_str()),
        "{}",
        world.last_body
    );
    assert_eq!(
        dom::attr(&world.last_body, "#feed", "hx-swap").as_deref(),
        Some("afterbegin"),
        "{}",
        world.last_body
    );
}

#[then(regex = r#"^the fold panel polls "([^"]+)" every 10 seconds$"#)]
fn panel_polls(world: &mut UiWorld, url: String) {
    assert_eq!(
        dom::attr(&world.last_body, "#equipment-leds", "hx-get").as_deref(),
        Some(url.as_str()),
        "{}",
        world.last_body
    );
    let trigger = dom::attr(&world.last_body, "#equipment-leds", "hx-trigger").unwrap_or_default();
    assert!(trigger.contains("every 10s"), "hx-trigger: {trigger}");
}

#[given("a connected reader on the BFF event stream")]
#[when("I connect a reader to the BFF event stream")]
async fn connect_reader(world: &mut UiWorld) {
    world.connect_stream_events(None).await;
}

#[given("a running rp with a stub safety monitor and an empty roster")]
async fn rp_with_safety_monitor(world: &mut UiWorld) {
    world.start_rp_with_safety_monitor().await;
}

#[when("the safety monitor reports unsafe on rp")]
async fn safety_unsafe(world: &mut UiWorld) {
    world.set_safety(false).await;
}

#[when("the safety monitor reports safe again on rp")]
async fn safety_safe(world: &mut UiWorld) {
    world.set_safety(true).await;
}

/// Pre-seeds history (an unsafe and a safe transition) before any reader.
#[given("the safety monitor went unsafe and back to safe on rp")]
async fn safety_flipped_and_back(world: &mut UiWorld) {
    world.set_safety(false).await;
    world.set_safety(true).await;
}

#[given(regex = r#"^a "([\w]+)" frame arrives whose card mentions "([^"]+)"$"#)]
#[then(regex = r#"^a "([\w]+)" frame arrives whose card mentions "([^"]+)"$"#)]
async fn frame_arrives(world: &mut UiWorld, event: String, needle: String) {
    world
        .sse
        .as_ref()
        .expect("no SSE reader connected")
        .wait_for(&event, &needle)
        .await;
}

#[then(regex = r#"^an "([\w]+)" frame arrives mentioning "([^"]+)"$"#)]
async fn slot_frame_arrives(world: &mut UiWorld, event: String, needle: String) {
    world
        .sse
        .as_ref()
        .expect("no SSE reader connected")
        .wait_for(&event, &needle)
        .await;
}

#[then(regex = r#"^every received "([\w]+)" frame carries a numeric SSE id$"#)]
async fn frames_carry_numeric_ids(world: &mut UiWorld, event: String) {
    let frames = world.sse.as_ref().expect("no SSE reader").frames().await;
    let feed: Vec<_> = frames
        .iter()
        .filter(|f| f.event.as_deref() == Some(event.as_str()))
        .collect();
    assert!(!feed.is_empty(), "no {event} frames received");
    for frame in feed {
        let id = frame
            .id
            .as_deref()
            .unwrap_or_else(|| panic!("{event} frame without an id: {:?}", frame.data));
        id.parse::<u64>()
            .unwrap_or_else(|_| panic!("non-numeric SSE id {id:?}"));
    }
}

#[when("I remember the highest received SSE id and reconnect with it as the cursor")]
async fn reconnect_with_cursor(world: &mut UiWorld) {
    let frames = world.sse.as_ref().expect("no SSE reader").frames().await;
    let max = frames
        .iter()
        .filter_map(|f| f.id.as_deref().and_then(|id| id.parse::<u64>().ok()))
        .max()
        .expect("no frame carried an id to use as the cursor");
    world.sse_cursor = Some(max);
    // Drop the old reader before reconnecting (one live stream at a time).
    world.sse = None;
    world.connect_stream_events(Some(max)).await;
}

#[then(regex = r#"^every received "([\w]+)" frame carries an SSE id greater than the cursor$"#)]
async fn frames_after_cursor(world: &mut UiWorld, event: String) {
    let cursor = world.sse_cursor.expect("no cursor was remembered");
    let frames = world.sse.as_ref().expect("no SSE reader").frames().await;
    let feed: Vec<_> = frames
        .iter()
        .filter(|f| f.event.as_deref() == Some(event.as_str()))
        .collect();
    assert!(
        !feed.is_empty(),
        "no {event} frames received after reconnect"
    );
    for frame in feed {
        let id: u64 = frame
            .id
            .as_deref()
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(|| panic!("{event} frame without a numeric id"));
        assert!(
            id > cursor,
            "frame id {id} is not after the cursor {cursor} — the replay \
             re-delivered an already-seen envelope"
        );
    }
}

#[then("the event stream ends")]
async fn stream_ends(world: &mut UiWorld) {
    world
        .sse
        .as_ref()
        .expect("no SSE reader connected")
        .wait_for_end()
        .await;
}

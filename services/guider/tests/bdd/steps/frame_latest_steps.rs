//! Steps of `frame_latest.feature`: `GET /api/v1/frame/latest`.

use cucumber::{then, when};

use crate::steps::loop_steps::latest;
use crate::world::GuiderWorld;

#[when("a client asks for the latest frame")]
async fn ask_for_latest(world: &mut GuiderWorld) {
    let answer = world
        .request(reqwest::Method::GET, "/api/v1/frame/latest", None)
        .await
        .unwrap_or_else(|e| panic!("GET {e}"));
    world.answer = Some(answer);
}

#[then(regex = r"^the request answers 200 with a PNG of (\d+) × (\d+) pixels$")]
fn answers_png(world: &mut GuiderWorld, width: u32, height: u32) {
    let answer = latest(world);
    assert_eq!(
        (answer.status, answer.content_type.as_deref()),
        (200, Some("image/png")),
        "the answer's status and content type"
    );
    let decoder = png::Decoder::new(std::io::Cursor::new(&answer.bytes));
    let reader = decoder.read_info().expect("the answer is a PNG");
    let info = reader.info();
    assert_eq!((info.width, info.height), (width, height), "the PNG's size");
}

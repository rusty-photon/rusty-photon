// Manual hardware-probe binary (requires a physical camera with a CFW) —
// excluded from coverage.
//
// How a QHY CFW driven through its camera's handle answers moves, status reads
// and the camera's own traffic. It reads only what the CFW reports — the slot
// `CONTROL_CFWPORT` names — so it says where the wheel *reports* it is, not
// where it physically is. Every step is printed as one JSON line, `t` in ms
// since start, interleaved with the SDK's own log. Each mode opens the camera
// and runs its handshake's init first (stream mode, readout mode 0,
// `InitQHYCCD`), as a connect does. Run one mode per process: the first move
// after the SDK starts reads differently from every later one.
//
//   cfw_probe home <slot>        settle at <slot>, then two re-inits and a close,
//                                re-open and init, reading the status throughout
//   cfw_probe rest <ms> [read]   a move sent <ms> after the read that saw the
//                                last move arrive ([read]: after a fresh idle read)
//   cfw_probe during <call> <ms> <call> run <ms> into a rested wheel's travel:
//                                none, init, readmode, stream, sequence, temps, exposure
//   cfw_probe overlap <ms>       a move sent <ms> after the init sequence starts
//   cfw_probe lost <resend|next> drop a move, then send the same slot again or
//                                go on to another
//   cfw_probe reads <ms>...      status reads running across an init started <ms> in
//   cfw_probe settle <slot>      bring the wheel to <slot> and leave it there
//   cfw_probe zero               the process's first move goes to slot 0
//   cfw_probe same               the process's first command names the slot the
//                                wheel stands on; then a move to slot 0
//   cfw_probe forget <how>       a move after the last command and a re-init
//                                (reinit), a close, re-open and init (reopen), or
//                                neither (none)
//   cfw_probe travel             a rested wheel's move from every slot to every
//                                other: how long it travels
//
// It opens the last camera the SDK lists, or, with CFW_PROBE_CAMERA set, the
// one whose SDK id starts with it (a host with several QHY cameras).
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
// Dying loudly on a missing device or an unmet precondition is the intended
// failure mode of a hand-run probe (the zwo-rs probe-example convention); the
// JSON lines are its output; and its arithmetic is on millisecond timestamps
// and one-byte slot codes.
#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use qhyccd_rs::{Camera, ControlType, Sdk, StreamMode};

static T0: OnceLock<Instant> = OnceLock::new();

/// The slots `rest`, `during` and `lost` move between.
const FROM: u32 = 2;
const TO: u32 = 4;
const ELSEWHERE: u32 = 5;
/// Long enough to rest a wheel: the measured drop window is 10–15 ms.
const RESTED: Duration = Duration::from_secs(2);

fn ms() -> f64 {
    (T0.get_or_init(Instant::now).elapsed().as_secs_f64() * 10_000.0).round() / 10.0
}

fn emit(line: &str) {
    println!("{{\"t\":{},{line}}}", ms());
}

fn or_null(t: Option<f64>) -> String {
    t.map_or_else(|| "null".to_string(), |t| format!("{t:.1}"))
}

/// One status read, logged with its raw value and how long it took; the slot
/// it names, if it names one. The CFW's hex-ASCII code is decoded here rather
/// than by the crate, so a code that names no slot is logged as it came.
fn read(cam: &Camera, tag: &str) -> Option<u32> {
    let start = ms();
    let raw = cam.get_parameter(ControlType::CfwPort);
    let slot = raw.as_ref().ok().and_then(|raw| match *raw as u32 {
        code @ 0x30..=0x39 => Some(code - 0x30),
        code @ 0x41..=0x46 => Some(code - 0x41 + 10),
        _ => None,
    });
    emit(&format!(
        "\"ev\":\"read\",\"tag\":\"{tag}\",\"start\":{start},\"raw\":{},\"slot\":{}",
        raw.map_or_else(|e| format!("\"{e:?}\""), |v| format!("{v}")),
        slot.map_or_else(|| "null".to_string(), |s| s.to_string())
    ));
    slot
}

/// Command a move; the time it was sent.
fn send(cam: &Camera, slot: u32, tag: &str) -> f64 {
    let start = ms();
    let code = if slot < 10 {
        0x30 + slot
    } else {
        0x41 + slot - 10
    };
    let sent = cam.set_parameter(ControlType::CfwPort, f64::from(code));
    emit(&format!(
        "\"ev\":\"move\",\"tag\":\"{tag}\",\"slot\":{slot},\"start\":{start},\"ok\":{}",
        sent.is_ok()
    ));
    start
}

/// The camera handshake's init: stream mode, readout mode 0, `InitQHYCCD`.
fn init_sequence(cam: &Camera, tag: &str) -> bool {
    let start = ms();
    let stream = cam.set_stream_mode(StreamMode::SingleFrameMode).is_ok();
    let mode = cam.set_readout_mode(0).is_ok();
    let init_start = ms();
    let init = cam.init();
    emit(&format!(
        "\"ev\":\"init\",\"tag\":\"{tag}\",\"start\":{start},\"init_start\":{init_start},\"stream\":{stream},\"mode\":{mode},\"ok\":{}",
        init.is_ok()
    ));
    init.is_ok()
}

/// Read until the wheel names `slot`; the time it did, or `None` at `limit`.
fn wait_for(cam: &Camera, slot: u32, limit: Duration, tag: &str) -> Option<f64> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if read(cam, tag) == Some(slot) {
            return Some(ms());
        }
    }
    None
}

/// Bring the wheel to `slot` with nothing else running, and rest it there.
fn settle_at(cam: &Camera, slot: u32) {
    if read(cam, "settle") != Some(slot) {
        send(cam, slot, "settle");
        assert!(
            wait_for(cam, slot, Duration::from_secs(30), "settle").is_some(),
            "precondition: the wheel reaches slot {slot} with nothing else running"
        );
    }
    thread::sleep(RESTED);
}

/// Bring the wheel to `slot` by a move of its own, ending on the read that saw
/// it arrive — no rest.
fn arrive_at(cam: &Camera, slot: u32) {
    settle_at(cam, if slot == FROM { TO } else { FROM });
    send(cam, slot, "arrive");
    assert!(
        wait_for(cam, slot, Duration::from_secs(30), "arrive").is_some(),
        "precondition: the wheel reaches slot {slot} with nothing else running"
    );
}

/// Read for `secs`, to catch the wheel moving.
fn watch(cam: &Camera, secs: u64, tag: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        read(cam, tag);
    }
}

/// The result of a move: when, after it was sent, the status named its slot.
fn outcome(cam: &Camera, slot: u32, sent: f64, limit: Duration, ev: &str) {
    let arrived = wait_for(cam, slot, limit, ev).map(|t| t - sent);
    emit(&format!(
        "\"ev\":\"{ev}\",\"slot\":{slot},\"arrived_after_ms\":{}",
        or_null(arrived)
    ));
}

fn arg<T: std::str::FromStr>(args: &[String], index: usize, what: &str) -> T {
    args.get(index)
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| panic!("usage: argument {index} is {what}"))
}

fn home(cam: &Camera, args: &[String]) {
    settle_at(cam, arg(args, 2, "a slot"));
    for n in 0..2 {
        init_sequence(cam, &format!("reinit-{n}"));
        watch(cam, 8, &format!("after-reinit-{n}"));
    }
    cam.close().expect("close");
    cam.open().expect("re-open");
    emit("\"ev\":\"reopened\"");
    watch(cam, 3, "after-reopen");
    init_sequence(cam, "reopen");
    watch(cam, 8, "after-reopen-init");
}

fn rest(cam: &Camera, args: &[String]) {
    let rest: u64 = arg(args, 2, "a rest in ms");
    arrive_at(cam, FROM);
    thread::sleep(Duration::from_millis(rest));
    if args.get(3).map(String::as_str) == Some("read") {
        read(cam, "idle");
    }
    let sent = send(cam, TO, "rest");
    outcome(cam, TO, sent, Duration::from_secs(8), "rest");
}

fn during(cam: &Camera, args: &[String]) {
    let call: String = arg(args, 2, "a camera call");
    let delay: u64 = arg(args, 3, "a delay in ms");
    settle_at(cam, FROM);
    let sent = send(cam, TO, "during");
    thread::sleep(Duration::from_millis(delay));
    let start = ms();
    let ok = match call.as_str() {
        "none" => true,
        "init" => cam.init().is_ok(),
        "readmode" => cam.set_readout_mode(0).is_ok(),
        "stream" => cam.set_stream_mode(StreamMode::SingleFrameMode).is_ok(),
        "sequence" => init_sequence(cam, "during"),
        "temps" => (0..10).all(|_| {
            cam.get_parameter(ControlType::CurTemp).is_ok()
                && cam.get_parameter(ControlType::CurPWM).is_ok()
        }),
        "exposure" => {
            let exposure_set = cam.set_parameter(ControlType::Exposure, 1000.0).is_ok();
            let started = cam.start_single_frame_exposure().is_ok();
            let mut buf = vec![0u8; cam.get_image_size().expect("image size")];
            exposure_set && started && cam.get_single_frame(&mut buf).is_ok()
        }
        other => panic!("usage: no camera call {other}"),
    };
    emit(&format!(
        "\"ev\":\"call\",\"call\":\"{call}\",\"start\":{start},\"ok\":{ok}"
    ));
    outcome(cam, TO, sent, Duration::from_secs(8), "during");
}

fn overlap(cam: &Camera, args: &[String]) {
    let delay: u64 = arg(args, 2, "a delay in ms");
    settle_at(cam, FROM);
    let sent = thread::scope(|s| {
        let init = s.spawn(|| init_sequence(cam, "overlap"));
        thread::sleep(Duration::from_millis(delay));
        let sent = send(cam, TO, "overlap");
        init.join().expect("init thread");
        sent
    });
    outcome(cam, TO, sent, Duration::from_secs(8), "overlap");
}

fn lost(cam: &Camera, args: &[String]) {
    let then: String = arg(args, 2, "resend or next");
    arrive_at(cam, FROM);
    let sent = send(cam, TO, "drop");
    outcome(cam, TO, sent, Duration::from_secs(4), "drop");
    thread::sleep(Duration::from_millis(500));
    if then == "resend" {
        let sent = send(cam, TO, "resend");
        outcome(cam, TO, sent, Duration::from_secs(8), "resend");
        thread::sleep(Duration::from_millis(500));
    }
    // One slot from TO is ~1.5 s of travel, three from FROM ~3.9 s: the time
    // says which the wheel set off from.
    let sent = send(cam, ELSEWHERE, "next");
    outcome(cam, ELSEWHERE, sent, Duration::from_secs(10), "next");
}

fn reads(cam: &Camera, args: &[String]) {
    for (n, delay) in args.iter().skip(2).enumerate() {
        let delay: u64 = delay.parse().expect("usage: a delay in ms");
        thread::scope(|s| {
            let reader = s.spawn(|| watch(cam, 2, &format!("reads-{n}")));
            thread::sleep(Duration::from_millis(delay));
            init_sequence(cam, &format!("reads-{n}"));
            reader.join().expect("reader thread");
        });
    }
}

fn settle(cam: &Camera, args: &[String]) {
    settle_at(cam, arg(args, 2, "a slot"));
}

/// The slot a wheel at rest names, which must not be slot 0: a first move to
/// slot 0 is the move under test.
fn off_zero(cam: &Camera) -> u32 {
    let from = read(cam, "start").expect("precondition: the status names a slot");
    assert_ne!(
        from, 0,
        "precondition: the wheel stands off slot 0 (run `settle` first)"
    );
    thread::sleep(RESTED);
    from
}

/// A move to slot 0, then the reads on past its arrival: the pace says when the
/// wheel stopped, whatever slot the status named.
fn to_zero(cam: &Camera) {
    let sent = send(cam, 0, "zero");
    outcome(cam, 0, sent, Duration::from_secs(10), "zero");
    watch(cam, 8, "zero-after");
}

fn zero(cam: &Camera) {
    off_zero(cam);
    to_zero(cam);
}

fn same(cam: &Camera) {
    let from = off_zero(cam);
    let sent = send(cam, from, "same");
    outcome(cam, from, sent, Duration::from_secs(5), "same");
    // A full turn of a seven-slot wheel is ~8.5 s.
    watch(cam, 10, "same-after");
    thread::sleep(RESTED);
    to_zero(cam);
}

/// A rested wheel's move from every slot to every other, each timed. The
/// process's first command goes to the slot the wheel stands on, so from then
/// on what the Linux SDK names in transit is the slot a move left, never its
/// target.
fn travel(cam: &Camera, slots: u32) {
    let mut at = read(cam, "travel").expect("precondition: the status names a slot");
    send(cam, at, "prime");
    assert!(
        wait_for(cam, at, Duration::from_secs(5), "prime").is_some(),
        "precondition: the wheel names the slot it was sent back to"
    );
    for from in 0..slots {
        for to in (0..slots).filter(|to| *to != from) {
            if at != from {
                timed_move(cam, at, from);
            }
            timed_move(cam, from, to);
            at = to;
        }
    }
}

/// A rested wheel's move from `from` to `to`: when, after it was sent, a
/// status read first named `to`.
fn timed_move(cam: &Camera, from: u32, to: u32) {
    thread::sleep(RESTED);
    let sent = send(cam, to, "travel");
    let arrived = wait_for(cam, to, Duration::from_secs(30), "travel").map(|t| t - sent);
    emit(&format!(
        "\"ev\":\"travel\",\"from\":{from},\"to\":{to},\"arrived_after_ms\":{}",
        or_null(arrived)
    ));
    assert!(arrived.is_some(), "the wheel reaches slot {to} within 30 s");
}

fn forget(cam: &Camera, args: &[String]) {
    let how: String = arg(args, 2, "none, reinit or reopen");
    arrive_at(cam, FROM);
    thread::sleep(RESTED);
    match how.as_str() {
        "none" => {}
        "reinit" => assert!(init_sequence(cam, "forget"), "the re-init succeeds"),
        "reopen" => {
            cam.close().expect("close");
            cam.open().expect("re-open");
            assert!(init_sequence(cam, "forget"), "the init succeeds");
        }
        other => panic!("usage: no way to forget {other}"),
    }
    thread::sleep(RESTED);
    let sent = send(cam, TO, "forget");
    outcome(cam, TO, sent, Duration::from_secs(8), "forget");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let _ = ms();
    let sdk = Sdk::new().expect("SDK::new failed");
    let wanted = std::env::var("CFW_PROBE_CAMERA").ok();
    let camera = sdk
        .cameras()
        .filter(|c| wanted.as_deref().is_none_or(|w| c.id().starts_with(w)))
        .last()
        .expect("precondition: the camera on the bus");
    camera.open().expect("precondition: the camera opens");
    assert!(
        camera.is_cfw_plugged_in().expect("CFW plug query"),
        "precondition: a CFW is plugged into the camera"
    );
    let slots = camera.cfw_slot_count().expect("slot count");
    emit(&format!(
        "\"ev\":\"start\",\"camera\":\"{}\",\"slots\":{slots}",
        camera.id()
    ));
    assert!(
        slots > ELSEWHERE,
        "precondition: the wheel has a slot {ELSEWHERE}"
    );
    assert!(
        init_sequence(camera, "connect"),
        "precondition: a first init succeeds"
    );

    match args.get(1).map(String::as_str) {
        Some("home") => home(camera, &args),
        Some("settle") => settle(camera, &args),
        Some("zero") => zero(camera),
        Some("same") => same(camera),
        Some("forget") => forget(camera, &args),
        Some("travel") => travel(camera, slots),
        Some("rest") => rest(camera, &args),
        Some("during") => during(camera, &args),
        Some("overlap") => overlap(camera, &args),
        Some("lost") => lost(camera, &args),
        Some("reads") => reads(camera, &args),
        _ => panic!("usage: cfw_probe home|rest|during|overlap|lost|reads|settle|zero|same|forget|travel ... (see the header)"),
    }
    camera.close().expect("close");
    emit("\"ev\":\"done\"");
}

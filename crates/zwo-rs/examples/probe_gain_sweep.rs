//! Exhaustive verification of the delivered ceiling at **every** gain the
//! camera advertises, for every attached camera.
//!
//! [`probe_ceiling`](probe_ceiling) samples four gains. That is enough to show
//! a clip is not a gain artifact, but not enough to *assert* a ceiling holds
//! across the whole register. This walks every integer gain from min to max.
//!
//! Exposure is scaled to hold the illumination roughly constant: ASI gain is in
//! 0.1 dB units, so the linear factor is `10^(gain/200)` and the exposure is
//! divided by it. Without that, a fixed exposure would leave low gains dark
//! (where the test is vacuous — an unsaturated frame trivially fails to exceed
//! any ceiling) and waste time at high gains.
//!
//! ```sh
//! cargo run -p zwo-rs --no-default-features --features camera --example probe_gain_sweep
//! ```
//!
//! This runs for roughly an hour across three cameras, most of it exposing at
//! low gain. It is read-mostly: it sets gain, exposure and ROI format and reads
//! frames. It never touches the cooler.
// Bench diagnostic, run by hand against attached hardware: dying loudly on a
// missing camera or SDK fault is the intended failure mode, and the gain/ADU
// arithmetic runs on values the probe itself bounds by the advertised range.
// Its display statistics cast counts and sums to f64; nothing feeds back into
// control flow, so lossless-conversion pedantry buys nothing here.
#![allow(
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

use zwo_rs::{Camera, ControlType, ExposureStatus, ImageType, Sdk};

/// Exposure at gain 0. Chosen because it is where an ordinary indoor scene
/// just reaches saturation on these sensors; every other gain is scaled from it.
const BASE_US: f64 = 15_000_000.0;
/// Don't go below a duration the SDK can honour meaningfully.
const MIN_US: i64 = 1_000;

fn main() {
    let sdk = Sdk::new().expect("SDK init");
    let cameras = sdk.cameras().expect("enumerate cameras");
    println!("{} camera(s) attached\n", cameras.len());

    for (index, info) in cameras.iter().enumerate() {
        // Every frame below is Raw16, so a camera that does not offer it has
        // nothing to contribute. Skip rather than let `set_roi_format` panic
        // through `expect` and abort the remaining cameras — this sweep runs
        // for an hour, and losing the cameras after the odd one out would be a
        // poor trade. (RM3 notes no such ASI model is known; Raw8 is the SDK's
        // universal baseline. This is cheap insurance, not an observed case.)
        if !info.supported_video_formats.contains(&ImageType::Raw16) {
            println!(
                "-- camera {index} ({}) advertises {:?}, no Raw16 — skipped",
                info.name, info.supported_video_formats
            );
            continue;
        }
        let camera = match sdk.open_camera(index) {
            Ok(camera) => camera,
            Err(e) => {
                println!("!! camera {index} ({}) open failed: {e}", info.name);
                continue;
            }
        };
        let caps = camera.control_caps().expect("control caps");
        let Some(gain) = caps.iter().find(|c| c.control_type == ControlType::Gain) else {
            println!("!! camera {index} ({}) exposes no Gain control", info.name);
            continue;
        };

        // What the driver publishes (ST3, with the one-step margin) and what a
        // bare shift would predict. The gap between them is the margin itself.
        // `expected_shift` is `None` outside the shifted range: there is no
        // packing to disagree with, and `16 - depth` would underflow if the SDK
        // ever reported a depth above the container's own 16.
        let depth = info.bit_depth;
        let (reported, shifted_full_scale, expected_shift) = if depth <= 1 || depth >= 16 {
            (u32::from(u16::MAX), u32::from(u16::MAX), None)
        } else {
            (
                ((1u32 << depth) - 2) << (16 - depth),
                ((1u32 << depth) - 1) << (16 - depth),
                Some(16 - depth),
            )
        };

        let (width, height) = (
            info.max_width - info.max_width % 8,
            info.max_height - info.max_height % 2,
        );
        println!("=== camera {index}: {} ({depth}-bit) ===", info.name);
        println!("gain range {}..={}, {width}x{height}", gain.min, gain.max);
        println!("driver reports MaxADU {reported}; a bare shift would give {shifted_full_scale}");

        let mut global_max = 0u16;
        let mut attained = 0usize;
        let mut over_reported = 0usize;
        let mut wrong_shift = Vec::new();
        let mut failures = Vec::new();
        let started = Instant::now();

        for g in gain.min..=gain.max {
            let scale = 10f64.powf(g as f64 / 200.0);
            // Bounded: BASE_US / scale is in (0, 15e6]; sub-microsecond truncation is noise.
            #[allow(clippy::cast_possible_truncation)]
            let exposure_us = ((BASE_US / scale) as i64).max(MIN_US);
            let Some(pixels) = expose(&camera, width, height, g, exposure_us) else {
                failures.push(g);
                continue;
            };
            let max = pixels.iter().copied().max().unwrap_or(0);
            let at_max = pixels.iter().filter(|&&p| p == max).count();
            let above_reported = pixels.iter().filter(|&&p| u32::from(p) > reported).count();
            let shift = trailing_zero_bits(&pixels);

            global_max = global_max.max(max);
            if u32::from(max) >= reported {
                attained += 1;
            }
            if above_reported > 0 {
                over_reported += 1;
            }
            // The shift signature must not move with gain. A fully flat frame
            // reports more zero bits than the packing has, so only a *smaller*
            // count is a real disagreement.
            if expected_shift.is_some_and(|expected| shift < expected) {
                wrong_shift.push((g, shift));
            }

            println!(
                "  gain {g:>3} {exposure_us:>8}us: max {max:>5} ×{at_max:<8} \
                 >MaxADU {above_reported:<8} shift {shift}"
            );
        }

        let span = gain.max - gain.min + 1;
        println!(
            "  --- camera {index} summary over {span} gain values, {:?} ---",
            started.elapsed()
        );
        println!("  global max delivered      : {global_max}");
        println!("  gains reaching MaxADU     : {attained}/{span}");
        println!("  gains with pixels > MaxADU: {over_reported}/{span}");
        println!(
            "  gains exceeding the shift : {}",
            u32::from(global_max) > shifted_full_scale
        );
        println!("  packing disagreements     : {wrong_shift:?}");
        println!("  exposure failures         : {failures:?}\n");
    }
}

/// Trailing zero bits common to every pixel — the left shift, as long as the
/// frame is not completely flat.
fn trailing_zero_bits(pixels: &[u16]) -> u32 {
    let or_all = pixels.iter().fold(0u16, |acc, &p| acc | p);
    if or_all == 0 {
        16
    } else {
        or_all.trailing_zeros()
    }
}

fn expose(
    camera: &Camera,
    width: u32,
    height: u32,
    gain: i64,
    exposure_us: i64,
) -> Option<Vec<u16>> {
    camera
        .set_roi_format(width, height, 1, ImageType::Raw16)
        .expect("set_roi_format");
    camera
        .set_control_value(ControlType::Gain, gain, false)
        .expect("set gain");
    camera
        .set_control_value(ControlType::Exposure, exposure_us, false)
        .expect("set exposure");
    camera.start_exposure(true).expect("start_exposure");

    let deadline = Instant::now()
        + Duration::from_micros(u64::try_from(exposure_us).expect("exposure is non-negative"))
        + Duration::from_secs(30);
    loop {
        match camera.exposure_status().expect("exposure_status") {
            ExposureStatus::Success => break,
            ExposureStatus::Failed => return None,
            _ => {}
        }
        if Instant::now() >= deadline {
            let _ = camera.stop_exposure();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let roi = camera.roi_format().expect("roi_format");
    let mut buf = vec![0u8; roi.buffer_len().expect("addressable frame")];
    camera.download_exposure(&mut buf).expect("download");
    Some(
        buf.chunks_exact(2)
            // The camera puts Raw16 on the wire low byte first, which
            // `from_le_bytes` says directly.
            .map(|c| u16::from_le_bytes(c.try_into().expect("chunks_exact(2) yields pairs")))
            .collect(),
    )
}

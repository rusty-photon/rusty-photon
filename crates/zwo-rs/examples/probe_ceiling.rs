//! Hardware probe for issue #888: what is the **true delivered ceiling** in
//! `Raw16`, and does it hold across gain and binning?
//!
//! [`probe_formats`](probe_formats) answers *how* the ADC's bits sit in the
//! 16-bit container by measuring whatever the scene happened to give. That is
//! enough to tell a left shift from a rescale, but not enough to state the
//! ceiling: the brightest pixel in an ordinary frame is a property of the room,
//! not of the camera. This probe deliberately blows the frame out, then prints
//! the tail of the histogram — a hard ceiling shows as a spike at one value
//! with nothing above it, however much more light you throw at it.
//!
//! It sweeps gain and bin because neither is obviously neutral: gain could in
//! principle scale the clip, and binning changes how neighbouring ADC counts
//! are combined. The driver publishes a single `MaxADU` for all of them
//! (ST3), so they all have to agree.
//!
//! Run against a physically attached camera, with the real SDK linked, in front
//! of a light source bright enough to saturate:
//!
//! ```sh
//! cargo run -p zwo-rs --no-default-features --features camera --example probe_ceiling
//! ```
//!
//! It takes several deliberately long exposures, so it runs for a few minutes.
//! It is read-mostly: it sets gain, exposure and ROI format and reads frames.
//! It never touches the cooler.
// Bench diagnostic, run by hand against attached hardware: dying loudly on a
// missing camera or SDK fault is the intended failure mode, and the histogram
// arithmetic runs on counters the probe itself bounds by frame size.
// Its display statistics cast counts and sums to f64; nothing feeds back into
// control flow, so lossless-conversion pedantry buys nothing here.
#![allow(
    clippy::expect_used,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use zwo_rs::{Camera, ControlType, ExposureStatus, ImageType, Sdk};

/// Well-exposed: leaves a spread of values, so the low-bit packing test says
/// something. A saturated frame answers that question with "everything is flat".
const MODERATE_US: i64 = 100_000;
/// Long enough that an ordinary indoor scene is comprehensively blown out, so
/// the maximum is the container's ceiling and not the brightest thing in view.
const BLOWN_OUT_US: i64 = 15_000_000;

/// One exposure's worth of settings.
struct Shot {
    image_type: ImageType,
    width: u32,
    height: u32,
    bin: u32,
    gain: i64,
    exposure_us: i64,
}

fn main() {
    let sdk = Sdk::new().expect("SDK init");
    let cameras = sdk.cameras().expect("enumerate cameras");
    let info = cameras.first().expect("a camera must be attached");
    println!("=== {} — {}-bit ADC ===", info.name, info.bit_depth);

    let camera = sdk.open_camera(0).expect("open camera 0");
    let caps = camera.control_caps().expect("control caps");
    let gain = caps
        .iter()
        .find(|c| c.control_type == ControlType::Gain)
        .expect("a Gain control");
    println!(
        "gain: min {} max {} default {}",
        gain.min, gain.max, gain.default
    );

    // Two different numbers, and conflating them is what this probe exists to
    // avoid. `reported` is what the driver publishes as MaxADU (ST3, including
    // its one-step margin and its `bit_depth <= 1` guard — the depth arrives as
    // `unwrap_or(0)` at the FFI boundary). `shifted_full_scale` is what a bare
    // `16 - BitDepth` shift would predict, which is the value the measurement
    // below asks whether the sensor actually reaches.
    let depth = info.bit_depth;
    let (reported, shifted_full_scale) = if depth <= 1 || depth >= 16 {
        (u32::from(u16::MAX), u32::from(u16::MAX))
    } else {
        (
            ((1u32 << depth) - 2) << (16 - depth),
            ((1u32 << depth) - 1) << (16 - depth),
        )
    };
    println!("driver reports MaxADU {reported}; a bare shift would give {shifted_full_scale}");

    // Every geometry must satisfy the SDK's width%8 / height%2 rule *after*
    // binning, so align each binned extent independently.
    let align = |w: u32, h: u32| (w - w % 8, h - h % 2);
    let full = |bin: u32| align(info.max_width / bin, info.max_height / bin);

    println!("\n--- packing, well exposed (is it shifted or rescaled?) ---");
    for bin in info.supported_bins.iter().copied() {
        let (width, height) = full(bin);
        report(
            &camera,
            &Shot {
                image_type: ImageType::Raw16,
                width,
                height,
                bin,
                gain: gain.default,
                exposure_us: MODERATE_US,
            },
        );
    }

    // Quarters of the camera's *own* range. Hard-coded gains would silently
    // exceed it — the ASI120MC-S maxes at 100, where a requested 300 is
    // clamped by the SDK and the probe would print a gain the camera never
    // held, reported as if it were a separate data point.
    let span = gain.max - gain.min;
    let gains = [gain.min, gain.min + span / 4, gain.min + span / 2, gain.max];

    println!("\n--- ceiling vs gain, blown out (is the clip fixed?) ---");
    for g in gains {
        let (width, height) = full(1);
        report(
            &camera,
            &Shot {
                image_type: ImageType::Raw16,
                width,
                height,
                bin: 1,
                gain: g,
                exposure_us: BLOWN_OUT_US,
            },
        );
    }

    println!("\n--- ceiling vs bin, blown out (one MaxADU must cover all) ---");
    for bin in info.supported_bins.iter().copied() {
        let (width, height) = full(bin);
        report(
            &camera,
            &Shot {
                image_type: ImageType::Raw16,
                width,
                height,
                bin,
                gain: gain.max,
                exposure_us: BLOWN_OUT_US,
            },
        );
    }

    println!("\n--- Raw8 ceiling, blown out ---");
    let (width, height) = full(1);
    report(
        &camera,
        &Shot {
            image_type: ImageType::Raw8,
            width,
            height,
            bin: 1,
            gain: gain.max,
            exposure_us: BLOWN_OUT_US,
        },
    );
}

/// Take one frame and print what it says about the ceiling and the packing.
fn report(camera: &Camera, shot: &Shot) {
    let prefix = format!(
        "  {:?} bin {} gain {:>3} {:>8}us:",
        shot.image_type, shot.bin, shot.gain, shot.exposure_us
    );
    let buf = match expose(camera, shot) {
        Ok(buf) => buf,
        Err(reason) => {
            println!("{prefix} {reason}");
            return;
        }
    };

    if shot.image_type == ImageType::Raw8 {
        let ceiling = buf.iter().copied().max().unwrap_or(0);
        // One-shot probe; the bytecount dependency is not worth it here.
        #[allow(clippy::naive_bytecount)]
        let at_ceiling = buf.iter().filter(|&&b| b == ceiling).count();
        println!(
            "{prefix} max {ceiling}, {at_ceiling} px at max ({:.2}%)",
            at_ceiling as f64 * 100.0 / buf.len() as f64
        );
        return;
    }

    let pixels: Vec<u16> = buf
        .chunks_exact(2)
        // The camera puts Raw16 on the wire low byte first, which
        // `from_le_bytes` says directly.
        .map(|c| u16::from_le_bytes(c.try_into().expect("chunks_exact(2) yields pairs")))
        .collect();
    let mean = pixels.iter().map(|&p| u64::from(p)).sum::<u64>() as f64 / pixels.len() as f64;
    println!(
        "{prefix} mean {mean:.0}, {}, top {}",
        packing(&pixels),
        histogram_tail(&pixels)
    );
}

/// How the ADC's bits sit in the 16-bit container: a bare left shift leaves the
/// low bits always zero, a genuine rescale (or a binning sum) populates them.
fn packing(pixels: &[u16]) -> String {
    let low_zero = |mask: u16| pixels.iter().all(|p| p & mask == 0);
    match (1..=8u16).find(|&s| !low_zero((1 << s) - 1)) {
        Some(1) => "low bits populated (rescaled/summed)".to_owned(),
        Some(s) => format!("low {} bits always zero (shifted by {})", s - 1, s - 1),
        None => "low 8 bits always zero (or the frame is flat)".to_owned(),
    }
}

/// The highest distinct values with their pixel counts. A spike on the top
/// value with a smooth run below it is a clip; a single pixel at the top is
/// just the brightest thing in the scene.
fn histogram_tail(pixels: &[u16]) -> String {
    let mut counts: BTreeMap<u16, usize> = BTreeMap::new();
    for &p in pixels {
        *counts.entry(p).or_default() += 1;
    }
    counts
        .iter()
        .rev()
        .take(4)
        .map(|(value, n)| format!("{value}×{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Apply the settings, expose, and download. `Err` with the reason if the SDK
/// reported a failed exposure or the frame never arrived — a sweep of a dozen
/// long exposures should report the bad one and carry on, not die on it.
fn expose(camera: &Camera, shot: &Shot) -> Result<Vec<u8>, String> {
    camera
        .set_roi_format(shot.width, shot.height, shot.bin, shot.image_type)
        .expect("set_roi_format");
    camera
        .set_control_value(ControlType::Gain, shot.gain, false)
        .expect("set gain");
    camera
        .set_control_value(ControlType::Exposure, shot.exposure_us, false)
        .expect("set exposure");
    // `is_dark = true`: these models have no mechanical shutter, so this is
    // just a frame with whatever light is present. Nothing is actuated.
    camera.start_exposure(true).expect("start_exposure");

    // A real-clock deadline generous enough for the longest exposure here.
    let deadline = Instant::now()
        + Duration::from_micros(u64::try_from(shot.exposure_us).expect("exposure is non-negative"))
        + Duration::from_secs(30);
    loop {
        match camera.exposure_status().expect("exposure_status") {
            ExposureStatus::Success => break,
            ExposureStatus::Failed => return Err("exposure FAILED (SDK)".to_owned()),
            _ => {}
        }
        if Instant::now() >= deadline {
            // Leave the camera idle rather than mid-exposure, so the next shot
            // in the sweep starts from a clean state.
            let _ = camera.stop_exposure();
            return Err("exposure TIMED OUT".to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let roi = camera.roi_format().expect("roi_format");
    let mut buf = vec![0u8; roi.buffer_len().expect("addressable frame")];
    camera.download_exposure(&mut buf).expect("download");
    Ok(buf)
}

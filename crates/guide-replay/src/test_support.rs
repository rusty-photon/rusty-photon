//! What the tests share: bodies and recordings written as `record.ps1`
//! writes them, and a simulated sky to record.

use std::fs;
use std::path::Path;
use std::time::Duration;

use guide_simulator::{
    CameraConfig, Drift, FrameTruth, MountConfig, Seeing, SimConfig, Simulator, SkyConfig,
    SkyOffset, Star, StarField,
};
use ndarray::Array2;

/// A recorded frame's exposure.
pub const EXPOSURE: Duration = Duration::from_secs(2);

/// The time between one frame's end and the next one's start.
pub const DOWNLOAD: Duration = Duration::from_millis(500);

/// rig2's guide camera binned 2 × 2: 4 µm pixels at 530 mm.
pub const RIG2_SCALE_ARCSEC: f64 = 1.557;

/// A body with `header` as its metadata and `pixels` after it.
pub fn body(header: [i32; 11], pixels: &[u8]) -> Vec<u8> {
    let mut body: Vec<u8> = header.iter().flat_map(|word| word.to_le_bytes()).collect();
    body.extend_from_slice(pixels);
    body
}

/// `pixels` as a camera sends them as 16-bit unsigned values.
pub fn encode(pixels: &Array2<u16>) -> Vec<u8> {
    let (rows, columns) = pixels.dim();
    let data: Vec<u8> = pixels
        .t()
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    body(
        [
            1,
            0,
            0,
            0,
            44,
            2,
            8,
            2,
            i32::try_from(columns).unwrap(),
            i32::try_from(rows).unwrap(),
            0,
        ],
        &data,
    )
}

pub fn write_camera(dir: &Path, json: &str) {
    fs::write(dir.join("camera.json"), json).unwrap();
}

/// Writes a recording of `frames` from a camera of Alpaca sensor type
/// `sensortype`, a frame every 2.5 s; `None` is a frame the recorder did
/// not get.
pub fn write_recording(dir: &Path, sensortype: i64, frames: &[Option<Array2<u16>>]) {
    write_camera(
        dir,
        &format!(r#"{{"name": "simulated", "sensortype": {sensortype}, "maxadu": 65535}}"#),
    );
    let mut index =
        vec!["frame,file,start_utc,exposure_s,bin,numx,numy,bytes,download_ms,error".to_owned()];
    for (n, frame) in (1_u32..).zip(frames) {
        let tenths = 25 * (n - 1);
        let start = format!(
            "2026-11-23T03:{:02}:{:02}.{}00Z",
            tenths / 600,
            (tenths / 10) % 60,
            tenths % 10
        );
        // An RGGB camera is recorded unbinned, as the guider reads it.
        let bin = if sensortype == 2 { 1 } else { 2 };
        index.push(match frame {
            Some(pixels) => {
                let file = format!("frame-{n:05}.imagebytes");
                let body = encode(pixels);
                let (rows, columns) = pixels.dim();
                fs::write(dir.join(&file), &body).unwrap();
                format!(
                    "{n},{file},{start},2,{bin},{columns},{rows},{},20.0,",
                    body.len()
                )
            }
            None => format!("{n},,{start},2,{bin},,,0,,imagearray HTTP 500"),
        });
    }
    index.push(String::new());
    fs::write(dir.join("frames.csv"), index.join("\n")).unwrap();
}

fn star(east: f64, north: f64, e_per_s: f64) -> Star {
    Star {
        position: SkyOffset::new(east, north),
        e_per_s,
    }
}

/// A 480 × 320 field at rig2's binned scale, tracked with a slow drift,
/// in 2.5″ seeing whose image motion is common to the field but for
/// 0.3″ of each star's own: ten stars well inside the edge, the
/// faintest two at the edge of the eight the guider keeps.
pub fn sky(adjust: impl FnOnce(&mut SimConfig)) -> Simulator {
    let mut config = SimConfig {
        seed: 23,
        mount: MountConfig {
            declination_deg: 30.0,
            drift: Drift {
                east_arcsec_per_min: 1.0,
                north_arcsec_per_min: -0.5,
            },
            ..MountConfig::default()
        },
        camera: CameraConfig {
            width: 480,
            height: 320,
            pixel_scale_arcsec: RIG2_SCALE_ARCSEC,
            ..CameraConfig::default()
        },
        sky: SkyConfig {
            stars: StarField::Listed(vec![
                star(-250.0, -150.0, 60_000.0),
                star(200.0, 120.0, 40_000.0),
                star(0.0, 0.0, 30_000.0),
                star(-200.0, 150.0, 25_000.0),
                star(250.0, -140.0, 20_000.0),
                star(100.0, -60.0, 15_000.0),
                star(-80.0, 90.0, 12_000.0),
                star(-300.0, 20.0, 10_000.0),
                star(300.0, 40.0, 8_000.0),
                star(40.0, 160.0, 6_000.0),
            ]),
            seeing: Seeing {
                fwhm_arcsec: 2.5,
                common_motion_arcsec: 0.5,
                star_motion_arcsec: 0.3,
            },
            clouds: Vec::new(),
        },
    };
    adjust(&mut config);
    Simulator::new(config).unwrap()
}

/// Records `frames` frames of `sim` into `dir`, each passed to `edit`
/// with its number from 1 and its truth before it is written.
pub fn record(
    dir: &Path,
    sim: &mut Simulator,
    frames: usize,
    mut edit: impl FnMut(usize, &mut Array2<u16>, &FrameTruth),
) {
    let shots: Vec<Option<Array2<u16>>> = (1..=frames)
        .map(|n| {
            let mut shot = sim.expose(EXPOSURE);
            edit(n, &mut shot.pixels, &shot.truth);
            sim.advance(DOWNLOAD);
            Some(shot.pixels)
        })
        .collect();
    write_recording(dir, 0, &shots);
}

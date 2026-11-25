//! The latest frame as a picture for a person.
//!
//! The measured frame stretched to eight bits, with a circle around where
//! each guide star is looked for in the next frame
//! (docs/services/guider.md § `GET /api/v1/frame/latest`).
//!
//! The stretch is linear from the frame's background less two standard
//! deviations, black, to the background plus thirty, white: the sky's noise
//! shows, and a star a few deviations above it stands out.

use num_traits::ToPrimitive;
use rp_guiding::{Frame, Position};

/// Where black and white lie, in the background's standard deviations
/// from its mean.
const BLACK_SIGMA: f64 = 2.0;
const WHITE_SIGMA: f64 = 30.0;

/// The colour of a search region's circle.
const CIRCLE: [u8; 3] = [0, 255, 0];

/// `frame` as a PNG, with a circle of `radius` pixels around each of
/// `centres`.
///
/// # Errors
///
/// A message when the frame is too large for a PNG or cannot be encoded.
pub fn render(frame: &Frame, centres: &[Position], radius: f64) -> Result<Vec<u8>, String> {
    let pixels = frame.pixels();
    let (height, width) = pixels.dim();
    let background = frame.background();
    let (black, white) = levels(background.mean, background.stddev);
    let mut rgb: Vec<u8> = pixels
        .iter()
        .flat_map(|&value| {
            let grey = grey(value, black, white);
            [grey, grey, grey]
        })
        .collect();
    for centre in centres {
        circle(&mut rgb, width, height, *centre, radius);
    }
    let (Ok(png_width), Ok(png_height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(format!(
            "a frame of {width} × {height} is too large for a PNG"
        ));
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, png_width, png_height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("writing the PNG's header failed: {e}"))?;
    writer
        .write_image_data(&rgb)
        .map_err(|e| format!("writing the PNG's pixels failed: {e}"))?;
    writer
        .finish()
        .map_err(|e| format!("finishing the PNG failed: {e}"))?;
    Ok(out)
}

/// The black and the white of the stretch of a frame whose background has
/// `mean` and `stddev`.
fn levels(mean: f64, stddev: f64) -> (f64, f64) {
    (
        BLACK_SIGMA.mul_add(-stddev, mean),
        WHITE_SIGMA.mul_add(stddev, mean),
    )
}

/// `value` stretched linearly from `black` to `white`; mid-grey when the
/// two meet, as they do in a frame of one value.
fn grey(value: i32, black: f64, white: f64) -> u8 {
    let span = white - black;
    if span <= 0.0 {
        return 128;
    }
    ((f64::from(value) - black) / span * 255.0)
        .clamp(0.0, 255.0)
        .round()
        .to_u8()
        .unwrap_or(0)
}

/// Draw a circle of `radius` around `centre` into `rgb`, a frame of
/// `width` × `height` pixels, three bytes each, rows first.
fn circle(rgb: &mut [u8], width: usize, height: usize, centre: Position, radius: f64) {
    // Points closer together than a pixel along the circumference.
    let steps = (std::f64::consts::TAU * radius * 2.0)
        .ceil()
        .to_usize()
        .unwrap_or(0)
        .max(16);
    let Some(step) = steps.to_f64().map(|steps| std::f64::consts::TAU / steps) else {
        return;
    };
    for index in 0..steps {
        let angle = index.to_f64().unwrap_or(0.0) * step;
        let x = radius.mul_add(angle.cos(), centre.x).round().to_usize();
        let y = radius.mul_add(angle.sin(), centre.y).round().to_usize();
        let (Some(x), Some(y)) = (x, y) else {
            continue;
        };
        if x >= width || y >= height {
            continue;
        }
        let at = y
            .checked_mul(width)
            .and_then(|row| row.checked_add(x))
            .and_then(|pixel| pixel.checked_mul(3));
        if let Some(pixel) = at.and_then(|at| rgb.get_mut(at..at.saturating_add(3))) {
            pixel.copy_from_slice(&CIRCLE);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buffer).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgb);
        buffer.truncate(info.buffer_size());
        (info.width, info.height, buffer)
    }

    /// A 40 × 30 frame of a noisy background around 1,000 with one bright
    /// pixel.
    fn frame() -> Frame {
        let pixels = ndarray::Array2::from_shape_fn((30, 40), |(row, col)| {
            if (row, col) == (10, 20) {
                60_000_u16
            } else {
                1_000 + u16::try_from((row * 7 + col * 13) % 11).unwrap()
            }
        });
        Frame::new(&pixels.view(), rp_guiding::SensorType::Monochrome, None).unwrap()
    }

    fn pixel(rgb: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
        let at = usize::try_from((y * width + x) * 3).unwrap();
        [rgb[at], rgb[at + 1], rgb[at + 2]]
    }

    #[test]
    fn the_picture_is_the_frame_stretched_from_the_background() {
        let (width, height, rgb) = decode(&render(&frame(), &[], 5.0).unwrap());
        assert_eq!((width, height), (40, 30));
        assert_eq!(pixel(&rgb, width, 20, 10), [255, 255, 255]);
        let sky = pixel(&rgb, width, 0, 0);
        assert!(sky[0] > 0 && sky[0] < 64, "the sky is dark grey: {sky:?}");
        assert_eq!(sky[0], sky[1]);
        assert_eq!(sky[1], sky[2]);
    }

    #[test]
    fn the_stretch_runs_from_two_deviations_below_the_background_to_thirty_above() {
        assert_eq!(levels(1_000.0, 10.0), (980.0, 1_300.0));
        assert_eq!(grey(980, 980.0, 1_300.0), 0);
        assert_eq!(grey(1_140, 980.0, 1_300.0), 128);
        assert_eq!(grey(1_300, 980.0, 1_300.0), 255);
    }

    #[test]
    fn values_beyond_the_stretch_are_black_or_white() {
        assert_eq!(grey(900, 980.0, 1_300.0), 0);
        assert_eq!(grey(60_000, 980.0, 1_300.0), 255);
    }

    #[test]
    fn a_circle_is_drawn_around_each_search_centre_and_inside_the_frame_only() {
        let centres = [Position { x: 20.0, y: 10.0 }, Position { x: 1.0, y: 1.0 }];
        let (width, _, rgb) = decode(&render(&frame(), &centres, 5.0).unwrap());
        assert_eq!(pixel(&rgb, width, 25, 10), CIRCLE);
        assert_eq!(pixel(&rgb, width, 20, 15), CIRCLE);
        assert_eq!(pixel(&rgb, width, 20, 10), [255, 255, 255]);
        assert_eq!(pixel(&rgb, width, 6, 1), CIRCLE);
    }

    #[test]
    fn a_constant_frame_is_mid_grey() {
        let pixels = ndarray::Array2::<u16>::from_elem((8, 8), 500);
        let frame = Frame::new(&pixels.view(), rp_guiding::SensorType::Monochrome, None).unwrap();
        let (width, _, rgb) = decode(&render(&frame, &[], 2.0).unwrap());
        assert_eq!(pixel(&rgb, width, 3, 3), [128, 128, 128]);
    }
}

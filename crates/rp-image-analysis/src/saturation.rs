//! The level at or above which a pixel is saturated.
//!
//! It is the frame's own clip when it shows one, the camera's `max_adu`
//! otherwise, the lower of the two when both are known. See
//! `docs/services/rp.md` (`measure_basic` contract, the saturation level).
//!
//! A camera's `max_adu` can lie above anything its sensor delivers: a
//! QHY camera reports its 16-bit container's top, 65535, while its frames
//! clip at 65534 or, shifted from a 12- or 14-bit ADC, at 65520 or 65532.
//! A sensor that clips piles pixels on one value with none above it, so
//! the clip is read from the frame.

use ndarray::ArrayView2;

use super::pixel::Pixel;

/// How many pixels on a frame's maximum make it a clip.
///
/// The brightest pixels of a frame that does not clip spread over many
/// values, one or two to a value — but a star centred on a pixel corner
/// puts its four core pixels on one value when the data are quantized
/// (a 12-bit sensor shifted into 16 bits steps by 16), so four are not a
/// clip. On rig2's 79 clipped frames, 7 to 1,310 pixels hold the maximum.
pub const CLIP_PIXELS: usize = 5;

/// The frame's clip: its maximum, when at least [`CLIP_PIXELS`] pixels
/// hold it. `None` for a frame that does not clip, and for an empty one.
#[must_use]
pub fn frame_clip<T: Pixel>(view: &ArrayView2<T>) -> Option<u32> {
    let mut max: Option<u32> = None;
    let mut holding = 0_usize;
    for value in view.iter().map(|pixel| pixel.to_u32()) {
        match max {
            Some(top) if value < top => {}
            Some(top) if value == top => holding = holding.saturating_add(1),
            _ => {
                max = Some(value);
                holding = 1;
            }
        }
    }
    max.filter(|_| holding >= CLIP_PIXELS)
}

/// The frame's saturation level: its clip, the camera's `max_adu`, or the
/// lower of the two when both are known. The lower one never flags fewer
/// pixels than `max_adu` alone would. `None` when neither is known.
#[must_use]
pub fn saturation_level<T: Pixel>(view: &ArrayView2<T>, max_adu: Option<u32>) -> Option<u32> {
    match (frame_clip(view), max_adu) {
        (Some(clip), Some(max_adu)) => Some(clip.min(max_adu)),
        (clip, max_adu) => clip.or(max_adu),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;

    /// A frame of distinct values, 1000 upward, none repeated.
    fn ramp() -> Array2<u16> {
        Array2::from_shape_fn((16, 16), |(r, c)| 1000 + u16::try_from(r * 16 + c).unwrap())
    }

    #[test]
    fn a_maximum_five_pixels_hold_is_a_clip() {
        let mut arr = ramp();
        for c in 0..5 {
            arr[(8, c)] = 65534;
        }
        assert_eq!(frame_clip(&arr.view()), Some(65534));
    }

    #[test]
    fn a_maximum_up_to_four_pixels_hold_is_not() {
        let mut arr = ramp();
        assert_eq!(frame_clip(&arr.view()), None, "every value once");
        // A star centred on a pixel corner, its four core pixels on one
        // quantized value, and nothing brighter.
        for (r, c) in [(7, 7), (7, 8), (8, 7), (8, 8)] {
            arr[(r, c)] = 41_600;
        }
        assert_eq!(
            frame_clip(&arr.view()),
            None,
            "four pixels on the top value"
        );
    }

    #[test]
    fn the_clip_is_the_maximum_not_a_repeated_value_below_it() {
        // Many pixels on 5000, and one brighter pixel above them: 5000 is
        // no clip, since a clipping sensor puts nothing above its clip.
        let mut arr = ramp();
        for c in 0..10 {
            arr[(4, c)] = 5000;
        }
        arr[(15, 15)] = 6000;
        assert_eq!(frame_clip(&arr.view()), None);
    }

    #[test]
    fn the_level_is_the_lower_of_the_clip_and_max_adu() {
        let mut arr = ramp();
        for c in 0..5 {
            arr[(8, c)] = 65534;
        }
        let view = arr.view();
        assert_eq!(
            saturation_level(&view, Some(65535)),
            Some(65534),
            "a container top above the clip"
        );
        assert_eq!(
            saturation_level(&view, Some(65504)),
            Some(65504),
            "a max_adu below the clip"
        );
        assert_eq!(
            saturation_level(&view, None),
            Some(65534),
            "no max_adu: the clip"
        );
        assert_eq!(
            saturation_level(&ramp().view(), Some(65535)),
            Some(65535),
            "no clip: max_adu"
        );
        assert_eq!(saturation_level(&ramp().view(), None), None);
    }

    #[test]
    fn an_empty_frame_has_no_clip() {
        let arr = Array2::<u16>::zeros((0, 0));
        assert_eq!(frame_clip(&arr.view()), None);
    }
}

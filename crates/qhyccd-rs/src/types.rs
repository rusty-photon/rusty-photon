#[derive(Debug, PartialEq, Eq)]
/// Stream mode used in `set_stream_mode`
pub enum StreamMode {
    /// Long exposure mode
    SingleFrameMode = 0,
    /// Live video mode
    LiveMode = 1,
}

impl From<StreamMode> for u8 {
    /// The SDK wire value for the mode — the discriminants above, which match
    /// `SetQHYCCDStreamMode`'s own numbering.
    fn from(mode: StreamMode) -> Self {
        match mode {
            StreamMode::SingleFrameMode => 0,
            StreamMode::LiveMode => 1,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
/// Camera sensor info
pub struct CCDChipInfo {
    /// chip width in um
    pub chip_width: f64,
    /// chip height in um
    pub chip_height: f64,
    /// number of horizontal pixels
    pub image_width: u32,
    /// number of vertical pixels
    pub image_height: u32,
    /// pixel width in um
    pub pixel_width: f64,
    /// pixel height in um
    pub pixel_height: f64,
    /// maximum bit depth for transfer
    pub bits_per_pixel: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
/// Metadata describing a frame downloaded by
/// [`Camera::get_live_frame`](crate::Camera::get_live_frame) /
/// [`Camera::get_single_frame`](crate::Camera::get_single_frame).
///
/// The pixel bytes are written into the caller-owned `&mut [u8]` buffer passed to
/// those methods (the `zwo-rs` / `svbony-rs` caller-owned-buffer convention);
/// this carries only the dimensions the SDK reports alongside the download. The
/// number of valid bytes is the frame's own size — size the buffer with
/// [`Camera::get_image_size`](crate::Camera::get_image_size).
pub struct FrameInfo {
    /// the width of the image in pixels
    pub width: u32,
    /// the height of the image in pixels
    pub height: u32,
    /// the number of bits per pixel
    pub bits_per_pixel: u32,
    /// the number of channels: 1 (mono) or 3 (debayered colour)
    pub channels: u32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
/// this struct is used in `get_overscan_area`, `get_effective_area`, `set_roi` and `get_roi`
pub struct CCDChipArea {
    /// the x coordinate of the top left corner of the area
    pub start_x: u32,
    /// the y coordinate of the top left corner of the area
    pub start_y: u32,
    /// the width of the area in pixels
    pub width: u32,
    /// the height of the area in pixels
    pub height: u32,
}

/// Bayer colour-filter pattern, returned from `is_control_available` with
/// `ControlType::CamColor`.
///
/// The variant names and 1-based discriminants are the QHY SDK's own numbering
/// (`GBRG=1..RGGB=4`) — the sibling `zwo-rs`/`svbony-rs` crates expose the same
/// `BayerPattern` type with their SDKs' 0-based `Rg..Gb` variants (the names
/// differ because each mirrors its vendor SDK).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[expect(
    missing_docs,
    reason = "the variant names are the Bayer patterns themselves; a doc line per variant would restate them"
)]
pub enum BayerPattern {
    GBRG = 1,
    GRBG = 2,
    BGGR = 3,
    RGGB = 4,
}

impl From<BayerPattern> for u32 {
    /// The SDK code for a pattern. Total in this direction — every variant has
    /// one — which is why the discriminants are written out here rather than
    /// read back off the enum with a cast.
    fn from(pattern: BayerPattern) -> Self {
        match pattern {
            BayerPattern::GBRG => 1,
            BayerPattern::GRBG => 2,
            BayerPattern::BGGR => 3,
            BayerPattern::RGGB => 4,
        }
    }
}

impl TryFrom<u32> for BayerPattern {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::GBRG),
            2 => Ok(Self::GRBG),
            3 => Ok(Self::BGGR),
            4 => Ok(Self::RGGB),
            _ => Err(()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
/// used to store readout mode numbers and their descriptions coming from `get_readout_mode_name`
pub struct ReadoutMode {
    /// the number of the mode starting with 0
    pub id: u32,
    /// the name of the mode e.g., `"STANDARD MODE"`
    pub name: String,
}

#[derive(Debug, PartialEq, Eq)]
/// returned from `SDK::version`
pub struct SDKVersion {
    /// the year of the SDK version
    pub year: u32,
    /// the month of the SDK version
    pub month: u32,
    /// the day of the SDK version
    pub day: u32,
    /// the subday of the SDK version
    pub subday: u32,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // test code: don't count toward coverage
mod tests {
    use super::{BayerPattern, StreamMode};

    // `BayerPattern::try_from` is backend-independent pure logic; it was previously
    // only exercised by the deleted FFI-mock `camera_tests::bayer_mode_try_from`.
    #[test]
    fn bayer_pattern_try_from_maps_the_four_sdk_codes() {
        assert_eq!(BayerPattern::try_from(1), Ok(BayerPattern::GBRG));
        assert_eq!(BayerPattern::try_from(2), Ok(BayerPattern::GRBG));
        assert_eq!(BayerPattern::try_from(3), Ok(BayerPattern::BGGR));
        assert_eq!(BayerPattern::try_from(4), Ok(BayerPattern::RGGB));
    }

    #[test]
    fn bayer_pattern_converts_to_the_four_sdk_codes() {
        // Pins the codes in the outward direction too: with the discriminants
        // no longer read back off the enum, this is what keeps `From` and the
        // SDK numbering the doc comment promises from drifting apart.
        assert_eq!(u32::from(BayerPattern::GBRG), 1);
        assert_eq!(u32::from(BayerPattern::GRBG), 2);
        assert_eq!(u32::from(BayerPattern::BGGR), 3);
        assert_eq!(u32::from(BayerPattern::RGGB), 4);
    }

    #[test]
    fn bayer_pattern_try_from_rejects_codes_outside_1_to_4() {
        assert_eq!(BayerPattern::try_from(0), Err(()));
        assert_eq!(BayerPattern::try_from(5), Err(()));
    }

    // The sim backend never routes through the wire conversion (only the real
    // FFI arm does), so pin the SDK values here the way the `ControlType`
    // round-trip test pins `to_raw`.
    #[test]
    fn stream_mode_wire_values_match_the_sdk_numbering() {
        assert_eq!(u8::from(StreamMode::SingleFrameMode), 0);
        assert_eq!(u8::from(StreamMode::LiveMode), 1);
    }
}

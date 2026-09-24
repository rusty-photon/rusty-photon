//! The vendor-neutral half of the workspace's ASCOM camera drivers.
//!
//! `qhy-camera`, `zwo-camera` and `svbony-camera` drive three unrelated vendor
//! SDKs but present one ASCOM surface, so the rules that turn a client's
//! `StartX`/`NumX`/`BinX` into a frame the sensor can actually deliver are the
//! same rules written three times. They stayed the same rules until one copy
//! drifted: a sub-pixel ROI clamped to one pixel in `qhy-camera` and truncated
//! to zero in the other two, so a bin change could hand `StartExposure` a zero
//! the driver had invented and then blame the client for it. Neither the code
//! nor the tests showed it, because each driver curated its own case list — the
//! missing behaviour and its missing test hid each other.
//!
//! # What belongs here
//!
//! Two tests, both about the *driver* half rather than about dependencies:
//! nothing here implements ASCOM's `Camera` or `Device` traits or holds device
//! state, and no vendor SDK type appears in a signature. ASCOM Alpaca is the
//! workspace's lingua franca, so speaking its vocabulary — [`ImageArray`],
//! `ASCOMError` — is what lets a rule live here whole instead of arriving in a
//! private dialect each driver has to translate.
//!
//! [`BayerPattern`] shows where the line falls. Each SDK names the same four
//! mosaics differently (`ASI_BAYER_RG`, `QHY BayerPattern::RGGB`), so the
//! driver translates its own spelling; but *where the first red photosite sits*
//! is one ASCOM rule, so [`BayerPattern::offsets`] answers it once. What stays
//! in the drivers is what a vendor genuinely disagrees about: the `MaxADU`
//! ceiling, the readout formats, the exposure state machine.
//!
//! Where drivers differ by *degree* rather than in kind, the difference is a
//! parameter. All three sensors align their sub-frames, but not to the same
//! multiples — `zwo-camera` and `svbony-camera` want a binned width that is a
//! multiple of 8 and a height that is a multiple of 2, `qhy-camera` wants both
//! even — so [`check`] takes an [`Alignment`] rather than existing in three
//! versions that could disagree about anything else. `None` is still a rule a
//! driver may have: no alignment at all.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

use core::fmt;
use core::num::{NonZeroU128, NonZeroU32, NonZeroU64};
use core::time::Duration;

use ascom_alpaca::api::camera::ImageArray;
use ascom_alpaca::ASCOMError;
use ndarray::Array2;

/// A region of interest in *binned* pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Roi {
    /// Left edge, binned pixels from the sensor's origin.
    pub start_x: u32,
    /// Top edge, binned pixels from the sensor's origin.
    pub start_y: u32,
    /// Width in binned pixels (ASCOM `NumX`).
    pub width: u32,
    /// Height in binned pixels (ASCOM `NumY`).
    pub height: u32,
}

/// A region of interest in *unbinned* sensor pixels: the region the client
/// asked for, independent of the bin it was asked at.
///
/// ASCOM's `StartX`/`StartY`/`NumX`/`NumY` are **binned** members, so a driver
/// converts on the way in and on the way out — [`unbinned`] in a setter,
/// [`Self::binned`] in a getter and before it arms the SDK. Holding the
/// unbinned region is what makes a bin change non-destructive: it rewrites
/// nothing, it only changes the divisor.
///
/// Scaling the *previous binned value* instead — the rule this type replaces —
/// truncated once per step and compounded, because every step started from the
/// last step's already-truncated result. Measured on a QHY600M: a 100x100
/// sub-frame at (200,200) walked 1 → 3 → 4 → 1 came back 96x96 at (196,196),
/// four pixels short in both extent and origin, and stayed that way until the
/// client wrote the ROI again or reconnected.
///
/// `u64` rather than `u32` because a setter multiplies: a client may set any
/// `u32` at any bin, and reading it back at the bin it was set at has to give
/// that value rather than a ceiling the driver imposed on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnbinnedRoi {
    /// Left edge, unbinned pixels from the sensor's origin.
    pub start_x: u64,
    /// Top edge, unbinned pixels from the sensor's origin.
    pub start_y: u64,
    /// Width in unbinned pixels.
    pub width: u64,
    /// Height in unbinned pixels.
    pub height: u64,
}

impl UnbinnedRoi {
    /// The whole of a `width` x `height` unbinned sensor at its origin — the
    /// ROI ASCOM says a fresh connection reports.
    #[must_use]
    pub fn full_frame(width: u32, height: u32) -> Self {
        Self {
            start_x: 0,
            start_y: 0,
            width: u64::from(width),
            height: u64::from(height),
        }
    }

    /// This region seen at `bin`, in the binned coordinates ASCOM's members
    /// speak.
    ///
    /// Only the *view* truncates; the region itself is untouched, so every
    /// `binned` call derives from the same source and a round trip through any
    /// sequence of bins returns the client's own frame.
    ///
    /// The ROI members are set independently, so only their combination can be
    /// validated and it is — at `StartExposure`, by [`check`]. Whatever the
    /// client last set therefore arrives here, and this derivation must not
    /// change *which value* the eventual error is about:
    ///
    /// - A **sub-pixel** extent is clamped to 1. Truncating it to 0 would make
    ///   `StartExposure` reject a zero this method invented rather than the
    ///   client's own `NumX`.
    /// - A **client-set 0** is preserved, so it still earns the
    ///   [`ZeroExtent`](GeometryError::ZeroExtent) it was heading for. Clamping
    ///   it to 1 would clear that check and move the complaint onto a value
    ///   nobody set — or, on a driver with no alignment rule, expose a
    ///   one-pixel frame in place of the error the client had earned.
    ///
    /// A zero `bin` is a bin no handshake has published yet, which is not a
    /// scale: it divides by one.
    ///
    /// ```
    /// use rusty_photon_camera_core::{unbinned, Roi, UnbinnedRoi};
    ///
    /// let roi = UnbinnedRoi { start_x: 200, start_y: 200, width: 100, height: 100 };
    /// assert_eq!(roi.binned(3), Roi { start_x: 66, start_y: 66, width: 33, height: 33 });
    /// assert_eq!(roi.binned(4), Roi { start_x: 50, start_y: 50, width: 25, height: 25 });
    /// // The walk that used to lose four pixels: the source never moved.
    /// assert_eq!(roi.binned(1), Roi { start_x: 200, start_y: 200, width: 100, height: 100 });
    ///
    /// // A sub-pixel extent survives as one pixel, not as a zero the client never set.
    /// assert_eq!(UnbinnedRoi { width: 1, ..roi }.binned(4).width, 1);
    /// // A client-set zero stays the zero it earned.
    /// assert_eq!(UnbinnedRoi { width: 0, ..roi }.binned(4).width, 0);
    /// // Any u32 set at a bin reads back at that bin exactly.
    /// assert_eq!(UnbinnedRoi { width: unbinned(u32::MAX, 4), ..roi }.binned(4).width, u32::MAX);
    /// ```
    #[must_use]
    pub fn binned(self, bin: u8) -> Roi {
        // `NonZeroU64::MIN` is 1: a bin nobody has published is not a scale.
        let bin = NonZeroU64::new(u64::from(bin)).unwrap_or(NonZeroU64::MIN);
        let fit = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        let offset = |v: u64| fit(v / bin);
        let extent = |v: u64| if v == 0 { 0 } else { fit((v / bin).max(1)) };
        Roi {
            start_x: offset(self.start_x),
            start_y: offset(self.start_y),
            width: extent(self.width),
            height: extent(self.height),
        }
    }
}

/// The unbinned value a client's **binned** `value` denotes at `bin` — what a
/// ROI setter stores.
///
/// A zero `bin` is a bin no handshake has published yet, which is not a scale:
/// the value is already its own unbinned self.
#[must_use]
pub fn unbinned(value: u32, bin: u8) -> u64 {
    u64::from(value).saturating_mul(u64::from(bin.max(1)))
}

/// A Bayer mosaic, named — as every vendor SDK names it — by the colours of its
/// top-left 2x2 photosite quad read row-major.
///
/// Four variants because a Bayer quad has four arrangements and no more: red
/// takes one corner, blue the opposite one, and the greens the other diagonal.
/// A driver maps its SDK's spelling onto these; the ASCOM offsets then come
/// from [`offsets`](Self::offsets) rather than from a table per driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BayerPattern {
    /// `R G` over `G B`.
    Rggb,
    /// `B G` over `G R`.
    Bggr,
    /// `G R` over `B G`.
    Grbg,
    /// `G B` over `R G`.
    Gbrg,
}

impl BayerPattern {
    /// ASCOM `BayerOffsetX` and `BayerOffsetY`: the column and row of the first
    /// red photosite within the top-left quad.
    ///
    /// ```
    /// use rusty_photon_camera_core::BayerPattern;
    ///
    /// // `GRBG` reads `G R` over `B G`, so red is one across and none down.
    /// assert_eq!(BayerPattern::Grbg.offsets(), (1, 0));
    /// assert_eq!(BayerPattern::Rggb.offsets(), (0, 0));
    /// ```
    #[must_use]
    pub const fn offsets(self) -> (u8, u8) {
        match self {
            Self::Rggb => (0, 0),
            Self::Grbg => (1, 0),
            Self::Gbrg => (0, 1),
            Self::Bggr => (1, 1),
        }
    }
}

/// A sensor's sub-frame alignment rule: the binned extent must be a multiple of
/// these, or the SDK rejects the ROI.
///
/// The multiples are [`NonZeroU32`] because a zero one has no honest reading —
/// "every extent is a multiple of 0" is false, and treating it as "no rule"
/// would let a typo turn this into a weaker validator without saying so. The
/// absence of a rule is already spelled `None` where an `Alignment` is asked
/// for, so the type has no second way to mean it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    /// `NumX` must be a multiple of this.
    pub width: NonZeroU32,
    /// `NumY` must be a multiple of this.
    pub height: NonZeroU32,
}

impl Alignment {
    /// The rule requiring `width`-pixel and `height`-pixel multiples.
    #[must_use]
    pub const fn new(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self { width, height }
    }
}

/// Why a ROI is not a geometry the sensor can be asked for.
///
/// The `Display` text is what the driver hands to ASCOM as an `INVALID_VALUE`
/// message, so it names ASCOM members rather than this crate's field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryError {
    /// `NumX` or `NumY` is zero, so there is no frame to expose.
    ZeroExtent,
    /// The bin is zero, so the sensor extent it divides has no value.
    ZeroBin,
    /// The extent violates the sensor's sub-frame alignment rule.
    Misaligned(Alignment),
    /// `StartX + NumX` runs past the binned sensor width.
    OutOfBoundsX,
    /// `StartY + NumY` runs past the binned sensor height.
    OutOfBoundsY,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ZeroExtent => f.write_str("NumX and NumY must be greater than 0"),
            Self::ZeroBin => f.write_str("BinX and BinY must be greater than 0"),
            Self::Misaligned(align) => write!(
                f,
                "NumX must be a multiple of {} and NumY a multiple of {}",
                align.width, align.height
            ),
            Self::OutOfBoundsX => f.write_str("StartX + NumX exceeds CameraXSize / BinX"),
            Self::OutOfBoundsY => f.write_str("StartY + NumY exceeds CameraYSize / BinY"),
        }
    }
}

impl core::error::Error for GeometryError {}

impl From<GeometryError> for ASCOMError {
    /// Every geometry failure is the client having asked for a frame the sensor
    /// cannot deliver, so they share one ASCOM code. Choosing it here rather
    /// than at each driver's call site is the point: three `map_err`s were
    /// three chances to disagree about what a bad ROI is.
    fn from(error: GeometryError) -> Self {
        Self::invalid_value(error)
    }
}

/// Validate a ROI against the sensor at a given bin, with an optional
/// alignment rule.
///
/// `sensor_w`/`sensor_h` are the *reported* full-frame extent — for a driver
/// that aligns what it reports (see [`aligned_sensor_extent`]) that is the
/// aligned value, not the raw sensor, so the bound and the alignment rule
/// agree with each other.
///
/// # Order is the contract
///
/// Rules are applied cheapest-precondition-first — zero extent, zero bin,
/// alignment, then bounds — and the order is pinned by tests rather than left
/// to whichever check happens to come first in the source. It decides which
/// value a client is told about when a ROI breaks more than one rule at once,
/// and the answer has to be the one that explains the rest: a zero bin is not a
/// geometry that fails a rule but one with no rule to apply, so it is reported
/// ahead of an alignment complaint that a client could otherwise chase while
/// the real problem sat in `BinX`.
///
/// # Errors
///
/// Returns the first [`GeometryError`] the ROI trips, in that order.
///
/// ```
/// use core::num::NonZeroU32;
/// use rusty_photon_camera_core::{check, Alignment, GeometryError, Roi};
///
/// let eight = NonZeroU32::new(8).unwrap();
/// let two = NonZeroU32::new(2).unwrap();
/// let asi = Some(Alignment::new(eight, two));
/// let roi = Roi { start_x: 0, start_y: 0, width: 64, height: 48 };
/// check(roi, 6240, 4176, 1, asi).unwrap();
///
/// // A ROI that is both misaligned and at a zero bin hears about the bin.
/// let misaligned = Roi { width: 100, ..roi };
/// assert_eq!(check(misaligned, 6240, 4176, 0, asi), Err(GeometryError::ZeroBin));
/// assert_eq!(
///     check(misaligned, 6240, 4176, 1, asi),
///     Err(GeometryError::Misaligned(Alignment::new(eight, two)))
/// );
/// ```
pub fn check(
    roi: Roi,
    sensor_w: u32,
    sensor_h: u32,
    bin: u32,
    align: Option<Alignment>,
) -> Result<(), GeometryError> {
    if roi.width == 0 || roi.height == 0 {
        return Err(GeometryError::ZeroExtent);
    }
    // The bin divides the sensor extent, so rejecting a zero here is what makes
    // this validator total — the alternative leaves a division for every caller
    // to prove safe.
    let (Some(max_x), Some(max_y)) = (sensor_w.checked_div(bin), sensor_h.checked_div(bin)) else {
        return Err(GeometryError::ZeroBin);
    };
    if let Some(align) = align {
        // `Rem<NonZeroU32>` is total, so the rule needs no guard of its own.
        if roi.width % align.width != 0 || roi.height % align.height != 0 {
            return Err(GeometryError::Misaligned(align));
        }
    }
    if roi.start_x.saturating_add(roi.width) > max_x {
        return Err(GeometryError::OutOfBoundsX);
    }
    if roi.start_y.saturating_add(roi.height) > max_y {
        return Err(GeometryError::OutOfBoundsY);
    }
    Ok(())
}

/// The largest extent at or below `max` such that the full frame divided by
/// *every* supported bin is still a valid ROI — i.e. the binned extent is a
/// multiple of `unit` (8 for width, 2 for height on both ASI and `SVBony`).
///
/// `ConformU`, and clients generally, take a full frame at each bin via
/// `NumX = CameraXSize / bin`. Reporting the raw sensor size makes those binned
/// full frames unachievable wherever `raw / bin` misses the alignment rule, and
/// this was found by `ConformU` on real hardware twice: an ASI2600's 6248 / 2 is
/// 3124, not a multiple of 8, and an SV605CC's 3008 / 3 is 1002, likewise. So a
/// driver reports the largest multiple of `lcm(unit · bin)` that fits, giving up
/// a few edge columns at full resolution to make every binned full frame exactly
/// achievable.
///
/// Degenerate inputs return `max` unchanged rather than reducing it: an empty
/// bin list, a bin list of zeroes, or a step that already exceeds the sensor.
///
/// Private, and reached only through [`aligned_sensor`]: a caller that picks the
/// `unit` per axis by hand can pass one the ROI check does not use, or swap the
/// two axes' multiples, and get a driver that validates against one rule while
/// reporting a sensor sized for another.
fn aligned_sensor_extent(max: u32, supported_bins: &[u32], unit: u32) -> u32 {
    let step = supported_bins
        .iter()
        .copied()
        .filter(|&b| b > 0)
        .map(|b| unit.saturating_mul(b))
        .fold(1, lcm);
    let Some(step) = NonZeroU32::new(step).filter(|s| s.get() <= max) else {
        return max;
    };
    // A remainder is never larger than what it came from, so this cannot wrap.
    max.saturating_sub(max % step)
}

/// Both sensor extents, reduced so a full frame at every supported bin is still
/// a valid ROI under `align` — the `CameraXSize`/`CameraYSize` a driver reports.
///
/// Taking the whole [`Alignment`] rather than a per-axis multiple is what holds
/// this in step with [`check`]. The multiple an extent is aligned *to* and the
/// multiple a ROI is validated *against* are then the same value by
/// construction, and the width and height multiples cannot be swapped between
/// the axes on the way in. Reporting a sensor sized for one rule while checking
/// ROIs against another makes the binned full frame unachievable, which is the
/// exact failure `ConformU` caught on hardware.
///
/// `None` is no rule, so there is nothing to align to and both extents pass
/// through. No driver in this workspace passes it today — every sensor here
/// aligns its sub-frames to something — but a `None` that silently aligned
/// anyway would be a validator disagreeing with what it reports.
///
/// ```
/// use core::num::NonZeroU32;
/// use rusty_photon_camera_core::{aligned_sensor, Alignment};
///
/// let eight = NonZeroU32::new(8).unwrap();
/// let two = NonZeroU32::new(2).unwrap();
/// let asi = Some(Alignment::new(eight, two));
///
/// // ASI2600: 6248 reduces to 6240, so 6240/{1,2,3,4} are all multiples of 8;
/// // 4176 is already aligned for the height rule.
/// assert_eq!(aligned_sensor(6248, 4176, &[1, 2, 3, 4], asi), (6240, 4176));
/// // No rule, nothing to align to.
/// assert_eq!(aligned_sensor(6248, 4176, &[1, 2, 3, 4], None), (6248, 4176));
/// ```
#[must_use]
pub fn aligned_sensor(
    max_width: u32,
    max_height: u32,
    supported_bins: &[u32],
    align: Option<Alignment>,
) -> (u32, u32) {
    let Some(align) = align else {
        return (max_width, max_height);
    };
    (
        aligned_sensor_extent(max_width, supported_bins, align.width.get()),
        aligned_sensor_extent(max_height, supported_bins, align.height.get()),
    )
}

/// Greatest common divisor (Euclid) of two non-zero operands. The result is
/// itself non-zero, since it divides both — which is what lets [`lcm`] divide
/// by it without a second check.
fn gcd(a: NonZeroU32, b: NonZeroU32) -> NonZeroU32 {
    let (mut a, mut b) = (a, b);
    // `a % b` is total once `b` carries its own non-zero-ness, so the loop
    // needs no separate guard on the divisor.
    while let Some(rem) = NonZeroU32::new(a.get() % b) {
        a = b;
        b = rem;
    }
    b
}

/// Least common multiple (`0` if either operand is `0`).
fn lcm(a: u32, b: u32) -> u32 {
    let (Some(a_nz), Some(b_nz)) = (NonZeroU32::new(a), NonZeroU32::new(b)) else {
        // Zero shares no multiple with anything.
        return 0;
    };
    // The gcd divides `a` exactly, so dividing before multiplying keeps the
    // intermediate as small as it can be; saturating covers a pair whose
    // multiple genuinely does not fit.
    (a / gcd(a_nz, b_nz)).saturating_mul(b)
}

/// How far through an exposure, as ASCOM's `PercentCompleted`, **capped at 99**.
///
/// 100 is the ready state's answer, so an exposure still in flight must never
/// report it however close it is — a client polling for completion would
/// otherwise read 100 and go looking for an image that is not there yet.
///
/// A `total` under a microsecond has no ratio to report. The exposure is
/// effectively instantaneous rather than unstarted, so it reads 99: a 0 there
/// would claim no progress on something already finishing. A driver that cannot
/// tell an unrecorded duration from a sub-microsecond one should answer that
/// question before calling this, where it still has the state to answer it with.
///
/// ```
/// use core::time::Duration;
/// use rusty_photon_camera_core::progress_percent;
///
/// let total = Duration::from_secs(10);
/// assert_eq!(progress_percent(Duration::from_secs(3), total), 30);
/// // Never 100 while in flight, however close.
/// assert_eq!(progress_percent(total, total), 99);
/// assert_eq!(progress_percent(Duration::from_secs(999), total), 99);
/// ```
#[must_use]
pub fn progress_percent(done: Duration, total: Duration) -> u8 {
    let Some(total_us) = NonZeroU128::new(total.as_micros()) else {
        return 99;
    };
    // Integer throughout, so there is no float to round: the ratio is capped
    // before it is narrowed, which gives the conversion an answer for every
    // input, and the fallback is that same cap.
    let pct = done.as_micros().saturating_mul(100) / total_us;
    u8::try_from(pct.min(99)).unwrap_or(99)
}

/// How a downloaded frame stores each pixel.
///
/// The two single-plane depths every supported SDK delivers. Which of its own
/// formats maps onto which of these is the driver's question — `Raw8`/`Raw16`,
/// a bit count, a channel count — and so is which formats are unpackable at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelDepth {
    /// One byte per pixel.
    Eight,
    /// Two bytes per pixel, low byte first.
    Sixteen,
}

impl PixelDepth {
    /// Bytes one pixel occupies in a downloaded buffer.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Eight => 1,
            Self::Sixteen => 2,
        }
    }
}

/// Why a downloaded frame could not become an [`ImageArray`].
///
/// The `Display` text carries no format name: the driver knows which of its
/// own formats it asked for and prefixes it, so the message reads the same as
/// when each driver owned the whole unpack.
#[derive(Debug)]
pub enum FrameError {
    /// The buffer holds fewer bytes than `width × height × depth`.
    TooSmall,
    /// `ndarray` rejected the frame's shape.
    Shape(ndarray::ShapeError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall => f.write_str("buffer too small for frame"),
            Self::Shape(error) => write!(f, "frame shape rejected: {error}"),
        }
    }
}

impl core::error::Error for FrameError {}

/// Unpack a single-plane frame into an ASCOM [`ImageArray`] with `[x][y]` axis
/// order (ASCOM stores width-major).
///
/// Takes the buffer **by value** so the 8-bit path can hand it straight to
/// `Array2` rather than copying a frame the capture path already owns — at full
/// frame that copy is the frame itself. 16-bit still pays one, since its bytes
/// have to be re-read as `u16`, and `ImageArray` widens whatever it gets to
/// `i32` internally — so this saves an intermediate, not the dominant
/// allocation.
///
/// ```
/// use rusty_photon_camera_core::{to_image_array, PixelDepth};
///
/// // Two pixels, low byte first on the wire.
/// let frame = to_image_array(vec![0x34, 0x12, 0x78, 0x56], 2, 1, PixelDepth::Sixteen).unwrap();
/// assert_eq!(frame[(0, 0, 0)], 0x1234_i32);
/// assert_eq!(frame[(1, 0, 0)], 0x5678_i32);
/// ```
///
/// # Errors
///
/// Returns [`FrameError::TooSmall`] if `bytes` holds fewer bytes than
/// `width × height × depth` needs, and [`FrameError::Shape`] if
/// `ndarray` rejects the frame's shape.
pub fn to_image_array(
    mut bytes: Vec<u8>,
    width: u32,
    height: u32,
    depth: PixelDepth,
) -> Result<ImageArray, FrameError> {
    // The SDKs report geometry as `u32`; here it is the length of `bytes` and
    // the shape of the array, so convert once. A frame too large to address
    // saturates, which lands it in the same "buffer too small" answer as any
    // other short read rather than wrapping into a length the buffer appears to
    // satisfy. (Saturate what you compare — this length is only ever compared,
    // never allocated from.)
    let w = usize::try_from(width).unwrap_or(usize::MAX);
    let h = usize::try_from(height).unwrap_or(usize::MAX);
    let needed = w.saturating_mul(h).saturating_mul(depth.bytes_per_pixel());
    // Each arm builds the frame row-major in its own element type, then
    // reverses the axes for ASCOM's width-major order.
    match depth {
        PixelDepth::Eight => {
            // `from_shape_vec` demands an exact length, and `truncate` only
            // trims a caller's slack — it cannot grow a short buffer — so the
            // shortfall has to be rejected before it.
            if bytes.len() < needed {
                return Err(FrameError::TooSmall);
            }
            bytes.truncate(needed);
            let arr = Array2::from_shape_vec((h, w), bytes).map_err(FrameError::Shape)?;
            Ok(ImageArray::from(arr.reversed_axes()))
        }
        PixelDepth::Sixteen => {
            // The length check and the slice are one question, so `get` asks it
            // once: there is no separate test left to keep in step with the
            // bound it guards.
            let Some(frame) = bytes.get(..needed) else {
                return Err(FrameError::TooSmall);
            };
            // Low byte first on the wire, which `from_le_bytes` says directly.
            let pixels: Vec<u16> = frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let arr = Array2::from_shape_vec((h, w), pixels).map_err(FrameError::Shape)?;
            Ok(ImageArray::from(arr.reversed_axes()))
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A zero here would not compile, which is the point — `NonZeroU32::new(0)`
    /// is `None`, and `expect` on `None` in a `const` is a build error, not a
    /// runtime surprise.
    const EIGHT: NonZeroU32 = NonZeroU32::new(8).expect("8 is not zero");
    const TWO: NonZeroU32 = NonZeroU32::new(2).expect("2 is not zero");
    const ASI: Option<Alignment> = Some(Alignment::new(EIGHT, TWO));

    fn roi_at(start_x: u32, start_y: u32, width: u32, height: u32) -> Roi {
        Roi {
            start_x,
            start_y,
            width,
            height,
        }
    }

    // --- check ---------------------------------------------------------------

    #[test]
    fn zero_extent_is_rejected_on_either_axis() {
        assert_eq!(
            check(roi_at(0, 0, 0, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::ZeroExtent)
        );
        assert_eq!(
            check(roi_at(0, 0, 64, 0), 6240, 4176, 1, ASI),
            Err(GeometryError::ZeroExtent)
        );
    }

    #[test]
    fn a_zero_bin_is_reported_ahead_of_the_alignment_rule() {
        // Both rules are broken. The bin wins, because nothing — including the
        // bound the alignment rule is measured against — can be computed
        // without one.
        assert_eq!(
            check(roi_at(0, 0, 100, 64), 6240, 4176, 0, ASI),
            Err(GeometryError::ZeroBin)
        );
        // ...and with no alignment rule in play it is still the bin, not a
        // bounds complaint against a sensor extent that was never divided.
        assert_eq!(
            check(roi_at(0, 0, 100, 64), 6240, 4176, 0, None),
            Err(GeometryError::ZeroBin)
        );
    }

    #[test]
    fn a_zero_extent_is_reported_ahead_of_a_zero_bin() {
        // A client that set neither gets told about `NumX` first, matching the
        // order the ASCOM members are conventionally set in.
        assert_eq!(
            check(roi_at(0, 0, 0, 0), 6240, 4176, 0, ASI),
            Err(GeometryError::ZeroExtent)
        );
    }

    #[test]
    fn misalignment_is_reported_ahead_of_a_bounds_failure() {
        // 8000 is both misaligned and past the sensor. The alignment rule is
        // the local, fixable complaint; being told the frame is too big invites
        // shrinking it to another misaligned value.
        assert_eq!(
            check(roi_at(0, 0, 8001, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::Misaligned(Alignment::new(EIGHT, TWO)))
        );
    }

    #[test]
    fn alignment_applies_per_axis() {
        assert_eq!(
            check(roi_at(0, 0, 100, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::Misaligned(Alignment::new(EIGHT, TWO)))
        );
        assert_eq!(
            check(roi_at(0, 0, 64, 47), 6240, 4176, 1, ASI),
            Err(GeometryError::Misaligned(Alignment::new(EIGHT, TWO)))
        );
        // No rule, same ROI, no complaint: an unaligned driver is checked
        // against bounds alone.
        check(roi_at(0, 0, 100, 47), 6240, 4176, 1, None).unwrap();
    }

    #[test]
    fn bounds_are_measured_against_the_binned_extent() {
        check(roi_at(0, 0, 6240, 4176), 6240, 4176, 1, ASI).unwrap();
        check(roi_at(0, 0, 3120, 2088), 6240, 4176, 2, ASI).unwrap();
        // The same frame that fits at bin 1 does not at bin 2.
        assert_eq!(
            check(roi_at(0, 0, 4000, 64), 6240, 4176, 2, ASI),
            Err(GeometryError::OutOfBoundsX)
        );
    }

    #[test]
    fn bounds_name_the_axis_that_failed() {
        assert_eq!(
            check(roi_at(0, 0, 8000, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::OutOfBoundsX)
        );
        assert_eq!(
            check(roi_at(0, 0, 64, 6000), 6240, 4176, 1, ASI),
            Err(GeometryError::OutOfBoundsY)
        );
        // A start offset counts toward the bound, not just the extent.
        assert_eq!(
            check(roi_at(6240, 0, 64, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::OutOfBoundsX)
        );
        assert_eq!(
            check(roi_at(0, 4176, 64, 64), 6240, 4176, 1, ASI),
            Err(GeometryError::OutOfBoundsY)
        );
    }

    #[test]
    fn a_start_offset_near_u32_max_does_not_wrap_into_bounds() {
        // `start + width` is saturating, so an offset that would wrap reports
        // out-of-bounds rather than passing as a small sum.
        assert_eq!(
            check(roi_at(u32::MAX, 0, 8, 2), 6240, 4176, 1, ASI),
            Err(GeometryError::OutOfBoundsX)
        );
    }

    #[test]
    fn messages_name_ascom_members() {
        assert_eq!(
            GeometryError::ZeroExtent.to_string(),
            "NumX and NumY must be greater than 0"
        );
        assert_eq!(
            GeometryError::ZeroBin.to_string(),
            "BinX and BinY must be greater than 0"
        );
        assert_eq!(
            GeometryError::Misaligned(Alignment::new(EIGHT, TWO)).to_string(),
            "NumX must be a multiple of 8 and NumY a multiple of 2"
        );
        assert_eq!(
            GeometryError::OutOfBoundsX.to_string(),
            "StartX + NumX exceeds CameraXSize / BinX"
        );
        assert_eq!(
            GeometryError::OutOfBoundsY.to_string(),
            "StartY + NumY exceeds CameraYSize / BinY"
        );
    }

    // --- UnbinnedRoi ---------------------------------------------------------

    const fn unbinned_at(start_x: u64, start_y: u64, width: u64, height: u64) -> UnbinnedRoi {
        UnbinnedRoi {
            start_x,
            start_y,
            width,
            height,
        }
    }

    #[test]
    fn the_binned_view_is_the_region_divided_by_the_bin() {
        // #1194's arithmetic half. Every view derives from the same untouched
        // source, so walking the bins and coming back returns the client's own
        // frame. Scaling the *previous binned value* instead agreed at bin 3
        // and then diverged — 24x24 at (49,49) at bin 4, and 96x96 at
        // (196,196) back at bin 1, both measured on a QHY600M.
        //
        // Each bin is asserted rather than looped over: `binned` takes `self`
        // by value and is pure, so a loop that discarded its results would
        // pass just as well with the chaining restored. Chaining lived in the
        // drivers' `set_bin_x`, so the regression itself is caught by their
        // BDD round-trip scenarios, not here.
        let roi = unbinned_at(200, 200, 100, 100);
        assert_eq!(roi.binned(1), roi_at(200, 200, 100, 100));
        assert_eq!(roi.binned(2), roi_at(100, 100, 50, 50));
        assert_eq!(roi.binned(3), roi_at(66, 66, 33, 33));
        assert_eq!(roi.binned(4), roi_at(50, 50, 25, 25));
        // ...and back, from that same source rather than from the bin-4 view.
        assert_eq!(roi.binned(1), roi_at(200, 200, 100, 100));
    }

    #[test]
    fn a_sub_pixel_extent_is_one_binned_pixel() {
        // 1 / 4 truncates to 0, which `check` would then reject as a zero the
        // client never set.
        let view = unbinned_at(0, 0, 1, 1).binned(4);
        assert_eq!(view.width, 1);
        assert_eq!(view.height, 1);
    }

    #[test]
    fn a_client_set_zero_extent_stays_zero() {
        // The zero is the client's, and it has a `ZeroExtent` coming. Clamping
        // it to 1 would clear that check and complain about something else.
        let view = unbinned_at(0, 0, 0, 0).binned(4);
        assert_eq!(view.width, 0);
        assert_eq!(view.height, 0);
    }

    #[test]
    fn the_origin_is_a_coordinate_not_an_extent() {
        // A sub-pixel *offset* truncates to 0; the minimum-of-one rule is for
        // extents only, and moving an origin off zero would relocate the frame.
        assert_eq!(unbinned_at(1, 1, 64, 64).binned(4).start_x, 0);
    }

    #[test]
    fn an_unpublished_bin_divides_by_one() {
        // Zero is the "no handshake has published a bin yet" sentinel, not a
        // scale; dividing by it would panic and inventing one would lie.
        let roi = unbinned_at(10, 20, 64, 48);
        assert_eq!(roi.binned(0), roi.binned(1));
    }

    #[test]
    fn any_u32_set_at_a_bin_reads_back_at_that_bin_exactly() {
        // Why the store is `u64`: a `u32` store would saturate on the way in
        // and `StartExposure` would then complain about a number the client
        // never set.
        for bin in [1, 2, 3, 4, 8, u8::MAX] {
            let roi = UnbinnedRoi {
                start_x: unbinned(u32::MAX, bin),
                start_y: unbinned(12_345, bin),
                width: unbinned(u32::MAX, bin),
                height: unbinned(101, bin),
            };
            assert_eq!(roi.binned(bin), roi_at(u32::MAX, 12_345, u32::MAX, 101));
        }
    }

    #[test]
    fn a_full_frame_is_the_whole_sensor_at_the_origin() {
        assert_eq!(
            UnbinnedRoi::full_frame(9576, 6384).binned(1),
            roi_at(0, 0, 9576, 6384)
        );
        assert_eq!(
            UnbinnedRoi::full_frame(9576, 6384).binned(3),
            roi_at(0, 0, 3192, 2128)
        );
    }

    // --- aligned_sensor_extent -----------------------------------------------

    #[test]
    fn alignment_makes_every_binned_full_frame_valid_on_the_asi2600() {
        // 6248 reduces to 6240 so 6240/{1,2,3,4} are all multiples of 8; 4176
        // is already aligned. Found by ConformU on real hardware.
        assert_eq!(aligned_sensor_extent(6248, &[1, 2, 3, 4], 8), 6240);
        assert_eq!(aligned_sensor_extent(4176, &[1, 2, 3, 4], 2), 4176);
        for bin in [1_u32, 2, 3, 4] {
            assert_eq!((6240 / bin) % 8, 0, "width / {bin} not a multiple of 8");
            assert_eq!((4176 / bin) % 2, 0, "height / {bin} not even");
        }
    }

    #[test]
    fn alignment_makes_every_binned_full_frame_valid_on_the_sv605cc() {
        // Width step lcm(8,16,24,32) = 96, height step lcm(2,4,6,8) = 24. Also
        // found by ConformU on real hardware — the same lesson, a second time,
        // which is why it is one function now.
        assert_eq!(aligned_sensor_extent(3008, &[1, 2, 3, 4], 8), 2976);
        assert_eq!(aligned_sensor_extent(3008, &[1, 2, 3, 4], 2), 3000);
        for bin in [1_u32, 2, 3, 4] {
            assert_eq!((2976 / bin) % 8, 0, "width at bin {bin}");
            assert_eq!((3000 / bin) % 2, 0, "height at bin {bin}");
        }
    }

    #[test]
    fn both_axes_take_their_own_multiple_from_one_alignment() {
        // The SV605CC is square, so the pair is only right if each axis used its
        // own multiple: 3008 aligns to 2976 under the width rule and to 3000
        // under the height rule. Passing the units by hand is where those could
        // be swapped; there is no longer a way to.
        assert_eq!(aligned_sensor(3008, 3008, &[1, 2, 3, 4], ASI), (2976, 3000));
        // No rule, nothing to align to.
        assert_eq!(
            aligned_sensor(3008, 3008, &[1, 2, 3, 4], None),
            (3008, 3008)
        );
    }

    #[test]
    fn alignment_returns_the_raw_extent_for_degenerate_inputs() {
        // Nothing to align against.
        assert_eq!(aligned_sensor_extent(3008, &[], 8), 3008);
        assert_eq!(aligned_sensor_extent(100, &[0], 8), 100);
        // A step larger than the sensor would reduce it to zero; the raw
        // extent is the better answer.
        assert_eq!(aligned_sensor_extent(10, &[1, 2, 3, 4], 8), 10);
    }

    #[test]
    fn lcm_treats_zero_as_sharing_no_multiple() {
        assert_eq!(lcm(0, 5), 0);
        assert_eq!(lcm(5, 0), 0);
        assert_eq!(lcm(8, 12), 24);
        // Dividing by the gcd first keeps the intermediate small; saturating
        // covers a pair whose multiple genuinely does not fit.
        assert_eq!(lcm(u32::MAX, u32::MAX - 1), u32::MAX);
    }

    // --- BayerPattern --------------------------------------------------------

    /// Derived from each variant's *name* rather than restating the table
    /// `offsets` already holds: the name is the quad read row-major, so the
    /// red photosite at index `i` sits at `(i % 2, i / 2)`. A restatement
    /// would pass just as happily with x and y transposed, and on a square
    /// sensor nothing downstream would notice.
    #[test]
    fn offsets_locate_the_first_red_photosite_of_the_named_quad() {
        for (pattern, name) in [
            (BayerPattern::Rggb, "RGGB"),
            (BayerPattern::Bggr, "BGGR"),
            (BayerPattern::Grbg, "GRBG"),
            (BayerPattern::Gbrg, "GBRG"),
        ] {
            let red = name.find('R').expect("a Bayer quad has a red photosite");
            let expected = (
                u8::try_from(red % 2).expect("a quad index is below 4"),
                u8::try_from(red / 2).expect("a quad index is below 4"),
            );
            assert_eq!(pattern.offsets(), expected, "{name}");
        }
    }

    // --- to_image_array ------------------------------------------------------

    #[test]
    fn frames_are_width_major_at_both_depths() {
        for (depth, len) in [
            (PixelDepth::Eight, 64 * 48),
            (PixelDepth::Sixteen, 64 * 48 * 2),
        ] {
            let array = to_image_array(vec![0u8; len], 64, 48, depth).unwrap();
            // ASCOM `[x][y]`: the first axis is width.
            assert_eq!((array.dim().0, array.dim().1), (64, 48), "{depth:?}");
        }
    }

    /// Pins the wire contract: `34 12` is 0x1234, low byte first. Catches an
    /// outright swap (`from_be_bytes`, or hand-rolled shifts). It cannot catch
    /// a regression to `from_ne_bytes` — the workspace is little-endian-only by
    /// compile gate, so the two are the same instruction everywhere it builds.
    #[test]
    fn sixteen_bit_pixels_are_read_low_byte_first() {
        let mut bytes = vec![0u8; 64 * 48 * 2];
        bytes[0] = 0x34;
        bytes[1] = 0x12;
        let array = to_image_array(bytes, 64, 48, PixelDepth::Sixteen).unwrap();
        assert_eq!(array[(0, 0, 0)], 0x1234_i32);
    }

    /// The 8-bit path reads one byte per pixel. Before the drivers grew this
    /// arm, an 8-bit frame was rejected as "buffer too small" because the
    /// transform assumed 16-bit throughout.
    #[test]
    fn eight_bit_pixels_are_one_byte_each() {
        let bytes: Vec<u8> = (0..64 * 48)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let expected = i32::from(bytes[2 * 64 + 3]);
        let array = to_image_array(bytes, 64, 48, PixelDepth::Eight).unwrap();
        // Row 2, column 3 row-major becomes `[x=3][y=2]` width-major.
        assert_eq!(array[(3, 2, 0)], expected);
    }

    #[test]
    fn a_short_buffer_is_rejected_at_both_depths() {
        for depth in [PixelDepth::Eight, PixelDepth::Sixteen] {
            let error = to_image_array(vec![0u8; 8], 64, 48, depth).unwrap_err();
            assert!(matches!(error, FrameError::TooSmall), "{depth:?}: {error}");
            // The driver prefixes its own format name, so the text must stand
            // on its own without one.
            assert_eq!(error.to_string(), "buffer too small for frame");
        }
    }

    /// A frame whose pixel count cannot be addressed saturates into the same
    /// "too small" answer rather than wrapping into a length the buffer
    /// appears to satisfy.
    #[test]
    fn an_unaddressable_frame_is_too_small_rather_than_wrapping() {
        let error =
            to_image_array(vec![0u8; 8], u32::MAX, u32::MAX, PixelDepth::Sixteen).unwrap_err();
        assert!(matches!(error, FrameError::TooSmall), "{error}");
    }

    // --- progress_percent ----------------------------------------------------

    #[test]
    fn progress_is_the_integer_ratio() {
        let total = Duration::from_secs(10);
        assert_eq!(progress_percent(Duration::ZERO, total), 0);
        assert_eq!(progress_percent(Duration::from_secs(3), total), 30);
        assert_eq!(progress_percent(Duration::from_millis(9_990), total), 99);
    }

    #[test]
    fn progress_never_reports_complete_while_in_flight() {
        let total = Duration::from_secs(10);
        // Exactly done, and past done, both cap at 99 — 100 belongs to the
        // ready state, and a client polling for it must not be told early.
        assert_eq!(progress_percent(total, total), 99);
        assert_eq!(progress_percent(Duration::from_secs(999), total), 99);
        assert_eq!(progress_percent(Duration::MAX, total), 99);
    }

    #[test]
    fn a_sub_microsecond_exposure_reads_as_nearly_done() {
        // Not 0: there is no ratio to report, but the exposure is finishing,
        // not unstarted.
        assert_eq!(
            progress_percent(Duration::ZERO, Duration::from_nanos(500)),
            99
        );
    }
}

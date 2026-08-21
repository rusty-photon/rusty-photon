//! Pure geometry for polar-axis measurement: unit vectors, orthogonal
//! matrices, the three-point axis fit, camera attitudes from solved
//! WCS, TAN-projection pixel↔sky mapping, and the alt/az error
//! decomposition. No ephemeris, no I/O, no wall clock — everything
//! here is deterministic and unit-tested against synthetic geometry.

use crate::error::{PolarAlignError, Result};

/// Two pointing vectors closer than this (radians, ~2 arcsec) make
/// the three-point circle fit degenerate: the mount did not actually
/// move between exposures.
const MIN_POINT_SEPARATION_RAD: f64 = 1e-5;

/// Minimum rotation (1°) between consecutive attitudes for the axis
/// extraction to rise above solve noise — a smaller rotation means
/// the mount ignored the slew and the axis would be amplified noise.
const MIN_ATTITUDE_ROTATION_RAD: f64 = std::f64::consts::PI / 180.0;

/// Maximum angle (1°) between the sign-aligned axes of consecutive
/// rotation segments; more means something other than the RA axis
/// moved between the measurement points.
const MAX_SEGMENT_DISAGREEMENT_RAD: f64 = std::f64::consts::PI / 180.0;

/// A unit-ish vector in a right-handed celestial cartesian frame
/// (x → RA 0°, y → RA 90°, z → north pole).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    #[must_use]
    #[expect(
        clippy::suboptimal_flops,
        reason = "the symmetric sum is the dot product's reference shape; fusing gains nothing the callers can observe"
    )]
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    #[must_use]
    #[expect(
        clippy::suboptimal_flops,
        reason = "the naive products cancel v × v to exactly zero; a fused rewrite leaves a sign-unstable residue on the near-parallel inputs the tangent-basis and axis extraction feed in"
    )]
    pub fn cross(self, other: Self) -> Self {
        Self {
            x: self.y * other.z - self.z * other.y,
            y: self.z * other.x - self.x * other.z,
            z: self.x * other.y - self.y * other.x,
        }
    }

    #[must_use]
    pub fn norm(self) -> f64 {
        self.dot(self).sqrt()
    }

    #[must_use]
    pub fn scale(self, s: f64) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s)
    }

    /// Unit vector in this direction, or an error when the magnitude
    /// is numerically zero (the caller's geometry collapsed).
    pub fn normalized(self) -> Result<Self> {
        let n = self.norm();
        if n < 1e-12 {
            return Err(PolarAlignError::Geometry(
                "cannot normalize a zero-length vector".to_string(),
            ));
        }
        Ok(self.scale(1.0 / n))
    }

    /// Angle to `other` in radians, numerically stable near 0 and π.
    #[must_use]
    pub fn angle_to(self, other: Self) -> f64 {
        self.cross(other).norm().atan2(self.dot(other))
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }
}

/// A 3×3 orthogonal matrix stored row-major. Camera attitudes may be
/// improper (determinant −1) when the optical train mirrors the
/// image; that is fine everywhere here because attitudes only ever
/// enter through relative rotations, where a fixed mirror cancels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3 {
    pub rows: [[f64; 3]; 3],
}

impl Mat3 {
    #[must_use]
    pub const fn from_columns(c0: Vec3, c1: Vec3, c2: Vec3) -> Self {
        Self {
            rows: [[c0.x, c1.x, c2.x], [c0.y, c1.y, c2.y], [c0.z, c1.z, c2.z]],
        }
    }

    #[must_use]
    pub const fn identity() -> Self {
        Self {
            rows: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    #[must_use]
    pub const fn transpose(self) -> Self {
        let r = self.rows;
        Self {
            rows: [
                [r[0][0], r[1][0], r[2][0]],
                [r[0][1], r[1][1], r[2][1]],
                [r[0][2], r[1][2], r[2][2]],
            ],
        }
    }

    #[must_use]
    #[expect(
        clippy::suboptimal_flops,
        reason = "row-times-column sums in the matrix product's reference shape; fusing hides the linear algebra"
    )]
    pub fn mul_vec(self, v: Vec3) -> Vec3 {
        let r = self.rows;
        Vec3::new(
            r[0][0] * v.x + r[0][1] * v.y + r[0][2] * v.z,
            r[1][0] * v.x + r[1][1] * v.y + r[1][2] * v.z,
            r[2][0] * v.x + r[2][1] * v.y + r[2][2] * v.z,
        )
    }

    #[must_use]
    pub fn mul_mat(self, other: Self) -> Self {
        // Row i of the product is row i of self dotted with each column of
        // `other`; transposing first turns every inner product into a
        // row-by-row zip.
        let columns = other.transpose().rows;
        let mut rows = [[0.0; 3]; 3];
        for (row, self_row) in rows.iter_mut().zip(&self.rows) {
            for (cell, column) in row.iter_mut().zip(&columns) {
                *cell = self_row.iter().zip(column).map(|(a, b)| a * b).sum();
            }
        }
        Self { rows }
    }

    /// The columns as vectors, in order.
    #[must_use]
    pub const fn columns(self) -> [Vec3; 3] {
        let r = self.rows;
        [
            Vec3::new(r[0][0], r[1][0], r[2][0]),
            Vec3::new(r[0][1], r[1][1], r[2][1]),
            Vec3::new(r[0][2], r[1][2], r[2][2]),
        ]
    }

    /// The determinant: +1 for a proper rotation, −1 for an improper
    /// (mirrored) orthogonal matrix.
    #[must_use]
    #[expect(
        clippy::suboptimal_flops,
        reason = "the cofactor expansion in its reference shape; fusing hides the formula"
    )]
    pub fn determinant(self) -> f64 {
        let r = self.rows;
        r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1])
            - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0])
            + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0])
    }

    /// Rodrigues rotation about a unit axis by `angle_rad`.
    #[must_use]
    #[expect(
        clippy::suboptimal_flops,
        reason = "the Rodrigues rotation formula in its reference shape; fusing hides the formula"
    )]
    pub fn from_axis_angle(axis: Vec3, angle_rad: f64) -> Self {
        let (sin_a, cos_a) = angle_rad.sin_cos();
        let t = 1.0 - cos_a;
        let (x, y, z) = (axis.x, axis.y, axis.z);
        Self {
            rows: [
                [
                    t * x * x + cos_a,
                    t * x * y - sin_a * z,
                    t * x * z + sin_a * y,
                ],
                [
                    t * x * y + sin_a * z,
                    t * y * y + cos_a,
                    t * y * z - sin_a * x,
                ],
                [
                    t * x * z - sin_a * y,
                    t * y * z + sin_a * x,
                    t * z * z + cos_a,
                ],
            ],
        }
    }
}

/// The full linear WCS of a solve: reference pixel (FITS 1-based) and
/// the 2×2 CD matrix in degrees/pixel. Mirrors the plate-solver
/// response's `wcs_matrix` block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WcsMatrix {
    pub crpix1: f64,
    pub crpix2: f64,
    pub cd1_1: f64,
    pub cd1_2: f64,
    pub cd2_1: f64,
    pub cd2_2: f64,
}

/// A solved frame reduced to what the geometry needs: the boresight
/// (image center) and the linear WCS around it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolvedFrame {
    pub center_ra_deg: f64,
    pub center_dec_deg: f64,
    pub matrix: WcsMatrix,
}

/// Celestial unit vector from RA/dec in degrees.
#[must_use]
pub fn unit_from_radec(ra_deg: f64, dec_deg: f64) -> Vec3 {
    let ra = ra_deg.to_radians();
    let dec = dec_deg.to_radians();
    Vec3::new(dec.cos() * ra.cos(), dec.cos() * ra.sin(), dec.sin())
}

/// RA/dec in degrees (RA folded to [0, 360)) from a unit vector.
#[must_use]
pub fn radec_from_unit(v: Vec3) -> (f64, f64) {
    let ra = v.y.atan2(v.x).to_degrees().rem_euclid(360.0);
    let dec = v.z.clamp(-1.0, 1.0).asin().to_degrees();
    (ra, dec)
}

/// The mount's RA-axis direction from three pointings taken with only
/// the RA axis moving: the pointings lie on a circle about the axis,
/// so the plane they span has the axis as its normal. `toward` picks
/// the sign — the returned axis lies in the same hemisphere (positive
/// dot product).
pub fn axis_from_three_points(p1: Vec3, p2: Vec3, p3: Vec3, toward: Vec3) -> Result<Vec3> {
    for (a, b, which) in [(p1, p2, "1-2"), (p2, p3, "2-3"), (p1, p3, "1-3")] {
        if (a - b).norm() < MIN_POINT_SEPARATION_RAD {
            return Err(PolarAlignError::Geometry(format!(
                "measurement points {which} are separated by less than ~2 arcsec; \
                 the mount did not move between exposures"
            )));
        }
    }
    let normal = (p2 - p1).cross(p3 - p2);
    let axis = normal.normalized().map_err(|_| {
        PolarAlignError::Geometry(
            "the three measurement points are collinear; no rotation circle fits them".to_string(),
        )
    })?;
    if axis.dot(toward) < 0.0 {
        Ok(axis.scale(-1.0))
    } else {
        Ok(axis)
    }
}

/// Tangent-plane basis at a boresight: unit east and north vectors.
/// Degenerate within ~0.2 arcsec of a celestial pole, where "east"
/// loses meaning — solves centered exactly on a pole are rejected.
fn tangent_basis(boresight: Vec3) -> Result<(Vec3, Vec3)> {
    let z = Vec3::new(0.0, 0.0, 1.0);
    let east = z.cross(boresight).normalized().map_err(|_| {
        PolarAlignError::Geometry(
            "solve center is exactly on a celestial pole; the tangent frame is undefined"
                .to_string(),
        )
    })?;
    let north = boresight.cross(east);
    Ok((east, north))
}

/// Camera attitude from a solved frame: an orthogonal matrix whose
/// columns are the sky directions of the camera's pixel-x, pixel-y,
/// and boresight axes. Mirrored optical trains yield an improper
/// matrix (det −1), which downstream relative rotations cancel.
pub fn attitude_from_wcs(frame: &SolvedFrame) -> Result<Mat3> {
    let b = unit_from_radec(frame.center_ra_deg, frame.center_dec_deg);
    let (east, north) = tangent_basis(b)?;
    let m = &frame.matrix;
    let v_x = east.scale(m.cd1_1) + north.scale(m.cd2_1);
    let v_y = east.scale(m.cd1_2) + north.scale(m.cd2_2);
    let x = v_x
        .normalized()
        .map_err(|_| PolarAlignError::Geometry("wcs_matrix pixel-x column is zero".to_string()))?;
    let y_raw = v_y - x.scale(v_y.dot(x));
    let y = y_raw.normalized().map_err(|_| {
        PolarAlignError::Geometry(
            "wcs_matrix columns are parallel; the CD matrix is singular".to_string(),
        )
    })?;
    Ok(Mat3::from_columns(x, y, b))
}

/// The rotation the camera (and with it, the whole mount head)
/// underwent between two attitudes: `now · prevᵀ`. Proper (det +1)
/// whenever both attitudes share the same parity, which they do for a
/// fixed optical train.
#[must_use]
pub fn relative_rotation(prev: Mat3, now: Mat3) -> Mat3 {
    now.mul_mat(prev.transpose())
}

/// Axis and unsigned angle of a proper rotation, from its
/// skew-symmetric part (`R − Rᵀ = 2 sin(angle) [axis]×`). The angle
/// lands in (0, π); a rotation's handedness rides in the axis sign.
/// Errors when the rotation is numerically the identity (no axis
/// exists) or within numerical noise of a half turn, where the skew
/// part vanishes and the axis is ambiguous.
pub fn rotation_axis_angle(r: Mat3) -> Result<(Vec3, f64)> {
    let m = r.rows;
    let skew = Vec3::new(m[2][1] - m[1][2], m[0][2] - m[2][0], m[1][0] - m[0][1]);
    let sin_term = skew.norm() / 2.0;
    let cos_term = (m[0][0] + m[1][1] + m[2][2] - 1.0) / 2.0;
    if sin_term < 1e-9 {
        return Err(PolarAlignError::Geometry(if cos_term > 0.0 {
            "the two attitudes are identical; there is no rotation to extract an axis from"
                .to_string()
        } else {
            "the rotation between attitudes is within numerical noise of 180°; its axis is \
             ambiguous"
                .to_string()
        }));
    }
    Ok((skew.normalized()?, sin_term.atan2(cos_term)))
}

/// The mount's RA-axis direction from camera attitudes taken with
/// only the RA axis moving: every relative rotation between them
/// (commanded sweep plus tracking, the same physical axis) is a
/// rotation about that axis. Each consecutive segment must rotate by
/// at least ~1°, and the sign-aligned segment axes must agree within
/// 1° — a disagreement means something other than the RA axis moved
/// (a meridian flip, a bumped mount). Consecutive attitudes must
/// share parity: a rigid optical train cannot change its mirror
/// state, so an improper relative transform means a solve lied about
/// parity and is rejected rather than yielding a meaningless axis.
/// `toward` picks the hemisphere, exactly as for
/// [`axis_from_three_points`].
pub fn axis_from_attitudes(attitudes: &[Mat3], toward: Vec3) -> Result<Vec3> {
    if attitudes.len() < 2 {
        return Err(PolarAlignError::Geometry(
            "at least two camera attitudes are needed to extract a rotation axis".to_string(),
        ));
    }
    let mut segments: Vec<Vec3> = Vec::with_capacity(attitudes.len().saturating_sub(1));
    let pairs = attitudes.iter().zip(attitudes.iter().skip(1));
    for (first, (&prev, &next)) in (1usize..).zip(pairs) {
        let second = first.saturating_add(1);
        let relative = relative_rotation(prev, next);
        if relative.determinant() < 0.0 {
            return Err(PolarAlignError::Geometry(format!(
                "the camera parity flipped between measurement points {first} and {second}; \
                 the solves disagree on the optical train's mirror state"
            )));
        }
        let (axis, angle) = rotation_axis_angle(relative).map_err(|e| {
            PolarAlignError::Geometry(format!(
                "between measurement points {first} and {second}: {e}"
            ))
        })?;
        if angle < MIN_ATTITUDE_ROTATION_RAD {
            return Err(PolarAlignError::Geometry(format!(
                "the mount rotated only {:.2}° between measurement points {first} and {second}; \
                 at least 1° is needed for a usable axis",
                angle.to_degrees(),
            )));
        }
        segments.push(if axis.dot(toward) < 0.0 {
            axis.scale(-1.0)
        } else {
            axis
        });
    }
    for (a, b) in segments.iter().zip(segments.iter().skip(1)) {
        let disagreement = a.angle_to(*b);
        if disagreement > MAX_SEGMENT_DISAGREEMENT_RAD {
            return Err(PolarAlignError::Geometry(format!(
                "the rotation segments between the measurement points disagree by {:.2}°; \
                 something other than the RA axis moved (a meridian flip or a bumped mount)",
                disagreement.to_degrees()
            )));
        }
    }
    segments
        .into_iter()
        .fold(Vec3::new(0.0, 0.0, 0.0), |acc, s| acc + s)
        .normalized()
}

/// The rotation taking `from` onto `to` about their common
/// perpendicular. Identity when they already coincide.
pub fn rotation_between(from: Vec3, to: Vec3) -> Result<Mat3> {
    let cross = from.cross(to);
    let angle = from.angle_to(to);
    if cross.norm() < 1e-12 {
        return Ok(Mat3::identity());
    }
    Ok(Mat3::from_axis_angle(cross.normalized()?, angle))
}

/// Sky direction of a pixel through the frame's TAN projection.
#[expect(
    clippy::suboptimal_flops,
    reason = "the CD-matrix application in its reference shape; fusing hides the projection"
)]
pub fn wcs_pixel_to_sky(frame: &SolvedFrame, x: f64, y: f64) -> Result<Vec3> {
    let b = unit_from_radec(frame.center_ra_deg, frame.center_dec_deg);
    let (east, north) = tangent_basis(b)?;
    let m = &frame.matrix;
    let dx = x - m.crpix1;
    let dy = y - m.crpix2;
    let xi = (m.cd1_1 * dx + m.cd1_2 * dy).to_radians();
    let eta = (m.cd2_1 * dx + m.cd2_2 * dy).to_radians();
    (b + east.scale(xi) + north.scale(eta)).normalized()
}

/// Pixel position of a sky direction through the frame's TAN
/// projection. `None` when the direction is behind the tangent plane
/// (more than 90° from the boresight).
#[expect(
    clippy::suboptimal_flops,
    reason = "Cramer's rule for the CD-matrix inverse in its reference shape; fusing hides the projection"
)]
pub fn wcs_sky_to_pixel(frame: &SolvedFrame, sky: Vec3) -> Result<Option<(f64, f64)>> {
    let b = unit_from_radec(frame.center_ra_deg, frame.center_dec_deg);
    let (east, north) = tangent_basis(b)?;
    let along = sky.dot(b);
    if along <= 0.0 {
        return Ok(None);
    }
    let xi = (sky.dot(east) / along).to_degrees();
    let eta = (sky.dot(north) / along).to_degrees();
    let m = &frame.matrix;
    let det = m.cd1_1 * m.cd2_2 - m.cd1_2 * m.cd2_1;
    if det.abs() < 1e-18 {
        return Err(PolarAlignError::Geometry(
            "wcs_matrix CD matrix is singular; cannot invert the projection".to_string(),
        ));
    }
    let dx = (m.cd2_2 * xi - m.cd1_2 * eta) / det;
    let dy = (-m.cd2_1 * xi + m.cd1_1 * eta) / det;
    Ok(Some((m.crpix1 + dx, m.crpix2 + dy)))
}

/// The linear WCS a solve would report for a camera at `attitude`
/// with `scale_deg_per_px` square pixels and reference pixel
/// `crpix` — the exact inverse of [`attitude_from_wcs`], for
/// attitudes whose columns are orthonormal with the boresight third.
/// Test choreography uses it to synthesize solves from attitudes
/// rotated about a known axis.
pub fn wcs_from_attitude(
    attitude: Mat3,
    scale_deg_per_px: f64,
    crpix: (f64, f64),
) -> Result<SolvedFrame> {
    let [x, y, b] = attitude.columns();
    let (center_ra_deg, center_dec_deg) = radec_from_unit(b);
    let (east, north) = tangent_basis(b)?;
    Ok(SolvedFrame {
        center_ra_deg,
        center_dec_deg,
        matrix: WcsMatrix {
            crpix1: crpix.0,
            crpix2: crpix.1,
            cd1_1: scale_deg_per_px * x.dot(east),
            cd1_2: scale_deg_per_px * y.dot(east),
            cd2_1: scale_deg_per_px * x.dot(north),
            cd2_2: scale_deg_per_px * y.dot(north),
        },
    })
}

/// Unit vector in the horizontal cartesian frame
/// (x → north, y → east, z → zenith) for an az/alt in degrees.
#[must_use]
pub fn horizontal_unit(azimuth_deg: f64, altitude_deg: f64) -> Vec3 {
    let az = azimuth_deg.to_radians();
    let alt = altitude_deg.to_radians();
    Vec3::new(alt.cos() * az.cos(), alt.cos() * az.sin(), alt.sin())
}

/// Signed alignment errors between the measured axis and the pole
/// target, both given as observed (azimuth, altitude) in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlignmentErrors {
    /// True angular offset toward the east horizon, degrees.
    /// Positive = the axis sits east of the pole.
    pub azimuth_error_deg: f64,
    /// Altitude offset, degrees. Positive = the axis sits above the
    /// pole.
    pub altitude_error_deg: f64,
    /// Great-circle separation between axis and pole, degrees.
    pub total_error_deg: f64,
}

impl AlignmentErrors {
    #[must_use]
    pub fn azimuth_direction(&self) -> &'static str {
        if self.azimuth_error_deg >= 0.0 {
            "move azimuth west"
        } else {
            "move azimuth east"
        }
    }

    #[must_use]
    pub fn altitude_direction(&self) -> &'static str {
        if self.altitude_error_deg >= 0.0 {
            "lower altitude"
        } else {
            "raise altitude"
        }
    }
}

/// Decompose the axis-to-pole offset into the two adjuster motions.
///
/// Azimuth is measured as the true angular component along the local
/// east direction (a great-circle angle, not a coordinate azimuth
/// difference — the two differ by cos(altitude)).
#[must_use]
pub fn alignment_errors(
    axis_azimuth_deg: f64,
    axis_altitude_deg: f64,
    pole_azimuth_deg: f64,
    pole_altitude_deg: f64,
) -> AlignmentErrors {
    let axis = horizontal_unit(axis_azimuth_deg, axis_altitude_deg);
    let pole = horizontal_unit(pole_azimuth_deg, pole_altitude_deg);
    let east = Vec3::new(0.0, 1.0, 0.0);
    AlignmentErrors {
        azimuth_error_deg: axis.dot(east).clamp(-1.0, 1.0).asin().to_degrees(),
        altitude_error_deg: axis_altitude_deg - pole_altitude_deg,
        total_error_deg: axis.angle_to(pole).to_degrees(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const TIGHT: f64 = 1e-9;

    fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
        assert!((a - b).abs() < tol, "{what}: {a} vs {b} (tol {tol})");
    }

    fn assert_vec_close(a: Vec3, b: Vec3, tol: f64, what: &str) {
        assert!((a - b).norm() < tol, "{what}: {a:?} vs {b:?} (tol {tol})");
    }

    /// A frame with square pixels, north-up / east-left orientation
    /// (the standard sky-image parity: negative CD determinant).
    fn normal_parity_frame(ra: f64, dec: f64) -> SolvedFrame {
        SolvedFrame {
            center_ra_deg: ra,
            center_dec_deg: dec,
            matrix: WcsMatrix {
                crpix1: 512.0,
                crpix2: 384.0,
                cd1_1: -2.9167e-4,
                cd1_2: 0.0,
                cd2_1: 0.0,
                cd2_2: 2.9167e-4,
            },
        }
    }

    /// The same frame mirror-flipped (positive CD determinant).
    fn flipped_parity_frame(ra: f64, dec: f64) -> SolvedFrame {
        let mut frame = normal_parity_frame(ra, dec);
        frame.matrix.cd1_1 = 2.9167e-4;
        frame
    }

    #[test]
    fn test_unit_from_radec_round_trips() {
        for (ra, dec) in [(0.0, 0.0), (123.4, 56.7), (359.0, -89.0), (42.0, 85.0)] {
            let (ra_back, dec_back) = radec_from_unit(unit_from_radec(ra, dec));
            assert_close(ra_back, ra, 1e-9, "ra round trip");
            assert_close(dec_back, dec, 1e-9, "dec round trip");
        }
    }

    #[test]
    fn test_rotation_round_trips_a_vector() {
        let axis = unit_from_radec(200.0, 30.0);
        let v = unit_from_radec(10.0, 80.0);
        let r = Mat3::from_axis_angle(axis, 0.7);
        let back = r.transpose().mul_vec(r.mul_vec(v));
        assert_vec_close(back, v, TIGHT, "R^T R v = v");
    }

    #[test]
    fn test_rotation_preserves_the_axis() {
        let axis = unit_from_radec(15.0, 88.0);
        let r = Mat3::from_axis_angle(axis, 1.2);
        assert_vec_close(r.mul_vec(axis), axis, TIGHT, "rotation fixes its axis");
    }

    /// Three pointings generated by rotating a start vector about a
    /// known axis must reproduce that axis to numerical precision, in
    /// both hemispheres and both sweep directions.
    #[test]
    fn test_axis_from_three_points_recovers_a_known_axis() {
        for (axis_ra, axis_dec, toward_dec) in [
            (37.0, 89.2, 90.0),
            (300.0, 88.5, 90.0),
            (120.0, -89.4, -90.0),
        ] {
            for sweep_sign in [1.0, -1.0] {
                let axis = unit_from_radec(axis_ra, axis_dec);
                // Start ~5° off-axis, as the dec-85 measurement does.
                let start = Mat3::from_axis_angle(
                    unit_from_radec(axis_ra + 90.0, 0.0),
                    5.0_f64.to_radians(),
                )
                .mul_vec(axis);
                let p2 =
                    Mat3::from_axis_angle(axis, sweep_sign * 45.0_f64.to_radians()).mul_vec(start);
                let p3 =
                    Mat3::from_axis_angle(axis, sweep_sign * 90.0_f64.to_radians()).mul_vec(start);
                let toward = unit_from_radec(0.0, toward_dec);
                let fitted = axis_from_three_points(start, p2, p3, toward).unwrap();
                assert_vec_close(fitted, axis, 1e-9, "fitted axis");
            }
        }
    }

    #[test]
    fn test_axis_from_three_points_rejects_an_unmoved_mount() {
        let p = unit_from_radec(10.0, 85.0);
        let err = axis_from_three_points(p, p, unit_from_radec(50.0, 85.0), p).unwrap_err();
        assert!(err.to_string().contains("did not move"), "{err}");
    }

    #[test]
    fn test_attitude_columns_are_orthonormal_for_both_parities() {
        for frame in [
            normal_parity_frame(30.0, 85.0),
            flipped_parity_frame(30.0, 85.0),
        ] {
            let a = attitude_from_wcs(&frame).unwrap();
            let at_a = a.transpose().mul_mat(a);
            for i in 0..3 {
                for j in 0..3 {
                    let expected = if i == j { 1.0 } else { 0.0 };
                    assert_close(at_a.rows[i][j], expected, 1e-9, "AᵀA = I");
                }
            }
        }
    }

    #[test]
    fn test_attitude_rejects_a_pole_centered_solve() {
        let frame = normal_parity_frame(0.0, 90.0);
        let err = attitude_from_wcs(&frame).unwrap_err();
        assert!(err.to_string().contains("celestial pole"), "{err}");
    }

    /// The load-bearing property of the adjustment loop: the relative
    /// rotation between two attitudes recovers the sky-frame rotation
    /// applied between them — for mirrored optical trains too, since
    /// the fixed mirror cancels in `now · prevᵀ`.
    #[test]
    fn test_relative_rotation_recovers_an_applied_rotation() {
        for base in [
            normal_parity_frame(40.0, 84.0),
            flipped_parity_frame(40.0, 84.0),
        ] {
            let a0 = attitude_from_wcs(&base).unwrap();
            let applied = Mat3::from_axis_angle(unit_from_radec(0.0, 48.0), 0.01);
            let a1 = applied.mul_mat(a0);
            let recovered = relative_rotation(a0, a1);
            for i in 0..3 {
                for j in 0..3 {
                    assert_close(
                        recovered.rows[i][j],
                        applied.rows[i][j],
                        1e-9,
                        "recovered rotation",
                    );
                }
            }
        }
    }

    /// Axis update through a relative rotation: rotating the mount
    /// head moves the axis with it, and tracking (a rotation about
    /// the axis itself) does not move it at all.
    #[test]
    fn test_axis_update_sees_adjustment_but_not_tracking() {
        let axis = unit_from_radec(15.0, 88.9);
        let adjustment = Mat3::from_axis_angle(unit_from_radec(105.0, 0.0), 0.005);
        let tracking = Mat3::from_axis_angle(axis, 0.3);
        let moved = adjustment.mul_mat(tracking);
        let updated = moved.mul_vec(axis);
        assert_vec_close(
            updated,
            adjustment.mul_vec(axis),
            TIGHT,
            "tracking drops out of the axis update",
        );
        assert!(
            updated.angle_to(axis) > 0.004,
            "the adjustment itself must move the axis"
        );
    }

    #[test]
    fn test_rotation_axis_angle_recovers_axis_and_angle() {
        let axis = unit_from_radec(200.0, 30.0);
        for angle in [0.1, 0.8, 2.0] {
            let (u, a) = rotation_axis_angle(Mat3::from_axis_angle(axis, angle)).unwrap();
            assert_vec_close(u, axis, TIGHT, "extracted axis");
            assert_close(a, angle, TIGHT, "extracted angle");
        }
        // A negative rotation extracts as the flipped axis with a
        // positive angle — same rotation, angle stays in (0, π).
        let (u, a) = rotation_axis_angle(Mat3::from_axis_angle(axis, -0.7)).unwrap();
        assert_vec_close(u, axis.scale(-1.0), TIGHT, "flipped axis");
        assert_close(a, 0.7, TIGHT, "positive angle");
    }

    #[test]
    fn test_rotation_axis_angle_rejects_identity_and_half_turn() {
        let err = rotation_axis_angle(Mat3::identity()).unwrap_err();
        assert!(err.to_string().contains("identical"), "{err}");
        let half_turn = Mat3::from_axis_angle(unit_from_radec(10.0, 20.0), std::f64::consts::PI);
        let err = rotation_axis_angle(half_turn).unwrap_err();
        assert!(err.to_string().contains("180"), "{err}");
    }

    /// Attitude at a boresight with pixel axes derived from the local
    /// tangent frame — any rigid choice works, only relative
    /// rotations enter the axis extraction.
    fn attitude_with_boresight(b: Vec3) -> Mat3 {
        let x = Vec3::new(0.0, 0.0, 1.0).cross(b).normalized().unwrap();
        let y = b.cross(x);
        Mat3::from_columns(x, y, b)
    }

    #[test]
    fn test_axis_from_attitudes_recovers_a_known_axis_both_parities() {
        for (axis_ra, axis_dec, toward_dec) in [(37.0, 89.2, 90.0), (120.0, -89.4, -90.0)] {
            let axis = unit_from_radec(axis_ra, axis_dec);
            // Boresight 35° off-axis: a mid-sky pointing, where the
            // plane fit is legal but this method must work anyway.
            let offset =
                Mat3::from_axis_angle(unit_from_radec(axis_ra + 90.0, 0.0), 35.0_f64.to_radians());
            let a0_proper = attitude_with_boresight(offset.mul_vec(axis));
            let a0_mirrored = {
                let a = a0_proper;
                let [c0, c1, c2] = a.columns();
                Mat3::from_columns(c0.scale(-1.0), c1, c2)
            };
            for a0 in [a0_proper, a0_mirrored] {
                let attitudes: Vec<Mat3> = [0.0_f64, 45.0, 90.0]
                    .iter()
                    .map(|deg| Mat3::from_axis_angle(axis, deg.to_radians()).mul_mat(a0))
                    .collect();
                let toward = unit_from_radec(0.0, toward_dec);
                let fitted = axis_from_attitudes(&attitudes, toward).unwrap();
                assert_vec_close(fitted, axis, 1e-9, "attitude-fitted axis");
            }
        }
    }

    /// Tracking between exposures is a rotation about the axis
    /// itself; unequal amounts per segment must not move the result.
    #[test]
    fn test_axis_from_attitudes_ignores_tracking_between_points() {
        let axis = unit_from_radec(15.0, 88.9);
        let offset = Mat3::from_axis_angle(unit_from_radec(105.0, 0.0), 5.0_f64.to_radians());
        let a0 = attitude_with_boresight(offset.mul_vec(axis));
        let attitudes: Vec<Mat3> = [0.0_f64, 45.0 + 3.2, 90.0 + 3.2 + 7.9]
            .iter()
            .map(|deg| Mat3::from_axis_angle(axis, deg.to_radians()).mul_mat(a0))
            .collect();
        let fitted = axis_from_attitudes(&attitudes, unit_from_radec(0.0, 90.0)).unwrap();
        assert_vec_close(fitted, axis, 1e-9, "tracking-immune axis");
    }

    #[test]
    fn test_axis_from_attitudes_rejects_an_unmoved_mount() {
        let axis = unit_from_radec(15.0, 88.9);
        let a0 = attitude_with_boresight(unit_from_radec(20.0, 84.0));
        let barely = Mat3::from_axis_angle(axis, 0.5_f64.to_radians()).mul_mat(a0);
        let err = axis_from_attitudes(&[a0, barely], unit_from_radec(0.0, 90.0)).unwrap_err();
        assert!(err.to_string().contains("rotated only"), "{err}");
        let err = axis_from_attitudes(&[a0], unit_from_radec(0.0, 90.0)).unwrap_err();
        assert!(err.to_string().contains("at least two"), "{err}");
    }

    /// A dec-axis rotation injected into one segment must be caught
    /// as a disagreement, not averaged into a wrong axis.
    #[test]
    fn test_axis_from_attitudes_rejects_a_dec_axis_move() {
        let axis = unit_from_radec(15.0, 88.9);
        let a0 = attitude_with_boresight(unit_from_radec(20.0, 84.0));
        let a1 = Mat3::from_axis_angle(axis, 45.0_f64.to_radians()).mul_mat(a0);
        let dec_bump = Mat3::from_axis_angle(unit_from_radec(105.0, 0.0), 3.0_f64.to_radians());
        let a2 = dec_bump
            .mul_mat(Mat3::from_axis_angle(axis, 90.0_f64.to_radians()))
            .mul_mat(a0);
        let err = axis_from_attitudes(&[a0, a1, a2], unit_from_radec(0.0, 90.0)).unwrap_err();
        assert!(err.to_string().contains("disagree"), "{err}");
    }

    /// A solve that flips parity mid-measurement makes the relative
    /// transform improper; the extraction must reject it instead of
    /// producing a meaningless axis.
    #[test]
    fn test_axis_from_attitudes_rejects_a_parity_flip() {
        let axis = unit_from_radec(15.0, 88.9);
        let a0 = attitude_with_boresight(unit_from_radec(20.0, 84.0));
        assert!(a0.determinant() > 0.0, "test attitude starts proper");
        let rotated = Mat3::from_axis_angle(axis, 45.0_f64.to_radians()).mul_mat(a0);
        let [c0, c1, c2] = rotated.columns();
        let flipped = Mat3::from_columns(c0.scale(-1.0), c1, c2);
        assert!(flipped.determinant() < 0.0, "flipped attitude is improper");
        let err = axis_from_attitudes(&[a0, flipped], unit_from_radec(0.0, 90.0)).unwrap_err();
        assert!(err.to_string().contains("parity flipped"), "{err}");
        assert!(err.to_string().contains("points 1 and 2"), "{err}");
    }

    #[test]
    fn test_wcs_from_attitude_round_trips_attitude_from_wcs() {
        for frame in [
            normal_parity_frame(52.0, 40.0),
            flipped_parity_frame(52.0, 40.0),
        ] {
            let attitude = attitude_from_wcs(&frame).unwrap();
            let synthesized = wcs_from_attitude(attitude, 2.9167e-4, (512.0, 384.0)).unwrap();
            assert_close(
                synthesized.center_ra_deg,
                frame.center_ra_deg,
                1e-9,
                "center ra",
            );
            assert_close(
                synthesized.center_dec_deg,
                frame.center_dec_deg,
                1e-9,
                "center dec",
            );
            let back = attitude_from_wcs(&synthesized).unwrap();
            for i in 0..3 {
                for j in 0..3 {
                    assert_close(
                        back.rows[i][j],
                        attitude.rows[i][j],
                        1e-9,
                        "round-tripped attitude",
                    );
                }
            }
        }
    }

    /// End-to-end current-position synthetic measurement: attitudes
    /// rotated about a misaligned axis from a mid-sky boresight must
    /// recover the injected error — and agree with the plane fit of
    /// their own boresights.
    #[test]
    fn test_synthetic_current_position_measurement_recovers_injected_errors() {
        let pole_alt: f64 = 48.0;
        let east_err_deg: f64 = 20.0 / 60.0;
        let alt_err_deg: f64 = -12.0 / 60.0;
        let axis_alt = pole_alt + alt_err_deg;
        let axis_az = (east_err_deg.to_radians().sin() / axis_alt.to_radians().cos())
            .asin()
            .to_degrees();
        let axis = horizontal_unit(axis_az, axis_alt);

        // Boresight 35° from the axis — nowhere near the pole.
        let offset = Mat3::from_axis_angle(
            axis.cross(Vec3::new(0.0, 0.0, 1.0)).normalized().unwrap(),
            35.0_f64.to_radians(),
        );
        let a0 = attitude_with_boresight(offset.mul_vec(axis));
        let attitudes: Vec<Mat3> = [0.0_f64, 45.0, 90.0]
            .iter()
            .map(|deg| Mat3::from_axis_angle(axis, deg.to_radians()).mul_mat(a0))
            .collect();

        let toward = horizontal_unit(0.0, pole_alt);
        let fitted = axis_from_attitudes(&attitudes, toward).unwrap();
        let (az_fit, alt_fit) = {
            let az = fitted.y.atan2(fitted.x).to_degrees();
            let alt = fitted.z.clamp(-1.0, 1.0).asin().to_degrees();
            (az, alt)
        };
        let errors = alignment_errors(az_fit, alt_fit, 0.0, pole_alt);
        assert_close(
            errors.azimuth_error_deg * 60.0,
            20.0,
            1e-6,
            "recovered azimuth error (arcmin)",
        );
        assert_close(
            errors.altitude_error_deg * 60.0,
            -12.0,
            1e-6,
            "recovered altitude error (arcmin)",
        );

        let plane = axis_from_three_points(
            attitudes[0].columns()[2],
            attitudes[1].columns()[2],
            attitudes[2].columns()[2],
            toward,
        )
        .unwrap();
        assert_vec_close(
            plane,
            fitted,
            1e-9,
            "plane fit agrees with the attitude fit",
        );
    }

    #[test]
    fn test_pixel_sky_round_trip_for_both_parities() {
        for frame in [
            normal_parity_frame(52.0, 85.5),
            flipped_parity_frame(52.0, 85.5),
        ] {
            for (x, y) in [(512.0, 384.0), (100.0, 700.0), (1000.5, 12.25)] {
                let sky = wcs_pixel_to_sky(&frame, x, y).unwrap();
                let (x_back, y_back) = wcs_sky_to_pixel(&frame, sky).unwrap().unwrap();
                assert_close(x_back, x, 1e-6, "pixel x round trip");
                assert_close(y_back, y, 1e-6, "pixel y round trip");
            }
        }
    }

    #[test]
    fn test_boresight_projects_to_the_reference_pixel() {
        let frame = normal_parity_frame(52.0, 85.5);
        let b = unit_from_radec(52.0, 85.5);
        let (x, y) = wcs_sky_to_pixel(&frame, b).unwrap().unwrap();
        assert_close(x, 512.0, 1e-9, "crpix1");
        assert_close(y, 384.0, 1e-9, "crpix2");
    }

    #[test]
    fn test_sky_behind_the_tangent_plane_projects_to_none() {
        let frame = normal_parity_frame(52.0, 85.5);
        let behind = unit_from_radec(232.0, -85.5);
        assert!(wcs_sky_to_pixel(&frame, behind).unwrap().is_none());
    }

    /// One pixel step must move the sky direction by the pixel scale.
    #[test]
    fn test_pixel_scale_is_honored() {
        let frame = normal_parity_frame(52.0, 85.5);
        let a = wcs_pixel_to_sky(&frame, 512.0, 384.0).unwrap();
        let b = wcs_pixel_to_sky(&frame, 513.0, 384.0).unwrap();
        let arcsec = a.angle_to(b).to_degrees() * 3600.0;
        assert_close(arcsec, 1.05, 0.01, "pixel scale in arcsec");
    }

    #[test]
    fn test_alignment_errors_decompose_a_known_offset() {
        // Axis 0.5° east (in true angle) and 0.3° above a pole at
        // altitude 48°: coordinate azimuth ≈ 0.5 / cos(48.3°).
        let axis_alt: f64 = 48.3;
        let axis_az_deg = (0.5_f64.to_radians().sin() / axis_alt.to_radians().cos())
            .asin()
            .to_degrees();
        let errors = alignment_errors(axis_az_deg, axis_alt, 0.0, 48.0);
        assert_close(errors.azimuth_error_deg, 0.5, 1e-4, "azimuth error");
        assert_close(errors.altitude_error_deg, 0.3, 1e-9, "altitude error");
        assert!(errors.total_error_deg > 0.55 && errors.total_error_deg < 0.62);
        assert_eq!(errors.azimuth_direction(), "move azimuth west");
        assert_eq!(errors.altitude_direction(), "lower altitude");
    }

    #[test]
    fn test_alignment_errors_signs_in_the_southern_hemisphere() {
        // Facing the south celestial pole (azimuth 180): an axis at
        // azimuth 179.5° is offset toward the east horizon.
        let errors = alignment_errors(179.5, 33.0, 180.0, 33.5);
        assert!(
            errors.azimuth_error_deg > 0.0,
            "azimuth 179.5 facing south is east of the pole"
        );
        assert!(errors.altitude_error_deg < 0.0);
        assert_eq!(errors.azimuth_direction(), "move azimuth west");
        assert_eq!(errors.altitude_direction(), "raise altitude");
    }

    #[test]
    fn test_perfectly_aligned_axis_reports_zero_errors() {
        let errors = alignment_errors(0.0, 48.0, 0.0, 48.0);
        assert_close(errors.azimuth_error_deg, 0.0, TIGHT, "azimuth");
        assert_close(errors.altitude_error_deg, 0.0, TIGHT, "altitude");
        assert_close(errors.total_error_deg, 0.0, TIGHT, "total");
    }

    #[test]
    fn test_rotation_between_takes_from_onto_to() {
        let from = horizontal_unit(0.4, 47.7);
        let to = horizontal_unit(0.0, 48.0);
        let r = rotation_between(from, to).unwrap();
        assert_vec_close(r.mul_vec(from), to, TIGHT, "rotation_between");
    }

    #[test]
    fn test_rotation_between_identical_vectors_is_identity() {
        let v = horizontal_unit(10.0, 50.0);
        let r = rotation_between(v, v).unwrap();
        assert_vec_close(r.mul_vec(v), v, TIGHT, "identity rotation");
    }

    /// End-to-end synthetic measurement: a mount whose axis is offset
    /// from the pole by a known (east, up) error, measured through
    /// three 45°-spaced RA rotations, must report that error.
    #[test]
    fn test_synthetic_measurement_recovers_injected_errors() {
        // Work directly in the horizontal frame: pole at (az 0,
        // alt 48), axis displaced 20' east (true angle) and -12'
        // in altitude.
        let pole_alt: f64 = 48.0;
        let east_err_deg: f64 = 20.0 / 60.0;
        let alt_err_deg: f64 = -12.0 / 60.0;
        let axis_alt = pole_alt + alt_err_deg;
        let axis_az = (east_err_deg.to_radians().sin() / axis_alt.to_radians().cos())
            .asin()
            .to_degrees();
        let axis = horizontal_unit(axis_az, axis_alt);

        // Scope 5° off-axis, three points 45° apart in RA.
        let offset = Mat3::from_axis_angle(
            axis.cross(Vec3::new(0.0, 0.0, 1.0)).normalized().unwrap(),
            5.0_f64.to_radians(),
        );
        let start = offset.mul_vec(axis);
        let p2 = Mat3::from_axis_angle(axis, 45.0_f64.to_radians()).mul_vec(start);
        let p3 = Mat3::from_axis_angle(axis, 90.0_f64.to_radians()).mul_vec(start);

        let fitted = axis_from_three_points(start, p2, p3, horizontal_unit(0.0, pole_alt)).unwrap();
        let (az_fit, alt_fit) = {
            let az = fitted.y.atan2(fitted.x).to_degrees();
            let alt = fitted.z.clamp(-1.0, 1.0).asin().to_degrees();
            (az, alt)
        };
        let errors = alignment_errors(az_fit, alt_fit, 0.0, pole_alt);
        assert_close(
            errors.azimuth_error_deg * 60.0,
            20.0,
            1e-6,
            "recovered azimuth error (arcmin)",
        );
        assert_close(
            errors.altitude_error_deg * 60.0,
            -12.0,
            1e-6,
            "recovered altitude error (arcmin)",
        );
    }
}

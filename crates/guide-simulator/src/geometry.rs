//! Offsets on the sky and points on the sensor.

/// An offset on the sky, in arcseconds: `east` toward increasing RA,
/// `north` toward increasing Dec.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SkyOffset {
    /// Arcseconds toward increasing RA.
    pub east: f64,
    /// Arcseconds toward increasing Dec.
    pub north: f64,
}

impl SkyOffset {
    /// No offset.
    pub const ZERO: Self = Self {
        east: 0.0,
        north: 0.0,
    };

    /// The offset `east` arcseconds East and `north` North.
    #[must_use]
    pub const fn new(east: f64, north: f64) -> Self {
        Self { east, north }
    }

    /// The offset scaled by `factor`.
    #[must_use]
    pub fn scaled(self, factor: f64) -> Self {
        Self::new(self.east * factor, self.north * factor)
    }

    /// The sum of the two offsets.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self::new(self.east + other.east, self.north + other.north)
    }

    /// This offset less `other`.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        Self::new(self.east - other.east, self.north - other.north)
    }
}

/// A point on the sensor, or an offset between two, in pixels. The
/// centre of the pixel in row `r` and column `c` lies at `x = c`,
/// `y = r`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SensorPoint {
    /// Pixels along a row, toward increasing column.
    pub x: f64,
    /// Pixels along a column, toward increasing row.
    pub y: f64,
}

impl SensorPoint {
    /// The point at `x`, `y`.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// The offset from `other` to this point.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y)
    }
}

/// An index or a count as a coordinate. The indices of an in-memory frame,
/// and the counts the models loop over, fit `u32`.
pub fn coordinate(index: usize) -> f64 {
    f64::from(u32::try_from(index).unwrap_or(u32::MAX))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn sky_offsets_add_subtract_and_scale() {
        let a = SkyOffset::new(1.0, 2.0);
        let b = SkyOffset::new(0.5, -1.0);
        assert_eq!(a.plus(b), SkyOffset::new(1.5, 1.0));
        assert_eq!(a.minus(b), SkyOffset::new(0.5, 3.0));
        assert_eq!(a.scaled(2.0), SkyOffset::new(2.0, 4.0));
    }

    #[test]
    fn sensor_points_subtract() {
        assert_eq!(
            SensorPoint::new(3.0, 4.0).minus(SensorPoint::new(1.0, 1.5)),
            SensorPoint::new(2.0, 2.5)
        );
    }

    #[test]
    fn coordinate_is_the_index() {
        assert_eq!(coordinate(0), 0.0);
        assert_eq!(coordinate(640), 640.0);
    }
}

//! Whether a guide star the measurement did not use is still there: a
//! fresh star at its last place with its last flux. See
//! `docs/crates/guide-replay.md` (The replay).

use rp_guiding::{Frame, GuideStars, Position, StarParams, Usable};

/// How many of the brightest guide stars a jump of the field must move
/// together.
const JUMP_STARS: usize = 3;

/// How closely the brightest guide stars' moves must agree to be one jump
/// of the field, in pixels.
const JUMP_AGREEMENT_PX: f64 = 2.0;

/// Where a guide star was last usable, and its flux there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Last {
    /// Its position.
    pub position: Position,
    /// Its flux.
    pub flux: f64,
}

impl Last {
    /// Its reading, when usable.
    pub const fn of(star: &Usable) -> Self {
        Self {
            position: star.position,
            flux: star.flux,
        }
    }

    /// Whether `star` has this flux, within the measurement's own flux
    /// band.
    fn flux_matches(&self, star: &Usable, params: StarParams) -> bool {
        (star.flux - self.flux).abs() <= params.mass_change * self.flux
    }
}

/// Every star a fresh selection keeps in `frame`: all that pass the
/// guider's tests, with no cut to `stars.max` and an edge margin of the
/// search region alone.
pub fn fresh_stars(frame: &Frame, params: StarParams) -> Vec<Usable> {
    let wide = StarParams {
        max: usize::MAX,
        min: 1,
        calibration_distance_px: 0.0,
        ..params
    };
    GuideStars::select(frame, wide).map_or_else(
        |_| Vec::new(),
        |(_, measurement)| {
            measurement
                .stars
                .iter()
                .filter_map(|reading| match reading {
                    rp_guiding::StarReading::Usable(star) => Some(*star),
                    rp_guiding::StarReading::NotFound
                    | rp_guiding::StarReading::Faint { .. }
                    | rp_guiding::StarReading::FluxChanged { .. } => None,
                })
                .collect()
        },
    )
}

/// Whether a guide star last seen as `last` is there among `fresh`: a
/// fresh star within the search region of its last place, with its last
/// flux.
pub fn there(last: Option<Last>, fresh: &[Usable], params: StarParams) -> bool {
    last.is_some_and(|last| {
        fresh.iter().any(|star| {
            distance(star.position, last.position) <= params.search_region_px
                && last.flux_matches(star, params)
        })
    })
}

/// Whether the field jumped beyond the search region: the brightest guide
/// stars, up to three, all found among `fresh` with their last flux,
/// moved by one offset.
pub fn jumped(last: &[Option<Last>], fresh: &[Usable], params: StarParams) -> bool {
    let leaders: Vec<Last> = last.iter().take(JUMP_STARS).flatten().copied().collect();
    let Some((first, others)) = leaders.split_first() else {
        return false;
    };
    if others.is_empty() {
        return false;
    }
    fresh
        .iter()
        .filter(|star| first.flux_matches(star, params))
        .map(|star| Position {
            x: star.position.x - first.position.x,
            y: star.position.y - first.position.y,
        })
        .filter(|jump| jump.x.hypot(jump.y) > params.search_region_px)
        .any(|jump| {
            others.iter().all(|other| {
                let landing = Position {
                    x: other.position.x + jump.x,
                    y: other.position.y + jump.y,
                };
                fresh.iter().any(|star| {
                    distance(star.position, landing) <= JUMP_AGREEMENT_PX
                        && other.flux_matches(star, params)
                })
            })
        })
}

fn distance(a: Position, b: Position) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use rp_guiding::Offset;

    use super::*;

    fn star(x: f64, y: f64, flux: f64) -> Usable {
        Usable {
            position: Position { x, y },
            offset: Offset::default(),
            hfd: 3.0,
            snr: 50.0,
            flux,
        }
    }

    fn last(x: f64, y: f64, flux: f64) -> Last {
        Last::of(&star(x, y, flux))
    }

    #[test]
    fn a_star_is_there_within_the_search_region_with_its_flux() {
        let params = StarParams::default();
        let was = Some(last(100.0, 100.0, 1_000.0));
        assert!(there(was, &[star(110.0, 108.0, 1_400.0)], params));
        assert!(there(was, &[star(115.0, 100.0, 600.0)], params));
        assert!(there(
            was,
            &[star(500.0, 50.0, 1_000.0), star(99.0, 101.0, 1_000.0)],
            params
        ));
    }

    #[test]
    fn a_star_beyond_the_search_region_or_off_its_flux_is_not_there() {
        let params = StarParams::default();
        let was = Some(last(100.0, 100.0, 1_000.0));
        assert!(!there(was, &[star(115.1, 100.0, 1_000.0)], params));
        assert!(!there(was, &[star(100.0, 100.0, 499.0)], params));
        assert!(!there(was, &[star(100.0, 100.0, 1_501.0)], params));
        assert!(!there(was, &[star(500.0, 50.0, 1_000.0)], params));
        assert!(!there(was, &[], params));
        assert!(!there(None, &[star(100.0, 100.0, 1_000.0)], params));
    }

    #[test]
    fn the_brightest_stars_moved_together_beyond_the_search_region_are_a_jump() {
        let params = StarParams::default();
        let was = [
            Some(last(100.0, 100.0, 9_000.0)),
            Some(last(300.0, 150.0, 6_000.0)),
            Some(last(200.0, 250.0, 4_000.0)),
            Some(last(50.0, 50.0, 1_000.0)),
        ];
        let moved = [
            star(140.0, 130.0, 9_100.0),
            star(341.0, 181.0, 5_900.0),
            star(239.0, 279.0, 4_200.0),
        ];
        assert!(jumped(&was, &moved, params));
    }

    #[test]
    fn stars_that_did_not_all_move_together_are_no_jump() {
        let params = StarParams::default();
        let was = [
            Some(last(100.0, 100.0, 9_000.0)),
            Some(last(300.0, 150.0, 6_000.0)),
            Some(last(200.0, 250.0, 4_000.0)),
        ];
        let one_elsewhere = [
            star(140.0, 130.0, 9_100.0),
            star(341.0, 181.0, 5_900.0),
            star(245.0, 280.0, 4_200.0),
        ];
        assert!(!jumped(&was, &one_elsewhere, params));
        let one_dimmed = [
            star(140.0, 130.0, 9_100.0),
            star(340.0, 180.0, 2_000.0),
            star(240.0, 280.0, 4_200.0),
        ];
        assert!(!jumped(&was, &one_dimmed, params));
        let within_the_region = [
            star(110.0, 105.0, 9_000.0),
            star(310.0, 155.0, 6_000.0),
            star(210.0, 255.0, 4_000.0),
        ];
        assert!(!jumped(&was, &within_the_region, params));
    }

    #[test]
    fn a_jump_needs_two_stars_to_agree() {
        let params = StarParams::default();
        let moved = [star(140.0, 130.0, 9_000.0)];
        assert!(!jumped(
            &[Some(last(100.0, 100.0, 9_000.0))],
            &moved,
            params
        ));
        assert!(!jumped(&[], &moved, params));
        let two = [
            Some(last(100.0, 100.0, 9_000.0)),
            Some(last(300.0, 150.0, 6_000.0)),
        ];
        let both = [star(140.0, 130.0, 9_000.0), star(340.0, 180.0, 6_000.0)];
        assert!(jumped(&two, &both, params));
    }
}

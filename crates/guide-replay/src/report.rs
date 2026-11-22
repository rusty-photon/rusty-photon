//! What a replay found, and the tolerance it is held to. See
//! `docs/crates/guide-replay.md` (The report).

use std::fmt;

use serde::Serialize;

/// What a replay is held to: stage G3's pass mark, in pixels of the
/// measured frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Tolerance {
    /// The largest standard deviation a guide star's deviation from its
    /// frame's offset may have about the star's own mean, on each axis.
    pub scatter_px: f64,
    /// The distance from a star's mean deviation at which a deviation is
    /// an outlier.
    pub outlier_px: f64,
    /// The largest share of deviations that may be outliers.
    pub outlier_fraction: f64,
    /// The smallest share of the frames that are not star-lost in which
    /// each guide star must be usable.
    pub min_usable_fraction: f64,
    /// The fewest frames that are not star-lost.
    pub min_clear_frames: usize,
}

impl Default for Tolerance {
    /// Stage G3's tolerance for an hour on rig2.
    fn default() -> Self {
        Self {
            scatter_px: 0.5,
            outlier_px: 2.0,
            outlier_fraction: 0.001,
            min_usable_fraction: 0.9,
            min_clear_frames: 1_000,
        }
    }
}

/// What a replay found of one guide star.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StarReport {
    /// Its index, from 0, highest signal-to-noise ratio first.
    pub star: usize,
    /// Where it was selected, when it was usable in the selection's frame.
    pub selected_at: Option<[f64; 2]>,
    /// The frames that are not star-lost.
    pub clear: usize,
    /// The frames among them in which it is usable.
    pub usable: usize,
    /// The frames among them in which it is not found.
    pub not_found: usize,
    /// The frames among them in which it is faint.
    pub faint: usize,
    /// The frames among them in which its flux changed too far.
    pub flux_changed: usize,
    /// The mean of its deviation from its frame's offset, on each axis:
    /// where it sat, in the selection's frame, apart from where the
    /// others did.
    pub mean_deviation_px: Option<[f64; 2]>,
    /// The standard deviation of its deviation about that mean, on each
    /// axis.
    pub scatter_px: Option<[f64; 2]>,
    /// Its deviation's largest distance from that mean.
    pub largest_deviation_px: Option<f64>,
}

impl StarReport {
    /// The share of the frames that are not star-lost in which it is
    /// usable.
    #[must_use]
    pub fn usable_fraction(&self) -> Option<f64> {
        let clear = u32::try_from(self.clear).ok().filter(|clear| *clear > 0)?;
        let usable = u32::try_from(self.usable).ok()?;
        Some(f64::from(usable) / f64::from(clear))
    }
}

/// A way a replay misses its tolerance.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "failure", rename_all = "snake_case")]
pub enum Failure {
    /// Frames were star-lost though a fresh selection finds stars in them:
    /// the measurement lost stars that were there.
    Lost {
        /// How many.
        frames: usize,
    },
    /// Too few frames were not star-lost.
    TooFewClear {
        /// How many were.
        frames: usize,
        /// How many the tolerance asks for.
        min: usize,
    },
    /// A guide star was usable in too few of the frames that are not
    /// star-lost.
    SeldomUsable {
        /// The star.
        star: usize,
        /// The share it was usable in.
        fraction: f64,
        /// The share the tolerance asks for.
        min: f64,
    },
    /// A guide star's deviation scatters too far on an axis.
    Scattered {
        /// The star.
        star: usize,
        /// Its scatter on each axis.
        scatter_px: [f64; 2],
        /// The largest the tolerance allows.
        max_px: f64,
    },
    /// Too many deviations are outliers.
    Outliers {
        /// How many are.
        outliers: usize,
        /// How many deviations there are.
        deviations: usize,
        /// The largest share the tolerance allows.
        max_fraction: f64,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost { frames } => write!(
                f,
                "{frames} star-lost frames hold stars a fresh selection finds"
            ),
            Self::TooFewClear { frames, min } => {
                write!(f, "{frames} frames are not star-lost, fewer than {min}")
            }
            Self::SeldomUsable {
                star,
                fraction,
                min,
            } => write!(
                f,
                "star {star} is usable in {:.1} % of the clear frames, fewer than {:.1} %",
                fraction * 100.0,
                min * 100.0
            ),
            Self::Scattered {
                star,
                scatter_px: [x, y],
                max_px,
            } => write!(
                f,
                "star {star} scatters by {x:.3} px in x and {y:.3} px in y, more than {max_px} px"
            ),
            Self::Outliers {
                outliers,
                deviations,
                max_fraction,
            } => write!(
                f,
                "{outliers} of {deviations} deviations are outliers, more than {:.2} %",
                max_fraction * 100.0
            ),
        }
    }
}

/// What a replay found, held to its tolerance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The frames the recording lists.
    pub frames: usize,
    /// The frames the recorder did not get.
    pub failed: usize,
    /// The frames before the first one with stars to select.
    pub before_selection: usize,
    /// The frames measured, the selection's frame included.
    pub measured: usize,
    /// The measured frames that are not star-lost.
    pub clear: usize,
    /// The star-lost frames in which a fresh selection finds no stars
    /// either: clouds.
    pub clouded: usize,
    /// The star-lost frames in which a fresh selection finds stars.
    pub lost: usize,
    /// Each guide star.
    pub stars: Vec<StarReport>,
    /// The deviations measured: one for each usable star in each frame
    /// that is not star-lost.
    pub deviations: usize,
    /// The deviations at or beyond the tolerance's outlier distance from
    /// their star's mean.
    pub outliers: usize,
    /// The tolerance it is held to.
    pub tolerance: Tolerance,
    /// The ways it misses the tolerance; none when it passes.
    pub failures: Vec<Failure>,
}

impl Report {
    /// Whether the replay is within its tolerance.
    #[must_use]
    pub const fn passes(&self) -> bool {
        self.failures.is_empty()
    }

    /// Holds the counts to the tolerance, and records how they miss it.
    pub fn judge(&mut self) {
        let tolerance = self.tolerance;
        let mut failures = Vec::new();
        if self.lost > 0 {
            failures.push(Failure::Lost { frames: self.lost });
        }
        if self.clear < tolerance.min_clear_frames {
            failures.push(Failure::TooFewClear {
                frames: self.clear,
                min: tolerance.min_clear_frames,
            });
        }
        for star in &self.stars {
            let fraction = star.usable_fraction().unwrap_or(0.0);
            if fraction < tolerance.min_usable_fraction {
                failures.push(Failure::SeldomUsable {
                    star: star.star,
                    fraction,
                    min: tolerance.min_usable_fraction,
                });
            }
            if let Some(scatter_px) = star.scatter_px {
                if scatter_px
                    .iter()
                    .any(|scatter| *scatter > tolerance.scatter_px)
                {
                    failures.push(Failure::Scattered {
                        star: star.star,
                        scatter_px,
                        max_px: tolerance.scatter_px,
                    });
                }
            }
        }
        let allowed = to_f64(self.deviations) * tolerance.outlier_fraction;
        if to_f64(self.outliers) > allowed {
            failures.push(Failure::Outliers {
                outliers: self.outliers,
                deviations: self.deviations,
                max_fraction: tolerance.outlier_fraction,
            });
        }
        self.failures = failures;
    }
}

impl fmt::Display for Report {
    /// The report as the command prints it: the counts, a line for each
    /// guide star, and whether it is within its tolerance.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} frames: {} failed, {} before the selection, {} measured",
            self.frames, self.failed, self.before_selection, self.measured
        )?;
        writeln!(
            f,
            "measured: {} not star-lost, {} clouded, {} lost",
            self.clear, self.clouded, self.lost
        )?;
        writeln!(
            f,
            "{} deviations, {} of them {} px or more from their star's mean",
            self.deviations, self.outliers, self.tolerance.outlier_px
        )?;
        for star in &self.stars {
            write!(
                f,
                "star {}: usable in {} of {}",
                star.star, star.usable, star.clear
            )?;
            match star.scatter_px {
                Some([x, y]) => writeln!(f, ", scatter {x:.3} px in x and {y:.3} px in y")?,
                None => writeln!(f)?,
            }
        }
        if self.passes() {
            writeln!(f, "within the tolerance")
        } else {
            self.failures
                .iter()
                .try_for_each(|failure| writeln!(f, "FAILS: {failure}"))
        }
    }
}

/// A count as a float; counts here stay far below 2³².
fn to_f64(count: usize) -> f64 {
    u32::try_from(count).map_or_else(|_| f64::from(u32::MAX), f64::from)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn star(index: usize, clear: usize, usable: usize, scatter: Option<[f64; 2]>) -> StarReport {
        StarReport {
            star: index,
            selected_at: Some([10.0, 20.0]),
            clear,
            usable,
            not_found: clear - usable,
            faint: 0,
            flux_changed: 0,
            mean_deviation_px: scatter.map(|_| [0.1, -0.2]),
            scatter_px: scatter,
            largest_deviation_px: scatter.map(|[x, y]| 3.0 * x.max(y)),
        }
    }

    fn report(adjust: impl FnOnce(&mut Report)) -> Report {
        let mut report = Report {
            frames: 1_200,
            failed: 0,
            before_selection: 0,
            measured: 1_200,
            clear: 1_200,
            clouded: 0,
            lost: 0,
            stars: vec![
                star(0, 1_200, 1_200, Some([0.2, 0.3])),
                star(1, 1_200, 1_100, Some([0.4, 0.5])),
            ],
            deviations: 2_300,
            outliers: 2,
            tolerance: Tolerance::default(),
            failures: Vec::new(),
        };
        adjust(&mut report);
        report.judge();
        report
    }

    #[test]
    fn a_replay_within_every_limit_passes() {
        let report = report(|_| {});
        assert_eq!(report.failures, []);
        assert!(report.passes());
    }

    #[test]
    fn a_star_lost_frame_that_still_has_stars_fails() {
        let report = report(|report| report.lost = 1);
        assert_eq!(report.failures, [Failure::Lost { frames: 1 }]);
        assert!(!report.passes());
    }

    #[test]
    fn too_few_clear_frames_fail() {
        let report = report(|report| report.clear = 999);
        assert_eq!(
            report.failures,
            [Failure::TooFewClear {
                frames: 999,
                min: 1_000
            }]
        );
    }

    #[test]
    fn a_star_usable_in_too_few_frames_fails() {
        let report = report(|report| {
            report.stars[1] = star(1, 1_000, 899, Some([0.1, 0.1]));
            report.stars.push(star(2, 0, 0, None));
        });
        assert_eq!(
            report.failures,
            [
                Failure::SeldomUsable {
                    star: 1,
                    fraction: 0.899,
                    min: 0.9
                },
                Failure::SeldomUsable {
                    star: 2,
                    fraction: 0.0,
                    min: 0.9
                }
            ]
        );
    }

    #[test]
    fn a_star_that_scatters_too_far_on_either_axis_fails() {
        for scatter in [[0.51, 0.1], [0.1, 0.51]] {
            let report = report(|report| report.stars[0].scatter_px = Some(scatter));
            assert_eq!(
                report.failures,
                [Failure::Scattered {
                    star: 0,
                    scatter_px: scatter,
                    max_px: 0.5
                }]
            );
        }
    }

    #[test]
    fn more_outliers_than_one_in_a_thousand_fail() {
        assert!(report(|report| report.outliers = 2).passes());
        let report = report(|report| report.outliers = 3);
        assert_eq!(
            report.failures,
            [Failure::Outliers {
                outliers: 3,
                deviations: 2_300,
                max_fraction: 0.001
            }]
        );
    }

    #[test]
    fn each_failure_says_what_missed() {
        let lines: Vec<String> = [
            Failure::Lost { frames: 2 },
            Failure::TooFewClear {
                frames: 10,
                min: 1_000,
            },
            Failure::SeldomUsable {
                star: 3,
                fraction: 0.5,
                min: 0.9,
            },
            Failure::Scattered {
                star: 1,
                scatter_px: [0.6, 0.25],
                max_px: 0.5,
            },
            Failure::Outliers {
                outliers: 5,
                deviations: 1_000,
                max_fraction: 0.001,
            },
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            lines,
            [
                "2 star-lost frames hold stars a fresh selection finds",
                "10 frames are not star-lost, fewer than 1000",
                "star 3 is usable in 50.0 % of the clear frames, fewer than 90.0 %",
                "star 1 scatters by 0.600 px in x and 0.250 px in y, more than 0.5 px",
                "5 of 1000 deviations are outliers, more than 0.10 %",
            ]
        );
    }

    #[test]
    fn the_printed_report_gives_the_counts_each_star_and_the_verdict() {
        let mut passing = report(|_| {});
        passing.stars[1].scatter_px = None;
        assert_eq!(
            passing.to_string(),
            "1200 frames: 0 failed, 0 before the selection, 1200 measured\n\
             measured: 1200 not star-lost, 0 clouded, 0 lost\n\
             2300 deviations, 2 of them 2 px or more from their star's mean\n\
             star 0: usable in 1200 of 1200, scatter 0.200 px in x and 0.300 px in y\n\
             star 1: usable in 1100 of 1200\n\
             within the tolerance\n"
        );
        let failing = report(|report| report.lost = 3);
        assert!(failing
            .to_string()
            .ends_with("FAILS: 3 star-lost frames hold stars a fresh selection finds\n"));
    }
}

//! The mount state record: where the driver last left the mount, kept
//! beside the config file so that a power-up connect after a restart of
//! the service can check the seed's assumption against it.
//!
//! See the design doc's
//! [§"The mount state record"](../../../../docs/services/star-adventurer-gti.md#the-mount-state-record).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manager::MountSnapshot;

/// What the driver last knew about the mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum MountStateRecord {
    /// Both axes stopped at these encoder ticks.
    AtRest { ra_ticks: i32, dec_ticks: i32 },
    /// An axis is running.
    Moving,
    /// The position is lost (design doc §Power cut while serving).
    Lost,
}

impl MountStateRecord {
    /// The record that `snapshot` and the lost flag describe.
    pub(super) const fn of(snapshot: &MountSnapshot, position_lost: bool) -> Self {
        if position_lost {
            Self::Lost
        } else if snapshot.ra.running() || snapshot.dec.running() {
            Self::Moving
        } else {
            Self::AtRest {
                ra_ticks: snapshot.ra.position_ticks,
                dec_ticks: snapshot.dec.position_ticks,
            }
        }
    }
}

/// Where the record for the config file at `config_path` lives: beside
/// it, named after it (`star-adventurer-gti.json` →
/// `star-adventurer-gti.mount-state.json`).
pub(super) fn record_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map_or_else(|| "star-adventurer-gti".into(), |s| s.to_string_lossy());
    config_path.with_file_name(format!("{stem}.mount-state.json"))
}

/// What reading the record found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReadRecord {
    /// No record yet: a first install.
    Absent,
    Record(MountStateRecord),
    /// A record that could not be read or does not parse, with why.
    Unreadable(String),
}

/// Read the record at `path`. Blocking file I/O: call it on the blocking
/// pool.
pub(super) fn read(path: &Path) -> ReadRecord {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(record) => ReadRecord::Record(record),
            Err(e) => ReadRecord::Unreadable(format!("{} does not parse: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReadRecord::Absent,
        Err(e) => ReadRecord::Unreadable(format!("{} cannot be read: {e}", path.display())),
    }
}

/// Replace the record at `path` atomically. Blocking file I/O: call it
/// on the blocking pool.
///
/// # Errors
///
/// Returns the step of the atomic write that failed.
pub(super) fn write(path: &Path, record: MountStateRecord) -> crate::error::Result<()> {
    let mut json = serde_json::to_string(&record)
        .map_err(|e| crate::error::StarAdvError::Config(format!("serialise record: {e}")))?;
    json.push('\n');
    super::park_persistence::write_atomically(path, &json)
}

/// What a fresh-power-up connect does with a named
/// `unpark_from_ap_position`, given the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PowerUpVerdict {
    /// Seed the configured pose: no record, or the mount came to rest
    /// there.
    Seed,
    /// Do not seed; the position is lost, for this reason.
    Lost(String),
}

/// Decide a fresh-power-up connect from the record. `seed` is the
/// configured pose's encoder ticks; `tolerance` is how far from them a
/// record still counts as at that pose.
pub(super) fn power_up_verdict(
    read: &ReadRecord,
    seed: (i32, i32),
    tolerance: i32,
) -> PowerUpVerdict {
    match read {
        ReadRecord::Absent => PowerUpVerdict::Seed,
        ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks,
            dec_ticks,
        }) => {
            let at_seed = ra_ticks.abs_diff(seed.0) <= tolerance.unsigned_abs()
                && dec_ticks.abs_diff(seed.1) <= tolerance.unsigned_abs();
            if at_seed {
                PowerUpVerdict::Seed
            } else {
                PowerUpVerdict::Lost(format!(
                    "the mount came to rest at ({ra_ticks}, {dec_ticks}) ticks, not at the \
                     power-up pose ({}, {})",
                    seed.0, seed.1
                ))
            }
        }
        ReadRecord::Record(MountStateRecord::Moving) => {
            PowerUpVerdict::Lost("the mount was moving when the driver last knew it".to_string())
        }
        ReadRecord::Record(MountStateRecord::Lost) => {
            PowerUpVerdict::Lost("the mount's position was already lost".to_string())
        }
        ReadRecord::Unreadable(why) => PowerUpVerdict::Lost(format!("the record {why}")),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use skywatcher_motor_protocol::{
        AxisStatus, Direction, InitFlags, ModeKind, MotionFlags, Speed,
    };
    use tokio::time::Instant;

    use super::*;
    use crate::manager::AxisSnapshot;

    const SEED: (i32, i32) = (-907_200, 725_760);

    fn axis(ticks: i32, running: bool) -> AxisSnapshot {
        AxisSnapshot {
            position_ticks: ticks,
            sampled_at: Some(Instant::now()),
            status: Some(AxisStatus {
                mode: ModeKind::Tracking,
                direction: Direction::Cw,
                speed: Speed::Slow,
                motion: MotionFlags {
                    running,
                    blocked: false,
                },
                init: InitFlags {
                    initialized: true,
                    level_switch_on: false,
                },
            }),
            ..AxisSnapshot::default()
        }
    }

    #[test]
    fn the_record_is_named_after_the_config_file_beside_it() {
        assert_eq!(
            record_path(Path::new("/etc/rp/star-adventurer-gti.json")),
            PathBuf::from("/etc/rp/star-adventurer-gti.mount-state.json")
        );
    }

    #[test]
    fn a_stopped_mount_records_its_ticks() {
        let snap = MountSnapshot {
            ra: axis(SEED.0, false),
            dec: axis(SEED.1, false),
        };
        assert_eq!(
            MountStateRecord::of(&snap, false),
            MountStateRecord::AtRest {
                ra_ticks: SEED.0,
                dec_ticks: SEED.1
            }
        );
    }

    #[test]
    fn one_running_axis_records_moving() {
        let snap = MountSnapshot {
            ra: axis(SEED.0, true),
            dec: axis(SEED.1, false),
        };
        assert_eq!(MountStateRecord::of(&snap, false), MountStateRecord::Moving);
    }

    #[test]
    fn a_lost_position_records_lost_whatever_the_axes_do() {
        let snap = MountSnapshot {
            ra: axis(0, false),
            dec: axis(0, false),
        };
        assert_eq!(MountStateRecord::of(&snap, true), MountStateRecord::Lost);
    }

    #[test]
    fn the_record_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.mount-state.json");
        let record = MountStateRecord::AtRest {
            ra_ticks: SEED.0,
            dec_ticks: SEED.1,
        };
        write(&path, record).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"state\":\"at_rest\",\"ra_ticks\":-907200,\"dec_ticks\":725760}\n"
        );
        assert_eq!(read(&path), ReadRecord::Record(record));
    }

    #[test]
    fn a_missing_record_reads_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read(&dir.path().join("config.mount-state.json")),
            ReadRecord::Absent
        );
    }

    #[test]
    fn a_record_that_does_not_parse_reads_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.mount-state.json");
        std::fs::write(&path, "{\"state\":\"parked\"}").unwrap();
        assert!(matches!(read(&path), ReadRecord::Unreadable(_)));
    }

    #[test]
    fn no_record_seeds() {
        assert_eq!(
            power_up_verdict(&ReadRecord::Absent, SEED, 10),
            PowerUpVerdict::Seed
        );
    }

    #[test]
    fn at_rest_at_the_seed_pose_within_tolerance_seeds() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: SEED.0 + 10,
            dec_ticks: SEED.1 - 10,
        });
        assert_eq!(power_up_verdict(&read, SEED, 10), PowerUpVerdict::Seed);
    }

    #[test]
    fn at_rest_just_beyond_the_tolerance_is_lost() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: SEED.0 + 11,
            dec_ticks: SEED.1,
        });
        assert!(matches!(
            power_up_verdict(&read, SEED, 10),
            PowerUpVerdict::Lost(_)
        ));
    }

    #[test]
    fn moving_lost_and_unreadable_records_are_lost() {
        for read in [
            ReadRecord::Record(MountStateRecord::Moving),
            ReadRecord::Record(MountStateRecord::Lost),
            ReadRecord::Unreadable("x does not parse".to_string()),
        ] {
            assert!(
                matches!(power_up_verdict(&read, SEED, 10), PowerUpVerdict::Lost(_)),
                "{read:?}"
            );
        }
    }
}

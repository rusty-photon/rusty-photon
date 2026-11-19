//! The mount state record: where the driver last left the mount, kept
//! beside the config file so that a power-up connect after a restart of
//! the service can check the seed's assumption against it, and so that a
//! power-up check a long cut left pending outlives the process.
//!
//! See the design doc's
//! [§"The mount state record"](../../../docs/services/star-adventurer-gti.md#the-mount-state-record).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, MutexGuard};

use crate::manager::MountSnapshot;
use crate::power::Position;

/// What the driver last knew about the mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MountStateRecord {
    /// Both axes stopped at these encoder ticks.
    AtRest { ra_ticks: i32, dec_ticks: i32 },
    /// An axis is running.
    Moving,
    /// The mount came to rest at these ticks, and its controller has
    /// restarted since after a long cut: no connect has run the power-up
    /// check yet (design doc §Power cut while serving).
    AwaitingCheck { ra_ticks: i32, dec_ticks: i32 },
    /// The position is lost (design doc §Power cut while serving).
    Lost,
}

impl MountStateRecord {
    /// The record that `snapshot` and `position` describe, or `None` when
    /// they say nothing to record: a power-up check is pending (the record
    /// is its evidence, and stays as it is), or an axis has no status
    /// since the last handshake, so whether it moves is not known. Only
    /// the restore writes [`Self::AwaitingCheck`].
    pub const fn of(snapshot: &MountSnapshot, position: Position) -> Option<Self> {
        match position {
            Position::Lost => Some(Self::Lost),
            Position::AwaitingCheck => None,
            Position::Known => {
                if snapshot.ra.status.is_none() || snapshot.dec.status.is_none() {
                    None
                } else if snapshot.ra.running() || snapshot.dec.running() {
                    Some(Self::Moving)
                } else {
                    Some(Self::AtRest {
                        ra_ticks: snapshot.ra.position_ticks,
                        dec_ticks: snapshot.dec.position_ticks,
                    })
                }
            }
        }
    }
}

/// Where the record for the config file at `config_path` lives: beside
/// it, named after it (`star-adventurer-gti.json` →
/// `star-adventurer-gti.mount-state.json`).
fn record_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map_or_else(|| "star-adventurer-gti".into(), |s| s.to_string_lossy());
    config_path.with_file_name(format!("{stem}.mount-state.json"))
}

/// The record's file, and the lock that orders every read and write of
/// it. The writer that follows the mount, the restore after a controller
/// restart and the power-up check each write what they decided from what
/// they read or hold; the lock keeps one's write from landing on a
/// decision another made from the record before it.
///
/// A holder never keeps the lock across a wire command: the restore holds
/// it inside the handshake, so a command waiting on that handshake would
/// wait on itself.
#[derive(Debug)]
pub struct MountStateStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl MountStateStore {
    /// The store for the record beside the config file at `config_path`.
    pub fn beside(config_path: &Path) -> Self {
        Self {
            path: record_path(config_path),
            lock: Mutex::new(()),
        }
    }

    /// Hold the record until the returned guard drops.
    pub async fn lock(&self) -> HeldRecord<'_> {
        HeldRecord {
            path: &self.path,
            _guard: self.lock.lock().await,
        }
    }
}

/// The record, held: no other holder reads or writes it meanwhile.
pub struct HeldRecord<'a> {
    path: &'a Path,
    _guard: MutexGuard<'a, ()>,
}

impl HeldRecord<'_> {
    /// Read the record, on the blocking pool.
    pub async fn read(&self) -> ReadRecord {
        let path = self.path.to_path_buf();
        tokio::task::spawn_blocking(move || read(&path))
            .await
            .unwrap_or_else(|e| ReadRecord::Unreadable(format!("read did not finish: {e}")))
    }

    /// Replace the record atomically, on the blocking pool.
    ///
    /// # Errors
    ///
    /// Returns why the write did not land.
    pub async fn write(&self, record: MountStateRecord) -> Result<(), String> {
        let path = self.path.to_path_buf();
        tokio::task::spawn_blocking(move || write(&path, record))
            .await
            .map_err(|e| format!("the write did not finish: {e}"))
            .and_then(|written| written.map_err(|e| e.to_string()))
    }
}

/// What reading the record found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadRecord {
    /// No record yet: a first install.
    Absent,
    Record(MountStateRecord),
    /// A record that could not be read or does not parse, with why.
    Unreadable(String),
}

/// Read the record at `path`. Blocking file I/O: call it on the blocking
/// pool.
fn read(path: &Path) -> ReadRecord {
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
fn write(path: &Path, record: MountStateRecord) -> crate::error::Result<()> {
    let mut json = serde_json::to_string(&record)
        .map_err(|e| crate::error::StarAdvError::Config(format!("serialise record: {e}")))?;
    json.push('\n');
    crate::mount_device::write_atomically(path, &json)
}

/// What a fresh-power-up connect does with a named
/// `unpark_from_ap_position`, given the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerUpVerdict {
    /// Seed the configured pose: no record, or the mount came to rest
    /// there.
    Seed,
    /// Do not seed; the position is lost, for this reason.
    Lost(String),
}

/// The ticks the record says the mount came to rest at, or why it places
/// the mount nowhere a power-up check could seed from.
pub fn resting_ticks(read: &ReadRecord) -> Result<(i32, i32), String> {
    match read {
        ReadRecord::Absent => Err("no record says where the mount came to rest".to_string()),
        ReadRecord::Record(
            MountStateRecord::AtRest {
                ra_ticks,
                dec_ticks,
            }
            | MountStateRecord::AwaitingCheck {
                ra_ticks,
                dec_ticks,
            },
        ) => Ok((*ra_ticks, *dec_ticks)),
        ReadRecord::Record(MountStateRecord::Moving) => {
            Err("the mount was moving when the driver last knew it".to_string())
        }
        ReadRecord::Record(MountStateRecord::Lost) => {
            Err("the mount's position was already lost".to_string())
        }
        ReadRecord::Unreadable(why) => Err(format!("the record {why}")),
    }
}

/// What a long cut makes of the record: the power-up check pending at the
/// ticks a record that says at rest holds, or why the position is lost
/// now. Only a mount at rest leaves the check's outcome to the configured
/// pose; for any other record the check loses the position whatever the
/// pose, and a missing one cannot be a first install after a restart the
/// driver saw.
pub fn after_long_cut(read: &ReadRecord) -> Result<MountStateRecord, String> {
    resting_ticks(read).map(|(ra_ticks, dec_ticks)| MountStateRecord::AwaitingCheck {
        ra_ticks,
        dec_ticks,
    })
}

/// Decide a fresh-power-up connect from the record. `seed` is the
/// configured pose's encoder ticks; `tolerance` is how far from them a
/// record still counts as at that pose. `after_restart` says the check
/// follows a restart the driver saw while serving: a missing record then
/// cannot be a first install, and seeds nothing.
pub fn power_up_verdict(
    read: &ReadRecord,
    seed: (i32, i32),
    tolerance: i32,
    after_restart: bool,
) -> PowerUpVerdict {
    if !after_restart && *read == ReadRecord::Absent {
        return PowerUpVerdict::Seed;
    }
    match resting_ticks(read) {
        Err(reason) => PowerUpVerdict::Lost(reason),
        Ok((ra_ticks, dec_ticks)) => {
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

    /// `pkg/doctor.toml` declares the record as this service's state file,
    /// so doctor never flags it as a misnamed config (docs/services/doctor.md
    /// §The derived catalog); the name must be the one the driver writes
    /// beside its packaged config.
    #[test]
    fn the_doctor_catalog_declares_the_record_as_the_state_file() {
        let meta =
            rusty_photon_server_config::doctor_toml::parse(include_str!("../pkg/doctor.toml"))
                .unwrap();
        let record = record_path(Path::new("star-adventurer-gti.json"));
        assert_eq!(
            meta.state_file.as_deref(),
            record.file_name().and_then(|name| name.to_str())
        );
    }

    #[test]
    fn a_stopped_mount_records_its_ticks() {
        let snap = MountSnapshot {
            ra: axis(SEED.0, false),
            dec: axis(SEED.1, false),
        };
        assert_eq!(
            MountStateRecord::of(&snap, Position::Known),
            Some(MountStateRecord::AtRest {
                ra_ticks: SEED.0,
                dec_ticks: SEED.1
            })
        );
    }

    #[test]
    fn one_running_axis_records_moving() {
        let snap = MountSnapshot {
            ra: axis(SEED.0, true),
            dec: axis(SEED.1, false),
        };
        assert_eq!(
            MountStateRecord::of(&snap, Position::Known),
            Some(MountStateRecord::Moving)
        );
    }

    #[test]
    fn a_lost_position_records_lost_whatever_the_axes_do() {
        let snap = MountSnapshot {
            ra: axis(0, false),
            dec: axis(0, false),
        };
        assert_eq!(
            MountStateRecord::of(&snap, Position::Lost),
            Some(MountStateRecord::Lost)
        );
    }

    #[test]
    fn a_pending_power_up_check_records_nothing() {
        let snap = MountSnapshot {
            ra: axis(0, false),
            dec: axis(0, false),
        };
        assert_eq!(MountStateRecord::of(&snap, Position::AwaitingCheck), None);
    }

    #[test]
    fn an_axis_without_a_status_records_nothing() {
        let snap = MountSnapshot {
            ra: AxisSnapshot {
                position_ticks: SEED.0,
                sampled_at: Some(Instant::now()),
                ..AxisSnapshot::default()
            },
            dec: axis(SEED.1, false),
        };
        assert_eq!(MountStateRecord::of(&snap, Position::Known), None);
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
    fn a_record_awaiting_the_check_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.mount-state.json");
        let record = MountStateRecord::AwaitingCheck {
            ra_ticks: SEED.0,
            dec_ticks: SEED.1,
        };
        write(&path, record).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"state\":\"awaiting_check\",\"ra_ticks\":-907200,\"dec_ticks\":725760}\n"
        );
        assert_eq!(read(&path), ReadRecord::Record(record));
    }

    #[tokio::test]
    async fn the_store_keeps_the_record_beside_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MountStateStore::beside(&dir.path().join("config.json"));
        let held = store.lock().await;
        assert_eq!(held.read().await, ReadRecord::Absent);
        held.write(MountStateRecord::Moving).await.unwrap();
        assert_eq!(
            held.read().await,
            ReadRecord::Record(MountStateRecord::Moving)
        );
        assert!(dir.path().join("config.mount-state.json").exists());
    }

    #[tokio::test]
    async fn a_write_the_store_cannot_land_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let store = MountStateStore::beside(&dir.path().join("missing").join("config.json"));
        let err = store
            .lock()
            .await
            .write(MountStateRecord::Lost)
            .await
            .unwrap_err();
        assert!(err.contains("create temp file in"), "{err}");
    }

    #[test]
    fn a_long_cut_holds_the_check_at_the_ticks_a_record_at_rest_holds() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: 1_000,
            dec_ticks: 2_000,
        });
        assert_eq!(
            after_long_cut(&read),
            Ok(MountStateRecord::AwaitingCheck {
                ra_ticks: 1_000,
                dec_ticks: 2_000
            })
        );
    }

    #[test]
    fn a_long_cut_keeps_a_check_already_pending() {
        let pending = MountStateRecord::AwaitingCheck {
            ra_ticks: 1_000,
            dec_ticks: 2_000,
        };
        assert_eq!(after_long_cut(&ReadRecord::Record(pending)), Ok(pending));
    }

    #[test]
    fn a_long_cut_loses_the_position_unless_the_record_says_at_rest() {
        for read in [
            ReadRecord::Absent,
            ReadRecord::Record(MountStateRecord::Moving),
            ReadRecord::Record(MountStateRecord::Lost),
            ReadRecord::Unreadable("x does not parse".to_string()),
        ] {
            assert!(after_long_cut(&read).is_err(), "{read:?}");
        }
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
            power_up_verdict(&ReadRecord::Absent, SEED, 10, false),
            PowerUpVerdict::Seed
        );
    }

    #[test]
    fn no_record_after_a_restart_the_driver_saw_is_lost() {
        assert!(matches!(
            power_up_verdict(&ReadRecord::Absent, SEED, 10, true),
            PowerUpVerdict::Lost(_)
        ));
    }

    #[test]
    fn at_rest_at_the_seed_pose_after_a_restart_still_seeds() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: SEED.0,
            dec_ticks: SEED.1,
        });
        assert_eq!(
            power_up_verdict(&read, SEED, 10, true),
            PowerUpVerdict::Seed
        );
    }

    #[test]
    fn at_rest_at_the_seed_pose_within_tolerance_seeds() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: SEED.0 + 10,
            dec_ticks: SEED.1 - 10,
        });
        assert_eq!(
            power_up_verdict(&read, SEED, 10, false),
            PowerUpVerdict::Seed
        );
    }

    #[test]
    fn at_rest_just_beyond_the_tolerance_is_lost() {
        let read = ReadRecord::Record(MountStateRecord::AtRest {
            ra_ticks: SEED.0 + 11,
            dec_ticks: SEED.1,
        });
        assert!(matches!(
            power_up_verdict(&read, SEED, 10, false),
            PowerUpVerdict::Lost(_)
        ));
    }

    #[test]
    fn a_pending_check_at_the_seed_pose_seeds() {
        let read = ReadRecord::Record(MountStateRecord::AwaitingCheck {
            ra_ticks: SEED.0,
            dec_ticks: SEED.1,
        });
        assert_eq!(
            power_up_verdict(&read, SEED, 10, true),
            PowerUpVerdict::Seed
        );
    }

    #[test]
    fn a_pending_check_away_from_the_seed_pose_is_lost() {
        let read = ReadRecord::Record(MountStateRecord::AwaitingCheck {
            ra_ticks: SEED.0 + 11,
            dec_ticks: SEED.1,
        });
        assert!(matches!(
            power_up_verdict(&read, SEED, 10, true),
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
                matches!(
                    power_up_verdict(&read, SEED, 10, false),
                    PowerUpVerdict::Lost(_)
                ),
                "{read:?}"
            );
        }
    }
}

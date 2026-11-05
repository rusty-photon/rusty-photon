//! Per-connection task that follows the controller's power for the
//! connected device: it drops what a power cut ended, and keeps the mount
//! state record current.
//!
//! See the design doc's
//! [§"Power cut while serving"](../../../../docs/services/star-adventurer-gti.md#power-cut-while-serving)
//! and [§"The mount state record"](../../../../docs/services/star-adventurer-gti.md#the-mount-state-record).

use std::time::Duration;

use tokio::time::interval;
use tracing::{debug, warn};

use super::mount_state::{self, MountStateRecord};
use super::{MountDevice, PulseGuiding};
use crate::power::Position;

impl MountDevice {
    /// Drop what a power cut ended. The restarted firmware has stopped
    /// both axes, so the slew or park in flight is over (emptying the
    /// slot makes its watcher stand down), every guide pulse is over, and
    /// tracking is off until a client turns it on again. A position that
    /// is not known also leaves the frame unanchored, without a park
    /// target, and not parked.
    pub(super) async fn drop_what_the_cut_ended(&self) {
        let _axes = self.axis_ownership.lock().await;
        self.slew_in_progress.clear();
        let lost = !self.manager.power().position_known();
        let mut s = self.state.write().await;
        s.tracking_requested = false;
        s.pulse_guiding = PulseGuiding::IDLE;
        if lost {
            s.frame_anchored = false;
            s.park_ra_ticks = None;
            s.park_dec_ticks = None;
            s.at_park = false;
        }
        drop(s);
        debug!(lost, "dropped the device state the power cut ended");
    }

    /// The power-up check for a long cut that ended while a client is
    /// connected: what a connect after it would decide. The seed reads the
    /// mount state record and seeds the configured pose or loses the
    /// position, and the park target follows from that.
    pub(super) async fn check_power_up_while_connected(&self) {
        let cfg = match self.read_connect_config().await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("power-up check could not read the configuration: {e}");
                self.manager.power().mark_position_lost();
                return;
            }
        };
        let seeded = self
            .with_session(async |session| {
                self.seed_after_connect(session, cfg.unpark_from_ap_position)
                    .await
            })
            .await;
        if let Err(e) = seeded {
            warn!("power-up check failed, so the position is lost: {e}");
            self.manager.power().mark_position_lost();
        }
        if let Err(e) = self.load_park_target_after_connect(&cfg).await {
            warn!("power-up check could not load the park target: {e}");
        }
    }
}

/// Spawn the task for one connection. It ends when the device's session
/// closes.
pub(super) fn spawn_power_watch(device: MountDevice, polling_interval: Duration) {
    let record_path = device
        .config_file_path
        .as_deref()
        .map(mount_state::record_path);
    let mut restarts = device.manager.power().subscribe_restarts();
    // A restart found before this connection is no concern of its own:
    // the connect read the lost flag, and a disconnected device holds no
    // slew, pulse or tracking.
    restarts.mark_unchanged();
    tokio::spawn(async move {
        let mut ticker = interval(polling_interval);
        let mut written: Option<MountStateRecord> = None;
        loop {
            tokio::select! {
                changed = restarts.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    device.drop_what_the_cut_ended().await;
                    if device.manager.power().position() == Position::AwaitingCheck {
                        device.check_power_up_while_connected().await;
                    }
                }
                _ = ticker.tick() => {}
            }
            if device.session.read().await.is_none() {
                debug!("power watch: session closed, exiting");
                return;
            }
            let Some(path) = record_path.as_ref() else {
                continue;
            };
            // While the transport reconnects the snapshot is stale.
            if !device.manager.is_available() {
                continue;
            }
            let Some(record) = MountStateRecord::of(
                &device.manager.snapshot().await,
                device.manager.power().position(),
            ) else {
                continue;
            };
            if written == Some(record) {
                continue;
            }
            let target = path.clone();
            let result =
                tokio::task::spawn_blocking(move || mount_state::write(&target, record)).await;
            match result {
                Ok(Ok(())) => debug!(?record, "mount state record written"),
                Ok(Err(e)) => warn!(?record, "mount state record not written: {e}"),
                Err(e) => warn!(?record, "mount state record write did not finish: {e}"),
            }
            // One attempt per change: a failing write warns once, not on
            // every tick.
            written = Some(record);
        }
    });
}

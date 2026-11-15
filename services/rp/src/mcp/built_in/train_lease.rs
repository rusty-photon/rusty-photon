//! Train lease tool category: `acquire_train_lease` and
//! `release_train_lease` (rp.md § Train Lease).
//!
//! The acquire reserves at once in the lease book, then takes the five
//! things the lease holds, in the contract's order: the train's own
//! camera's slot, the guider's camera lease for a guiding train, the
//! slots of the devices the lease moves or holds still, the slots of the
//! cameras it drains, and the motion gate shared. Each wait races the
//! call's cancellation and the lease's own end (a release, the safety
//! transition, the guider's camera lease lost), and every slot goes into
//! the lease as soon as it is taken, so the lease's keeper gives back
//! whatever the acquire got whichever way it ends.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::{tool_error, tool_success};
use super::guider::DevicesCheck;
use crate::camera_lease;
use crate::equipment::DeviceSlot;
use crate::train_lease::{
    self, Lease, LeaseEnd, LeasePlan, DEFAULT_MAX_DURATION, MAX_DURATION_CEILING,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AcquireTrainLeaseParams {
    /// The optical train the training run is on.
    pub train_id: String,
    /// A fresh id of the caller's choosing, 1 to 128 printable ASCII
    /// characters without spaces (a UUID will do). Pass it on every
    /// `capture`, `move_focuser` and `set_filter` of the run, and to
    /// `release_train_lease`.
    pub lease_id: String,
    /// The lease's life from its grant, as a humantime string (e.g.
    /// `"5m"`). Omitted means 5 minutes; at most 10.
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub max_duration: Option<Duration>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReleaseTrainLeaseParams {
    /// The id the lease was acquired, or is being acquired, under.
    pub lease_id: String,
}

#[tool_router(router = tool_router_train_lease, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Take a train lease for a training run on an optical train, under a lease_id you name: the train's camera slots (its own camera, and every camera behind a device the lease moves), the slots of the devices the run moves (the terminal focuser, every filter wheel) or must leave still (every other focuser, every rotator), the guider's camera lease on a guiding train when a guider is configured, and the mount motion gate shared. Pass lease_id on every capture, move_focuser and set_filter of the run; calls that name no lease wait for it to end, and a slew or dither waits too. Ends at release_train_lease, after 60 s with no call under it, at ends_by (max_duration from the grant, default 5m, at most 10m), when the guider's camera lease is lost, on the safety transition, or at rp shutdown. An overlapping lease or auto_focus sweep is refused at once, and a guiding train whose guider is not idle, or exposes on another camera, is refused before anything moves. See docs/services/rp.md § Train Lease"
    )]
    pub(crate) async fn acquire_train_lease(
        &self,
        Parameters(params): Parameters<AcquireTrainLeaseParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.acquire_train_lease_inner(params, &cancel).await
    }

    /// Body of the `acquire_train_lease` MCP tool, split out so unit
    /// tests can pass a never-cancelled handle without constructing a
    /// real rmcp `RequestContext`.
    pub(crate) async fn acquire_train_lease_inner(
        &self,
        params: AcquireTrainLeaseParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(e) = train_lease::check_lease_id(&params.lease_id) {
            return Ok(tool_error!("acquire_train_lease: {}", e));
        }
        let max_duration = params.max_duration.unwrap_or(DEFAULT_MAX_DURATION);
        if max_duration.is_zero() || max_duration > MAX_DURATION_CEILING {
            return Ok(tool_error!(
                "acquire_train_lease: max_duration must be more than 0 s and at most {}, got {}",
                humantime::format_duration(MAX_DURATION_CEILING),
                humantime::format_duration(max_duration)
            ));
        }
        let plan = match LeasePlan::for_train(&self.trains, &params.train_id) {
            Ok(plan) => plan,
            Err(e) => return Ok(tool_error!("acquire_train_lease: {}", e)),
        };
        let acquiring = match self.leases.reserve(&params.lease_id, plan) {
            Ok(acquiring) => acquiring,
            Err(e) => return Ok(tool_error!("acquire_train_lease: {}", e)),
        };
        let camera_lease = match self.take_lease_holdings(acquiring.lease(), cancel).await {
            Ok(camera_lease) => camera_lease,
            Err(e) => {
                acquiring.lease().end(LeaseEnd::AcquireFailed);
                return Ok(tool_error!("acquire_train_lease: {}", e));
            }
        };
        if let Err(end) = acquiring.lease().grant(max_duration) {
            return Ok(tool_error!(
                "acquire_train_lease: lease '{}' ended before it was granted: {}",
                acquiring.lease().id,
                end.why()
            ));
        }
        let ends_by = chrono::TimeDelta::from_std(max_duration)
            .ok()
            .and_then(|life| chrono::Utc::now().checked_add_signed(life))
            .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        let plan = &acquiring.lease().plan;
        let granted = tool_success!({
            "lease_id": acquiring.lease().id,
            "train_id": plan.train_id,
            "focuser_id": plan.focuser_id,
            "filter_wheel_ids": plan.filter_wheel_ids,
            "held_focuser_ids": plan.held_focuser_ids,
            "held_rotator_ids": plan.held_rotator_ids,
            "cameras": plan.cameras().collect::<Vec<_>>(),
            "camera_lease": camera_lease,
            "ends_by": ends_by,
        });
        drop(acquiring);
        Ok(granted)
    }

    #[tool(
        description = "End a train lease, or cancel the acquire still waiting for it. Never an error: released is false for an id that holds nothing, and an id rp has not seen is remembered for an hour so a later acquire under it is refused. See docs/services/rp.md § Train Lease"
    )]
    pub(crate) async fn release_train_lease(
        &self,
        Parameters(params): Parameters<ReleaseTrainLeaseParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let released = if train_lease::check_lease_id(&params.lease_id).is_ok() {
            self.leases.release(&params.lease_id)
        } else {
            debug!(lease_id = ?params.lease_id, "release of an id no acquire could have named");
            false
        };
        Ok(tool_success!({ "released": released }))
    }
}

impl McpHandler {
    /// The acquire's five steps (rp.md § Train Lease). `Ok(true)` when it
    /// took the guider's camera lease.
    async fn take_lease_holdings(
        &self,
        lease: &Arc<Lease>,
        cancel: &Cancel,
    ) -> Result<bool, String> {
        let plan = &lease.plan;
        let camera = self
            .equipment
            .find_camera(&plan.camera_id)
            .ok_or_else(|| format!("camera not found: {}", plan.camera_id))?;

        // 1. The train's own camera's slot.
        lease.hold(lease_wait(lease, cancel, camera.capture_slot()).await?);

        // 2. The guider's camera lease, before any wait on another
        // camera or device or on the gate: it is the step that refuses
        // at once.
        let camera_lease = if plan.guiding {
            self.take_camera_lease(lease, cancel).await?
        } else {
            false
        };

        // 3. The devices' slots, in device-id order.
        let mut devices = plan
            .devices()
            .map(|(kind, id)| self.device_slot(kind, id).map(|slot| (kind, id, slot)))
            .collect::<Result<Vec<_>, _>>()?;
        devices.sort_by(|a, b| a.1.cmp(b.1));
        for (kind, id, slot) in devices {
            lease.hold(lease_wait(lease, cancel, slot.take(kind, id)).await?);
        }

        // 4. The drain: every other camera's slot, in camera-id order.
        for camera_id in &plan.drained_camera_ids {
            let drained = self
                .equipment
                .find_camera(camera_id)
                .ok_or_else(|| format!("camera not found: {camera_id}"))?;
            lease.hold(lease_wait(lease, cancel, drained.capture_slot()).await?);
        }

        // 5. The mount-motion lease: the gate, shared, after the slots.
        lease.hold_permit(lease_wait(lease, cancel, self.motion_gate.shared()).await?);
        Ok(camera_lease)
    }

    /// Step 2 for a guiding train: the guider's camera must be the
    /// train's, and the guider must grant its camera lease. `Ok(false)`
    /// with no guider configured, when nothing else exposes on the
    /// train's camera and the step is skipped.
    async fn take_camera_lease(&self, lease: &Arc<Lease>, cancel: &Cancel) -> Result<bool, String> {
        let Some(guider) = self.guider.as_ref() else {
            debug!(lease_id = %lease.id, "no guider configured: the lease takes no camera lease");
            return Ok(false);
        };
        let check = self.check_guider_devices(guider.as_ref(), DevicesCheck::CameraLease);
        lease_wait(lease, cancel, check).await??;
        let token = camera_lease::new_token();
        // Recorded before the guider is asked, so the lease's keeper
        // releases a camera lease whose answer never came.
        lease.record_camera_lease(Arc::clone(guider), token.clone());
        lease_wait(
            lease,
            cancel,
            camera_lease::acquire(guider.as_ref(), &token),
        )
        .await??;
        lease.camera_lease_granted();
        Ok(true)
    }

    /// The slot of a device a lease moves or holds still.
    fn device_slot(&self, kind: &str, id: &str) -> Result<&DeviceSlot, String> {
        let slot = match kind {
            "focuser" => self.equipment.find_focuser(id).map(|entry| &entry.slot),
            "filter wheel" => self
                .equipment
                .find_filter_wheel(id)
                .map(|entry| &entry.slot),
            _ => self.equipment.find_rotator(id).map(|entry| &entry.slot),
        };
        slot.ok_or_else(|| format!("{kind} not found: {id}"))
    }
}

/// One of the acquire's waits, raced against the call's cancellation and
/// the lease's own end.
async fn lease_wait<T>(
    lease: &Lease,
    cancel: &Cancel,
    wait: impl Future<Output = T>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(cancel.error()),
        () = lease.ending().cancelled() => Err(format!(
            "lease '{}' ended before it was granted: {}",
            lease.id,
            lease.end_reason().map_or_else(|| "it ended".to_string(), |end| end.why())
        )),
        value = wait => Ok(value),
    }
}

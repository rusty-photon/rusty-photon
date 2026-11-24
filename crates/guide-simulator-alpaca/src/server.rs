//! The two servers, and what a scenario scripts through them. See
//! `docs/crates/guide-simulator-alpaca.md` (Two servers, one simulation;
//! What a scenario scripts).

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use guide_simulator::{
    ConfigError, FrameTruth, MountError, PierSide, SimTime, Simulator, SkyOffset,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::camera::{self, SimCamera};
use crate::config::{Device, DevicesConfig, ExposureRecord, FrameFault, PulseRecord};
use crate::rig::{Rig, StuckPulses};
use crate::telescope::SimTelescope;

/// Why the devices did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The simulation cannot model its configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// `camera.max_bin` is 0.
    #[error("camera.max_bin must be at least 1")]
    MaxBin,
    /// A server could not bind its port.
    #[error("the {device:?} server could not bind: {source}")]
    Bind {
        /// Whose server.
        device: Device,
        /// What the system said.
        source: io::Error,
    },
}

/// One server, running until it is stopped or dropped.
#[derive(Debug)]
struct Served {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<io::Result<()>>,
}

/// The camera and the mount, served over one simulation. Dropping it
/// stops both servers.
#[derive(Debug)]
pub struct SimDevices {
    rig: Arc<Rig>,
    camera: Served,
    mount: Served,
}

impl SimDevices {
    /// Starts the simulation and serves its camera and its mount, each
    /// not connected.
    ///
    /// # Errors
    ///
    /// [`StartError`] when the configuration cannot be simulated or a
    /// server cannot bind.
    pub async fn start(config: DevicesConfig) -> Result<Self, StartError> {
        if config.camera.max_bin == 0 {
            return Err(StartError::MaxBin);
        }
        let sim = Simulator::new(config.sim)?;
        let rig = Arc::new(Rig::new(sim, config.camera, config.mount));
        let mut camera = Server::new(CargoServerInfo!());
        camera.devices.register(SimCamera::new(Arc::clone(&rig)));
        let camera = serve(Device::Camera, &rig, camera).await?;
        let mut mount = Server::new(CargoServerInfo!());
        mount.devices.register(SimTelescope::new(Arc::clone(&rig)));
        let mount = serve(Device::Mount, &rig, mount).await?;
        debug!(camera = %camera.addr, mount = %mount.addr, "simulated devices serving");
        Ok(Self { rig, camera, mount })
    }

    /// The camera's server, as a device's `alpaca_url`.
    #[must_use]
    pub fn camera_url(&self) -> String {
        format!("http://{}", self.camera.addr)
    }

    /// The mount's server, as a device's `alpaca_url`.
    #[must_use]
    pub fn mount_url(&self) -> String {
        format!("http://{}", self.mount.addr)
    }

    /// The simulation's time now.
    #[must_use]
    pub fn now(&self) -> SimTime {
        self.rig.now()
    }

    /// Sets `device`'s `Connected`. Connecting the camera starts it at
    /// the full frame at bin 1, with no frame.
    pub fn set_connected(&self, device: Device, connected: bool) {
        self.rig.with(|state| match device {
            Device::Camera => camera::connect(state, connected),
            Device::Mount => state.mount.connected = connected,
        });
    }

    /// Makes every request to `device`'s server answer `503`, or answer
    /// again.
    pub fn set_reachable(&self, device: Device, reachable: bool) {
        self.rig.with(|state| match device {
            Device::Camera => state.camera.reachable = reachable,
            Device::Mount => state.mount.reachable = reachable,
        });
    }

    /// Makes the next `count` exposures fail with `fault`, or every one
    /// while `count` is `None`.
    pub fn fail_frames(&self, fault: FrameFault, count: Option<usize>) {
        self.rig.with(|state| state.camera.faults.set(fault, count));
    }

    /// Lets every exposure succeed again.
    pub fn clear_frame_faults(&self) {
        self.rig.with(|state| state.camera.faults.clear());
    }

    /// Slews the mount by `by` over `duration`, ending on `pier_side`.
    ///
    /// # Errors
    ///
    /// [`MountError::Parked`] while parked.
    pub fn slew(
        &self,
        by: SkyOffset,
        duration: Duration,
        pier_side: PierSide,
    ) -> Result<(), MountError> {
        self.rig
            .with(|state| state.sim.mount_mut().slew(by, duration, pier_side))
    }

    /// Parks the mount.
    pub fn park(&self) {
        self.rig.with(|state| state.sim.mount_mut().park());
    }

    /// Turns the mount's tracking on or off.
    ///
    /// # Errors
    ///
    /// [`MountError::Parked`] when tracking is turned on while parked.
    pub fn set_tracking(&self, on: bool) -> Result<(), MountError> {
        self.rig
            .with(|state| state.sim.mount_mut().set_tracking(on))
    }

    /// Sets the mount's `CanPulseGuide`.
    pub fn set_can_pulse_guide(&self, can: bool) {
        self.rig.with(|state| state.mount.can_pulse_guide = can);
    }

    /// Makes pulses started from now on move each axis by `ra` and `dec`
    /// times what they should: below 1 a mount that falls behind its
    /// pulses, below 0 one that moves the wrong way.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a factor is not finite.
    pub fn set_rate_factors(&self, ra: f64, dec: f64) -> Result<(), ConfigError> {
        self.rig
            .with(|state| state.sim.mount_mut().set_rate_factors(ra, dec))
    }

    /// Keeps `IsPulseGuiding` true from the next pulse the mount accepts,
    /// past its end, until it is released; or releases it, so that it
    /// ends with its pulse again.
    pub fn set_pulses_stuck(&self, stuck: bool) {
        self.rig.with(|state| {
            state.mount.stuck_pulses = if stuck {
                StuckPulses::Armed
            } else {
                StuckPulses::Off
            };
        });
    }

    /// Every pulse the mount accepted, the earliest first.
    #[must_use]
    pub fn pulses(&self) -> Vec<PulseRecord> {
        self.rig.with(|state| state.pulses.clone())
    }

    /// Every exposure the camera started, the earliest first.
    #[must_use]
    pub fn exposures(&self) -> Vec<ExposureRecord> {
        self.rig.with(|state| state.exposures.clone())
    }

    /// How many times the camera was asked to abort its exposure, while
    /// connected, whether or not it could.
    #[must_use]
    pub fn aborts(&self) -> usize {
        self.rig.with(|state| state.aborts)
    }

    /// The truth of the latest frame rendered.
    #[must_use]
    pub fn last_truth(&self) -> Option<FrameTruth> {
        self.rig.with(|state| state.last_truth.clone())
    }

    /// Stops both servers and waits for them to end.
    pub async fn stop(self) {
        for served in [self.camera, self.mount] {
            // A server that has ended already has dropped its receiver.
            served.shutdown.send(()).ok();
            match served.task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => debug!(%error, "a simulated device's server ended with an error"),
                Err(error) => debug!(%error, "a simulated device's server task failed"),
            }
        }
    }
}

/// Serves `server` on a loopback port the system picks, answering `503`
/// to everything while `device` is not reachable.
async fn serve(device: Device, rig: &Arc<Rig>, server: Server) -> Result<Served, StartError> {
    let bind = |source| StartError::Bind { device, source };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(bind)?;
    let addr = listener.local_addr().map_err(bind)?;
    let gate = Arc::clone(rig);
    let app = Router::new()
        .fallback_service(server.into_service())
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let rig = Arc::clone(&gate);
            async move { answer(&rig, device, request, next).await }
        }));
    let (shutdown, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                // A dropped sender stops the server as a sent signal does.
                stopped.await.ok();
            })
            .await
    });
    Ok(Served {
        addr,
        shutdown,
        task,
    })
}

/// Passes `request` on while `device` is reachable, and answers `503`
/// with no Alpaca body otherwise.
async fn answer(rig: &Rig, device: Device, request: Request, next: Next) -> Response {
    if rig.reachable(device) {
        next.run(request).await
    } else {
        debug!(?device, uri = %request.uri(), "simulated server not answering, as scripted");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the simulated device's server is not answering",
        )
            .into_response()
    }
}

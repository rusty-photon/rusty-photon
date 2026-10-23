#![allow(dead_code)]
//! BDD test world for the polar-align service.
//!
//! Holds the external processes (`OmniSim`, rp, polar-align) plus the
//! in-process plate-solver stub whose choreographed solves inject a
//! known axis error. The shared harness types come from
//! `bdd_infra::rp_harness`; everything below is per-scenario
//! accumulator state.

use bdd_infra::rp_harness::{PlateSolverStub, RpConfigBuilder};
use bdd_infra::tls_auth::{TlsAuthSmokeWorld, TlsAuthState};
use bdd_infra::ServiceHandle;
use cucumber::World;
use serde_json::Value;

/// The observing site every scenario shares — must match the site in
/// the polar-align service config AND the site used to choreograph
/// the stub's solves, or the recovered error cannot match the
/// injected one.
pub const SITE_LATITUDE_DEG: f64 = 48.0;
pub const SITE_LONGITUDE_DEG: f64 = -122.8;

/// `OmniSim`'s default telescope-profile site. Scenarios that give *rp*
/// a site block must use this one: rp hard-errors on mount connect
/// when its configured site differs from the mount's reported site by
/// more than 0.01°, and mutating `OmniSim`'s site instead would leak
/// into other suites (the write outlives the scenario — see
/// `OmniSimHandle::set_telescope_site`).
pub const OMNISIM_SITE_LATITUDE_DEG: f64 = 51.0786;
pub const OMNISIM_SITE_LONGITUDE_DEG: f64 = -0.2944;

#[derive(Default, World, derive_more::Debug)]
#[debug("PolarAlignWorld {{ .. }}")]
pub struct PolarAlignWorld {
    // --- Infrastructure handles ---
    pub omnisim: Option<bdd_infra::rp_harness::OmniSimHandle>,
    pub rp: Option<ServiceHandle>,
    pub polar_align: Option<ServiceHandle>,
    pub plate_solver_stub: Option<PlateSolverStub>,

    // --- rp config building ---
    pub cameras: Vec<bdd_infra::rp_harness::CameraConfig>,
    pub mount: Option<bdd_infra::rp_harness::MountConfig>,
    pub plate_solver: Option<bdd_infra::rp_harness::PlateSolverConfig>,
    pub plugin_configs: Vec<Value>,
    /// rp's `site` block, absent by default. Scenarios exercising
    /// site-from-rp sourcing set it (to `OmniSim`'s default site — see
    /// the constants above).
    pub rp_site: Option<(f64, f64)>,

    /// Strip the `site` block from the polar-align service config, so
    /// the workflow must resolve it from rp's `get_site`.
    pub omit_plugin_site: bool,

    // --- Choreography ---
    /// The axis error the stub's solves were generated from, in
    /// arcminutes (east-positive azimuth, up-positive altitude).
    pub injected_error_arcmin: Option<(f64, f64)>,

    /// Per-scenario overrides merged into the service config's
    /// `adjustment` block (set by Given steps before the service
    /// starts, e.g. a short `max_duration` for deadline scenarios).
    pub adjustment_overrides: serde_json::Map<String, Value>,

    /// Per-scenario overrides merged into the service config's
    /// `measurement` block (e.g. `mode: current_position`).
    pub measurement_overrides: serde_json::Map<String, Value>,

    /// Where the simulator mount was synced to, in degrees — the
    /// anchor a current-position measurement must hint its first
    /// solve near.
    pub synced_ra_deg: Option<f64>,

    // --- TLS + auth smoke test (`auth.feature`) ---
    pub tls_auth: TlsAuthState,

    /// Doctor-subcommand smoke state (staged config file + run output)
    pub doctor_smoke: bdd_infra::doctor_smoke::DoctorSmokeState,

    // --- REST API state ---
    pub last_api_status: Option<u16>,
    pub last_api_body: Option<Value>,
}

impl PolarAlignWorld {
    pub fn rp_url(&self) -> String {
        self.rp
            .as_ref()
            .map(ServiceHandle::base_url)
            .expect("rp must be started before accessing its URL")
    }

    pub fn polar_align_url(&self) -> String {
        self.polar_align
            .as_ref()
            .map(ServiceHandle::base_url)
            .expect("polar-align must be started before accessing its URL")
    }

    /// The polar-align service's own config file: dynamic port, the
    /// shared test site, refraction off (the stub's solves are pure
    /// geometry), short exposures and intervals so scenarios stay
    /// fast.
    pub fn polar_align_service_config() -> Value {
        serde_json::json!({
            "server": { "port": 0 },
            "camera_id": "main-cam",
            "mount_id": "mount",
            "site": {
                "latitude_deg": SITE_LATITUDE_DEG,
                "longitude_deg": SITE_LONGITUDE_DEG
            },
            "measurement": { "exposure": "200ms", "settle": "0s" },
            "adjustment": {
                "exposure": "200ms",
                "interval": "200ms",
                "max_duration": "2m"
            },
            "refraction": { "enabled": false }
        })
    }

    /// Build the rp config JSON by feeding accumulated equipment,
    /// solver, and plugin entries through [`RpConfigBuilder`].
    pub fn build_rp_config(&self) -> Value {
        let mut builder = RpConfigBuilder::new();
        for camera in &self.cameras {
            builder.add_camera(camera.clone());
        }
        if let Some(mount) = &self.mount {
            builder.with_mount(mount.clone());
        }
        if let Some(plate_solver) = &self.plate_solver {
            builder.with_plate_solver(plate_solver.clone());
        }
        for plugin in &self.plugin_configs {
            builder.add_plugin(plugin.clone());
        }
        if let Some((lat, lon)) = self.rp_site {
            builder.with_site(lat, lon);
        }
        builder.build()
    }

    /// Wait for rp's `/health` endpoint to return 200.
    pub async fn wait_for_rp_healthy(&self) -> bool {
        bdd_infra::rp_harness::wait_for_rp_healthy(&self.rp_url()).await
    }
}

impl bdd_infra::doctor_smoke::DoctorSmokeWorld for PolarAlignWorld {
    fn doctor_smoke(&mut self) -> &mut bdd_infra::doctor_smoke::DoctorSmokeState {
        &mut self.doctor_smoke
    }

    fn valid_config(&self) -> serde_json::Value {
        Self::polar_align_service_config()
    }
}

impl TlsAuthSmokeWorld for PolarAlignWorld {
    const PROBE_PATH: &'static str = "/health";

    fn tls_auth(&mut self) -> &mut TlsAuthState {
        &mut self.tls_auth
    }

    fn base_test_config(&self) -> serde_json::Value {
        // The usual service config; it is never invoked — the smoke
        // scenario only probes `/health`.
        Self::polar_align_service_config()
    }

    async fn start_with_tls_auth(&mut self, config: serde_json::Value) {
        let handle = bdd_infra::tls_auth::spawn_service_handle(
            &mut self.tls_auth,
            env!("CARGO_PKG_NAME"),
            &config,
        )
        .await;
        self.polar_align = Some(handle);
    }
}

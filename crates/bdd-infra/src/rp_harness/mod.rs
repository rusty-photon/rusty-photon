//! Test harness for services that interact with the rp service.
//!
//! This module is gated behind the `rp-harness` cargo feature. It provides
//! everything a plugin or workflow service needs to run BDD tests against a
//! real rp process:
//!
//! - [`OmniSimHandle`] — singleton ASCOM simulator process (camera, filter
//!   wheel, cover calibrator).
//! - [`RpConfigBuilder`] + [`CameraConfig`], [`FilterWheelConfig`],
//!   [`CoverCalibratorConfig`] — accumulate equipment/plugin entries and emit
//!   a JSON config for rp.
//! - [`start_rp`] / [`wait_for_rp_healthy`] — spawn rp with a config and
//!   wait for its `/health` endpoint.
//! - [`WebhookReceiver`] + [`ReceivedEvent`] — in-process HTTP server that
//!   acts as an event plugin so tests can assert on emitted events.
//! - [`McpTestClient`] — persistent rmcp client for calling rp's MCP tools.
//!
//! All types emit and consume `serde_json::Value`. Nothing here depends on
//! rp's own types, which keeps the dependency direction one-way (rp's tests
//! and plugin tests depend on bdd-infra; bdd-infra does not depend on rp).

mod alpaca_stub;
mod basic_auth;
mod computed_sky;
mod config;
mod guider_stub;
mod launcher;
mod mcp_client;
mod omnisim;
mod plate_solver_stub;
mod scratch;
mod sse;
mod webhook;

pub use alpaca_stub::{
    AlpacaDeviceStub, StubDevice, STUB_CAMERA_HEIGHT_PX, STUB_CAMERA_MAX_ADU,
    STUB_CAMERA_PIXEL_SIZE_UM, STUB_CAMERA_WIDTH_PX,
};
pub use computed_sky::ComputedSky;
pub use config::{
    build_calibrator_flats_config, CameraConfig, CoolingOverrides, CoverCalibratorConfig,
    DomeConfig, FilterWheelConfig, FocuserConfig, GuiderConfig, MountConfig,
    ObservingConditionsConfig, OpticalTrainConfig, PlateSolverConfig, RotatorConfig,
    RpConfigBuilder, SafetyMonitorConfig, SwitchConfig, TrainAutoFocusConfig,
};
pub use guider_stub::{CannedGuiding, GuiderStub, GuiderStubBehavior};
pub use launcher::{start_rp, wait_for_rp_healthy, write_temp_config_file};
pub use mcp_client::McpTestClient;
pub use omnisim::OmniSimHandle;
pub use plate_solver_stub::{CannedWcs, CannedWcsMatrix, PlateSolverStub, StubBehavior};
pub use sse::{SseClient, SseFrame};
pub use webhook::{ReceivedEvent, WebhookReceiver};

// Re-exported so harness consumers can hold ComputedSky's target coords
// without depending on rp-ephemeris themselves.
pub use rp_ephemeris::IcrsCoord;

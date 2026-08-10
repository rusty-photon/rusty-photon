#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! PHD2 Guider Client Library
//!
//! This crate provides a Rust client for interacting with Open PHD Guiding 2 (PHD2)
//! via its JSON RPC interface on port 4400.
//!
//! ## Module Structure
//!
//! - [`client`] - PHD2 client for RPC communication
//! - [`config`] - Configuration types and loading
//! - `connection` - Internal connection management (not public)
//! - [`doctor`] - The read-only `doctor` subcommand (docs/services/doctor.md)
//! - [`error`] - Error types and Result alias
//! - [`events`] - PHD2 event types and application state
//! - [`fits`] - FITS file utilities for saving images
//! - [`process`] - PHD2 process management
//! - [`rpc`] - JSON RPC 2.0 types
//! - [`service`] - HTTP service mode (`phd2-guider serve`), the rp-managed guider service
//! - [`types`] - Common types (Rect, Profile, Equipment)
//!
//! ## Example
//!
//! ```ignore
//! use phd2_guider::{Phd2Client, Phd2Config, SettleParams};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let config = Phd2Config::default();
//!     let client = Phd2Client::new(config);
//!
//!     client.connect().await?;
//!
//!     let state = client.get_app_state().await?;
//!     println!("PHD2 state: {}", state);
//!
//!     client.disconnect().await?;
//!     Ok(())
//! }
//! ```

pub mod client;
pub mod config;
pub(crate) mod connection;
pub mod doctor;
pub mod error;
pub mod events;
pub mod fits;
pub mod io;
pub mod process;
pub mod rpc;
pub mod service;
pub mod types;

// Re-export commonly used types at the crate root for convenience
pub use client::Phd2Client;
pub use config::{load_config, Config, Phd2Config, ReconnectConfig, ServerConfig, SettleParams};
pub use error::{Phd2Error, Result};
pub use events::{AppState, GuideStepStats, Phd2Event};
pub use fits::{decode_base64_u16, write_grayscale_u16_fits};
pub use io::{
    ConnectionFactory, ConnectionPair, LineReader, MessageWriter, ProcessHandle, ProcessSpawner,
    TcpConnectionFactory, TcpLineReader, TcpMessageWriter, TokioProcessHandle, TokioProcessSpawner,
};
pub use process::{get_default_phd2_path, Phd2ProcessManager};
pub use rpc::{RpcErrorObject, RpcRequest, RpcResponse};
pub use service::{BoundServer, ServerBuilder};
pub use types::{
    CalibrationData, CalibrationTarget, CoolerStatus, Equipment, EquipmentDevice, GuideAxis,
    Profile, Rect, StarImage,
};

//! PHD2 client for communicating with PHD2 via JSON RPC

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tracing::debug;

/// Convert a `Duration` to whole seconds for PHD2's JSON-RPC settle payload,
/// rounding any sub-second component up. Preserves `0s` as `0`.
///
/// PHD2's `time` and `timeout` keys require integer seconds. The operator
/// config uses humantime, which accepts sub-second values like `"500ms"`;
/// without ceiling those would silently truncate to `0` on the wire.
fn settle_secs_ceil(d: Duration) -> u64 {
    d.as_secs().saturating_add(u64::from(d.subsec_nanos() != 0))
}

use crate::config::{Phd2Config, SettleParams};
use crate::connection::{
    spawn_reader_task, ConnectionConfig, ConnectionState, PendingRequest, SharedConnectionState,
};
use crate::error::{Phd2Error, Result};
use crate::events::{AppState, Phd2Event};
use crate::io::{ConnectionFactory, TcpConnectionFactory};
use crate::rpc::RpcRequest;
use crate::types::{
    CalibrationData, CalibrationTarget, CoolerStatus, Equipment, GuideAxis, Profile, Rect,
    StarImage,
};

/// PHD2 client for communicating with PHD2 via JSON RPC
pub struct Phd2Client {
    config: Phd2Config,
    request_id: AtomicU64,
    shared: SharedConnectionState,
    reconnect_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    connection_factory: Arc<dyn ConnectionFactory>,
}

impl Phd2Client {
    /// Create a new PHD2 client with the given configuration
    ///
    /// Uses the default TCP connection factory for production use.
    #[must_use]
    pub fn new(config: Phd2Config) -> Self {
        Self::with_connection_factory(config, Arc::new(TcpConnectionFactory::new()))
    }

    /// Create a new PHD2 client with a custom connection factory
    ///
    /// This is useful for testing with mock connections.
    pub fn with_connection_factory(
        config: Phd2Config,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Self {
        let auto_reconnect_enabled = config.reconnect.enabled;
        Self {
            config,
            request_id: AtomicU64::new(1),
            shared: SharedConnectionState::with_factory(
                auto_reconnect_enabled,
                connection_factory.clone(),
            ),
            reconnect_handle: tokio::sync::Mutex::new(None),
            connection_factory,
        }
    }

    /// Get the connection config for reconnection
    fn get_connection_config(&self) -> ConnectionConfig {
        ConnectionConfig {
            host: self.config.host.clone(),
            port: self.config.port,
            connection_timeout: self.config.connection_timeout,
            reconnect: self.config.reconnect.clone(),
        }
    }

    /// Connect to a running PHD2 instance
    pub async fn connect(&self) -> Result<()> {
        // Stop any ongoing reconnection attempt
        self.shared.stop_reconnect.notify_waiters();
        {
            let mut handle = self.reconnect_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        let addr = format!("{}:{}", self.config.host, self.config.port);
        debug!("Connecting to PHD2 at {}", addr);

        let connection_pair = self
            .connection_factory
            .connect(&addr, self.config.connection_timeout)
            .await?;

        debug!("Connection established to PHD2");

        // Store the writer
        {
            let mut writer_guard = self.shared.writer.lock().await;
            *writer_guard = Some(connection_pair.writer);
        }

        // Update connection state
        {
            let mut state = self.shared.state.write().await;
            state.connected = true;
            state.reconnecting = false;
        }

        // Start the reader task
        let reader_handle = spawn_reader_task(
            connection_pair.reader,
            self.get_connection_config(),
            self.shared.clone(),
        );
        {
            let mut handle_guard = self.shared.reader_handle.lock().await;
            *handle_guard = Some(reader_handle);
        }

        debug!("PHD2 client connected and reader task started");
        Ok(())
    }

    /// Disconnect from PHD2
    ///
    /// This will stop any ongoing reconnection attempts and cleanly disconnect
    /// from PHD2. After calling this, auto-reconnect will not trigger unless
    /// you manually call `connect()` again.
    pub async fn disconnect(&self) -> Result<()> {
        debug!("Disconnecting from PHD2");

        // Stop any ongoing reconnection attempt
        self.shared.stop_reconnect.notify_waiters();
        {
            let mut handle = self.reconnect_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // Abort the reader task
        {
            let mut handle = self.shared.reader_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }

        // Close the writer
        {
            let mut writer = self.shared.writer.lock().await;
            if let Some(mut w) = writer.take() {
                let _ = w.shutdown().await;
            }
        }

        // Update state
        {
            let mut state = self.shared.state.write().await;
            *state = ConnectionState::default();
        }

        // Clear pending requests
        {
            let mut pending = self.shared.pending_requests.lock().await;
            pending.clear();
        }

        debug!("Disconnected from PHD2");
        Ok(())
    }

    /// Check if connected to PHD2
    /// The `host:port` this client dials — for operator-facing messages.
    pub fn phd2_addr(&self) -> String {
        format!("{}:{}", self.config.host, self.config.port)
    }

    pub async fn is_connected(&self) -> bool {
        self.shared.is_connected().await
    }

    /// Get the PHD2 version (available after connection)
    pub async fn get_phd2_version(&self) -> Option<String> {
        self.shared.get_phd2_version().await
    }

    /// Subscribe to PHD2 events
    pub fn subscribe(&self) -> broadcast::Receiver<Phd2Event> {
        self.shared.event_sender.subscribe()
    }

    // ========================================================================
    // Auto-reconnect Control Methods
    // ========================================================================

    /// Check if auto-reconnect is currently enabled
    pub fn is_auto_reconnect_enabled(&self) -> bool {
        self.shared.is_auto_reconnect_enabled()
    }

    /// Enable or disable auto-reconnect
    ///
    /// When disabled during an active reconnection attempt, the attempt will
    /// be stopped after the current connection try completes.
    pub fn set_auto_reconnect_enabled(&self, enabled: bool) {
        self.shared.set_auto_reconnect_enabled(enabled);
    }

    /// Check if the client is currently attempting to reconnect
    pub async fn is_reconnecting(&self) -> bool {
        self.shared.is_reconnecting().await
    }

    /// Stop any ongoing reconnection attempts
    ///
    /// This stops the current reconnection task without disabling auto-reconnect.
    /// If the connection is lost again in the future, reconnection will be attempted.
    pub async fn stop_reconnection(&self) {
        self.shared.stop_reconnection().await;
        {
            let mut handle = self.reconnect_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
    }

    /// Send an RPC request and wait for response
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        if !self.is_connected().await {
            return Err(Phd2Error::NotConnected);
        }

        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = RpcRequest::new(method, params, id);
        let request_json = serde_json::to_string(&request)?;

        debug!("Sending RPC request: {}", request_json);

        // Create a oneshot channel for the response
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Register the pending request
        {
            let mut pending = self.shared.pending_requests.lock().await;
            pending.insert(id, PendingRequest { sender });
        }

        // Send the request using the MessageWriter trait
        {
            let mut writer_guard = self.shared.writer.lock().await;
            if let Some(writer) = writer_guard.as_mut() {
                writer.write_message(&request_json).await?;
            } else {
                return Err(Phd2Error::NotConnected);
            }
        }

        // Wait for response with timeout
        tokio::time::timeout(self.config.command_timeout, receiver)
            .await
            .map_err(|_| Phd2Error::Timeout(format!("Request '{method}' timed out")))?
            .map_err(|_| Phd2Error::ReceiveError)?
    }

    // ========================================================================
    // State and Status Methods
    // ========================================================================

    /// Get the current PHD2 application state
    pub async fn get_app_state(&self) -> Result<AppState> {
        let result = self.send_request("get_app_state", None).await?;
        let state_str = result
            .as_str()
            .ok_or_else(|| Phd2Error::InvalidState("Expected string for app state".to_string()))?;
        state_str.parse()
    }

    /// Get cached application state (from events, no RPC call)
    pub async fn get_cached_app_state(&self) -> Option<AppState> {
        self.shared.get_cached_app_state().await
    }

    // ========================================================================
    // Equipment and Profile Methods
    // ========================================================================

    /// Check if equipment is connected
    pub async fn is_equipment_connected(&self) -> Result<bool> {
        let result = self.send_request("get_connected", None).await?;
        result.as_bool().ok_or_else(|| {
            Phd2Error::InvalidState("Expected boolean for connected state".to_string())
        })
    }

    /// Connect all equipment in current profile
    pub async fn connect_equipment(&self) -> Result<()> {
        self.send_request("set_connected", Some(serde_json::json!(true)))
            .await?;
        Ok(())
    }

    /// Disconnect all equipment
    pub async fn disconnect_equipment(&self) -> Result<()> {
        self.send_request("set_connected", Some(serde_json::json!(false)))
            .await?;
        Ok(())
    }

    /// Get list of available equipment profiles
    pub async fn get_profiles(&self) -> Result<Vec<Profile>> {
        let result = self.send_request("get_profiles", None).await?;
        let profiles: Vec<Profile> = serde_json::from_value(result)?;
        Ok(profiles)
    }

    /// Get current active profile
    pub async fn get_current_profile(&self) -> Result<Profile> {
        let result = self.send_request("get_profile", None).await?;
        let profile: Profile = serde_json::from_value(result)?;
        Ok(profile)
    }

    /// Set active profile (equipment must be disconnected)
    pub async fn set_profile(&self, profile_id: i32) -> Result<()> {
        self.send_request("set_profile", Some(serde_json::json!({"id": profile_id})))
            .await?;
        Ok(())
    }

    /// Get current equipment configuration
    ///
    /// Returns information about all equipment devices in the current profile,
    /// including camera, mount, auxiliary mount, adaptive optics, and rotator.
    pub async fn get_current_equipment(&self) -> Result<Equipment> {
        debug!("Getting current equipment configuration");
        let result = self.send_request("get_current_equipment", None).await?;
        let equipment: Equipment = serde_json::from_value(result)?;
        Ok(equipment)
    }

    // ========================================================================
    // Application Control Methods
    // ========================================================================

    /// Shutdown PHD2 application
    pub async fn shutdown_phd2(&self) -> Result<()> {
        self.send_request("shutdown", None).await?;
        Ok(())
    }

    // ========================================================================
    // Guiding Control Methods
    // ========================================================================

    /// Start guiding with settling parameters
    ///
    /// # Arguments
    /// * `settle` - Settling parameters (pixels threshold, time, timeout)
    /// * `recalibrate` - If true, recalibrate before guiding
    /// * `roi` - Optional region of interest for star selection
    pub async fn start_guiding(
        &self,
        settle: &SettleParams,
        recalibrate: bool,
        roi: Option<Rect>,
    ) -> Result<()> {
        debug!(
            "Starting guiding with settle: pixels={}, time={:?}, timeout={:?}, recalibrate={}",
            settle.pixels, settle.time, settle.timeout, recalibrate
        );

        let settle_obj = serde_json::json!({
            "pixels": settle.pixels,
            "time": settle_secs_ceil(settle.time),
            "timeout": settle_secs_ceil(settle.timeout)
        });

        let mut params = serde_json::Map::new();
        params.insert("settle".to_string(), settle_obj);
        params.insert("recalibrate".to_string(), recalibrate.into());

        if let Some(rect) = roi {
            params.insert(
                "roi".to_string(),
                serde_json::json!([rect.x, rect.y, rect.width, rect.height]),
            );
        }

        self.send_request("guide", Some(serde_json::Value::Object(params)))
            .await?;
        Ok(())
    }

    /// Stop guiding but continue looping exposures
    ///
    /// This is equivalent to calling `loop` - it stops guiding but keeps
    /// the camera capturing frames.
    pub async fn stop_guiding(&self) -> Result<()> {
        debug!("Stopping guiding (continuing loop)");
        self.send_request("loop", None).await?;
        Ok(())
    }

    /// Stop all capture and guiding operations
    pub async fn stop_capture(&self) -> Result<()> {
        debug!("Stopping capture");
        self.send_request("stop_capture", None).await?;
        Ok(())
    }

    /// Start looping exposures without guiding
    ///
    /// If currently guiding, this will stop guiding but continue capturing frames.
    /// If stopped, this will start capturing frames.
    pub async fn start_loop(&self) -> Result<()> {
        debug!("Starting loop");
        self.send_request("loop", None).await?;
        Ok(())
    }

    /// Check if guiding is currently paused
    pub async fn is_paused(&self) -> Result<bool> {
        let result = self.send_request("get_paused", None).await?;
        result
            .as_bool()
            .ok_or_else(|| Phd2Error::InvalidState("Expected boolean for paused state".to_string()))
    }

    /// Pause guiding
    ///
    /// # Arguments
    /// * `full` - If true, pause looping entirely (no exposures). If false, continue
    ///   looping but don't send guide corrections.
    pub async fn pause(&self, full: bool) -> Result<()> {
        debug!("Pausing guiding (full={})", full);
        let params = if full {
            serde_json::json!({"paused": true, "full": "full"})
        } else {
            serde_json::json!({"paused": true})
        };
        self.send_request("set_paused", Some(params)).await?;
        Ok(())
    }

    /// Resume guiding after pause
    pub async fn resume(&self) -> Result<()> {
        debug!("Resuming guiding");
        self.send_request("set_paused", Some(serde_json::json!({"paused": false})))
            .await?;
        Ok(())
    }

    /// Dither the guide position
    ///
    /// Shifts the lock position by a random amount up to `amount` pixels.
    /// Used between exposures to reduce fixed pattern noise.
    ///
    /// # Arguments
    /// * `amount` - Maximum dither distance in pixels
    /// * `ra_only` - If true, only dither in RA axis
    /// * `settle` - Settling parameters to wait for after dither
    pub async fn dither(&self, amount: f64, ra_only: bool, settle: &SettleParams) -> Result<()> {
        debug!(
            "Dithering: amount={}, ra_only={}, settle: pixels={}, time={:?}, timeout={:?}",
            amount, ra_only, settle.pixels, settle.time, settle.timeout
        );

        let settle_obj = serde_json::json!({
            "pixels": settle.pixels,
            "time": settle_secs_ceil(settle.time),
            "timeout": settle_secs_ceil(settle.timeout)
        });

        let params = serde_json::json!({
            "amount": amount,
            "raOnly": ra_only,
            "settle": settle_obj
        });

        self.send_request("dither", Some(params)).await?;
        Ok(())
    }

    // ========================================================================
    // Star Selection Methods
    // ========================================================================

    /// Auto-select a guide star
    ///
    /// PHD2 will search for a suitable guide star. If a region of interest (ROI)
    /// is provided, the search will be limited to that region.
    ///
    /// # Arguments
    /// * `roi` - Optional region of interest for star selection
    pub async fn find_star(&self, roi: Option<Rect>) -> Result<()> {
        debug!(
            "Finding star{}",
            roi.map_or(String::new(), |r| format!(
                " in ROI [{},{},{},{}]",
                r.x, r.y, r.width, r.height
            ))
        );

        let params = roi.map(|r| serde_json::json!([r.x, r.y, r.width, r.height]));

        self.send_request("find_star", params).await?;
        Ok(())
    }

    /// Get the current lock position (guide star coordinates)
    ///
    /// Returns the x,y coordinates of the current lock position.
    /// Returns an error if no star is currently selected.
    pub async fn get_lock_position(&self) -> Result<(f64, f64)> {
        debug!("Getting lock position");
        let result = self.send_request("get_lock_position", None).await?;

        // PHD2 returns null when no star is selected
        if result.is_null() {
            return Err(Phd2Error::InvalidState(
                "No star selected - lock position not available".to_string(),
            ));
        }

        // PHD2 returns [x, y]
        let arr = result.as_array().ok_or_else(|| {
            Phd2Error::InvalidState("Expected array for lock position".to_string())
        })?;

        let [x_val, y_val] = arr.as_slice() else {
            return Err(Phd2Error::InvalidState(format!(
                "Expected 2 elements for lock position, got {}",
                arr.len()
            )));
        };

        let x = x_val.as_f64().ok_or_else(|| {
            Phd2Error::InvalidState("Expected number for x coordinate".to_string())
        })?;
        let y = y_val.as_f64().ok_or_else(|| {
            Phd2Error::InvalidState("Expected number for y coordinate".to_string())
        })?;

        Ok((x, y))
    }

    /// Set the lock position (guide star coordinates)
    ///
    /// # Arguments
    /// * `x` - X coordinate for the lock position
    /// * `y` - Y coordinate for the lock position
    /// * `exact` - If true, use the exact position. If false, PHD2 will search
    ///   for a nearby star to use as the guide star.
    pub async fn set_lock_position(&self, x: f64, y: f64, exact: bool) -> Result<()> {
        debug!("Setting lock position to ({}, {}), exact={}", x, y, exact);

        let params = serde_json::json!({
            "X": x,
            "Y": y,
            "EXACT": exact
        });

        self.send_request("set_lock_position", Some(params)).await?;
        Ok(())
    }

    // ========================================================================
    // Calibration Methods
    // ========================================================================

    /// Check if the mount is calibrated
    pub async fn is_calibrated(&self) -> Result<bool> {
        debug!("Checking calibration status");
        let result = self.send_request("get_calibrated", None).await?;
        result.as_bool().ok_or_else(|| {
            Phd2Error::InvalidState("Expected boolean for calibrated state".to_string())
        })
    }

    /// Get calibration data for the specified target
    ///
    /// # Arguments
    /// * `which` - Which calibration data to retrieve (Mount or AO)
    ///
    /// Note: `CalibrationTarget::Both` is not valid for this method and will
    /// default to returning Mount calibration data.
    pub async fn get_calibration_data(&self, which: CalibrationTarget) -> Result<CalibrationData> {
        debug!("Getting calibration data for {}", which);

        let params = serde_json::json!({
            "which": which.to_get_api_string()
        });

        let result = self
            .send_request("get_calibration_data", Some(params))
            .await?;
        let data: CalibrationData = serde_json::from_value(result)?;
        Ok(data)
    }

    /// Clear calibration data
    ///
    /// # Arguments
    /// * `which` - Which calibration to clear (Mount, AO, or Both)
    pub async fn clear_calibration(&self, which: CalibrationTarget) -> Result<()> {
        debug!("Clearing calibration for {}", which);

        let params = serde_json::json!({
            "which": which.to_clear_api_string()
        });

        self.send_request("clear_calibration", Some(params)).await?;
        Ok(())
    }

    /// Flip calibration for meridian flip
    ///
    /// Inverts the existing calibration data without requiring a full recalibration.
    /// Should be called after performing a meridian flip on the mount.
    pub async fn flip_calibration(&self) -> Result<()> {
        debug!("Flipping calibration");
        self.send_request("flip_calibration", None).await?;
        Ok(())
    }

    // ========================================================================
    // Camera Exposure Methods
    // ========================================================================

    /// Get the current exposure duration in milliseconds
    pub async fn get_exposure(&self) -> Result<u32> {
        debug!("Getting exposure duration");
        let result = self.send_request("get_exposure", None).await?;
        result
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| {
                Phd2Error::InvalidState("Expected integer for exposure duration".to_string())
            })
    }

    /// Set the exposure duration in milliseconds
    ///
    /// # Arguments
    /// * `exposure_ms` - Exposure duration in milliseconds
    pub async fn set_exposure(&self, exposure_ms: u32) -> Result<()> {
        debug!("Setting exposure duration to {} ms", exposure_ms);
        self.send_request("set_exposure", Some(serde_json::json!(exposure_ms)))
            .await?;
        Ok(())
    }

    /// Get the list of valid exposure durations
    ///
    /// Returns a list of exposure durations in milliseconds that the camera supports.
    pub async fn get_exposure_durations(&self) -> Result<Vec<u32>> {
        debug!("Getting exposure durations");
        let result = self.send_request("get_exposure_durations", None).await?;
        let durations: Vec<u32> = serde_json::from_value(result)?;
        Ok(durations)
    }

    /// Get the camera frame size (image dimensions)
    ///
    /// Returns the width and height of the camera sensor in pixels.
    pub async fn get_camera_frame_size(&self) -> Result<(u32, u32)> {
        debug!("Getting camera frame size");
        let result = self.send_request("get_camera_frame_size", None).await?;

        // PHD2 returns [width, height]
        let arr = result.as_array().ok_or_else(|| {
            Phd2Error::InvalidState("Expected array for camera frame size".to_string())
        })?;

        let [width_val, height_val] = arr.as_slice() else {
            return Err(Phd2Error::InvalidState(format!(
                "Expected 2 elements for frame size, got {}",
                arr.len()
            )));
        };

        let width = width_val
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| Phd2Error::InvalidState("Expected integer for width".to_string()))?;
        let height = height_val
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| Phd2Error::InvalidState("Expected integer for height".to_string()))?;

        Ok((width, height))
    }

    /// Check if subframe mode is enabled
    ///
    /// When subframing is enabled, PHD2 only reads a portion of the camera
    /// sensor around the guide star, which can improve frame rate.
    pub async fn get_use_subframes(&self) -> Result<bool> {
        debug!("Getting use subframes setting");
        let result = self.send_request("get_use_subframes", None).await?;
        result.as_bool().ok_or_else(|| {
            Phd2Error::InvalidState("Expected boolean for use subframes".to_string())
        })
    }

    /// Capture a single frame
    ///
    /// Acquires one frame from the camera. This can be used to preview the
    /// field of view or check focus without starting guiding.
    ///
    /// # Arguments
    /// * `exposure_ms` - Optional exposure duration in milliseconds. If not specified,
    ///   uses the current exposure setting.
    /// * `subframe` - Optional region of interest to capture. If not specified,
    ///   captures the full frame.
    pub async fn capture_single_frame(
        &self,
        exposure_ms: Option<u32>,
        subframe: Option<Rect>,
    ) -> Result<()> {
        debug!(
            "Capturing single frame{}{}",
            exposure_ms.map_or(String::new(), |e| format!(", exposure={e}ms")),
            subframe.map_or(String::new(), |r| format!(
                ", subframe=[{},{},{},{}]",
                r.x, r.y, r.width, r.height
            ))
        );

        let mut params = serde_json::Map::new();

        if let Some(exp) = exposure_ms {
            params.insert("exposure".to_string(), serde_json::json!(exp));
        }

        if let Some(rect) = subframe {
            params.insert(
                "subframe".to_string(),
                serde_json::json!([rect.x, rect.y, rect.width, rect.height]),
            );
        }

        let params_value = if params.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(params))
        };

        self.send_request("capture_single_frame", params_value)
            .await?;
        Ok(())
    }

    // ========================================================================
    // Guide Algorithm Parameter Methods
    // ========================================================================

    /// Get the list of algorithm parameter names for the specified axis
    ///
    /// Returns a list of parameter names that can be used with `get_algo_param`
    /// and `set_algo_param`.
    ///
    /// # Arguments
    /// * `axis` - The guide axis (RA or Dec)
    pub async fn get_algo_param_names(&self, axis: GuideAxis) -> Result<Vec<String>> {
        debug!("Getting algorithm parameter names for {} axis", axis);

        let params = serde_json::json!({
            "axis": axis.to_api_string()
        });

        let result = self
            .send_request("get_algo_param_names", Some(params))
            .await?;
        let names: Vec<String> = serde_json::from_value(result)?;
        Ok(names)
    }

    /// Get the value of a guide algorithm parameter
    ///
    /// # Arguments
    /// * `axis` - The guide axis (RA or Dec)
    /// * `name` - The parameter name (from `get_algo_param_names`)
    pub async fn get_algo_param(&self, axis: GuideAxis, name: &str) -> Result<f64> {
        debug!("Getting algorithm parameter '{}' for {} axis", name, axis);

        let params = serde_json::json!({
            "axis": axis.to_api_string(),
            "name": name
        });

        let result = self.send_request("get_algo_param", Some(params)).await?;
        result.as_f64().ok_or_else(|| {
            Phd2Error::InvalidState(format!("Expected number for algorithm parameter '{name}'"))
        })
    }

    /// Set the value of a guide algorithm parameter
    ///
    /// # Arguments
    /// * `axis` - The guide axis (RA or Dec)
    /// * `name` - The parameter name (from `get_algo_param_names`)
    /// * `value` - The new value for the parameter
    pub async fn set_algo_param(&self, axis: GuideAxis, name: &str, value: f64) -> Result<()> {
        debug!(
            "Setting algorithm parameter '{}' for {} axis to {}",
            name, axis, value
        );

        let params = serde_json::json!({
            "axis": axis.to_api_string(),
            "name": name,
            "value": value
        });

        self.send_request("set_algo_param", Some(params)).await?;
        Ok(())
    }

    // ========================================================================
    // Camera Cooling Methods
    // ========================================================================

    /// Get the current CCD sensor temperature
    ///
    /// Returns the temperature in degrees Celsius.
    /// Returns an error if the camera does not support temperature reading.
    pub async fn get_ccd_temperature(&self) -> Result<f64> {
        debug!("Getting CCD temperature");
        let result = self.send_request("get_ccd_temperature", None).await?;
        result.as_f64().ok_or_else(|| {
            Phd2Error::InvalidState("Expected number for CCD temperature".to_string())
        })
    }

    /// Get the cooler status including temperature and power
    ///
    /// Returns detailed cooler information including current temperature,
    /// whether the cooler is enabled, setpoint temperature, and power level.
    pub async fn get_cooler_status(&self) -> Result<CoolerStatus> {
        debug!("Getting cooler status");
        let result = self.send_request("get_cooler_status", None).await?;
        let status: CoolerStatus = serde_json::from_value(result)?;
        Ok(status)
    }

    /// Set the cooler state
    ///
    /// Enables or disables the camera cooler and optionally sets the target temperature.
    ///
    /// # Arguments
    /// * `enabled` - Whether to enable the cooler
    /// * `temperature` - Target temperature in degrees Celsius (required when enabling)
    pub async fn set_cooler_state(&self, enabled: bool, temperature: Option<f64>) -> Result<()> {
        debug!(
            "Setting cooler state: enabled={}, temperature={:?}",
            enabled, temperature
        );

        let mut params = serde_json::Map::new();
        params.insert("enabled".to_string(), enabled.into());

        if let Some(temp) = temperature {
            params.insert("temperature".to_string(), temp.into());
        }

        self.send_request("set_cooler_state", Some(serde_json::Value::Object(params)))
            .await?;
        Ok(())
    }

    // ========================================================================
    // Image Operations Methods
    // ========================================================================

    /// Get the current guide star image
    ///
    /// Returns the guide star image data including the image pixels as base64-encoded data.
    /// The image is a subframe around the guide star.
    ///
    /// # Arguments
    /// * `size` - Size of the image in pixels (width and height will be 2*size+1)
    pub async fn get_star_image(&self, size: u32) -> Result<StarImage> {
        debug!("Getting star image with size {}", size);

        let params = serde_json::json!({
            "size": size
        });

        let result = self.send_request("get_star_image", Some(params)).await?;
        let image: StarImage = serde_json::from_value(result)?;
        Ok(image)
    }

    /// Save the current camera frame to a file
    ///
    /// Saves the current frame to a FITS file in PHD2's default image directory.
    /// Returns the path to the saved file.
    pub async fn save_image(&self) -> Result<String> {
        debug!("Saving current image");
        let result = self.send_request("save_image", None).await?;
        let filename = result.as_str().ok_or_else(|| {
            Phd2Error::InvalidState("Expected string for saved image filename".to_string())
        })?;
        Ok(filename.to_string())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::ReconnectConfig;
    use crate::types::{CalibrationData, CalibrationTarget, Equipment, Rect};

    #[test]
    fn test_settle_secs_ceil_preserves_zero() {
        assert_eq!(settle_secs_ceil(Duration::ZERO), 0);
    }

    #[test]
    fn test_settle_secs_ceil_passes_whole_seconds() {
        assert_eq!(settle_secs_ceil(Duration::from_secs(10)), 10);
    }

    #[test]
    fn test_settle_secs_ceil_rounds_sub_second_up() {
        assert_eq!(settle_secs_ceil(Duration::from_millis(500)), 1);
        assert_eq!(settle_secs_ceil(Duration::from_millis(1)), 1);
    }

    #[test]
    fn test_settle_secs_ceil_rounds_mixed_up() {
        assert_eq!(settle_secs_ceil(Duration::from_millis(1500)), 2);
        assert_eq!(settle_secs_ceil(Duration::from_millis(2001)), 3);
    }

    #[test]
    fn test_settle_secs_ceil_saturates_on_overflow() {
        // Duration::MAX has u64::MAX seconds AND 999_999_999 sub-second nanos,
        // so the +1 ceiling adjustment would wrap without saturation.
        assert_eq!(settle_secs_ceil(Duration::MAX), u64::MAX);
    }

    #[test]
    fn test_guide_request_params_format() {
        let settle = SettleParams::default();
        let settle_obj = serde_json::json!({
            "pixels": settle.pixels,
            "time": settle_secs_ceil(settle.time),
            "timeout": settle_secs_ceil(settle.timeout)
        });
        let params = serde_json::json!({
            "settle": settle_obj,
            "recalibrate": false
        });

        assert!(params["settle"]["pixels"].as_f64().is_some());
        assert!(params["settle"]["time"].as_u64().is_some());
        assert!(params["settle"]["timeout"].as_u64().is_some());
        assert!(!params["recalibrate"].as_bool().unwrap());
    }

    #[test]
    fn test_guide_request_with_roi() {
        let settle = SettleParams::default();
        let roi = Rect::new(100, 100, 200, 200);

        let settle_obj = serde_json::json!({
            "pixels": settle.pixels,
            "time": settle_secs_ceil(settle.time),
            "timeout": settle_secs_ceil(settle.timeout)
        });
        let mut params = serde_json::json!({
            "settle": settle_obj,
            "recalibrate": true
        });
        params["roi"] = serde_json::json!([roi.x, roi.y, roi.width, roi.height]);

        let roi_arr = params["roi"].as_array().unwrap();
        assert_eq!(roi_arr.len(), 4);
        assert_eq!(roi_arr[0].as_i64().unwrap(), 100);
        assert_eq!(roi_arr[1].as_i64().unwrap(), 100);
        assert_eq!(roi_arr[2].as_i64().unwrap(), 200);
        assert_eq!(roi_arr[3].as_i64().unwrap(), 200);
    }

    #[test]
    fn test_dither_request_params_format() {
        let settle = SettleParams::default();
        let settle_obj = serde_json::json!({
            "pixels": settle.pixels,
            "time": settle_secs_ceil(settle.time),
            "timeout": settle_secs_ceil(settle.timeout)
        });
        let params = serde_json::json!({
            "amount": 5.0,
            "raOnly": true,
            "settle": settle_obj
        });

        assert_eq!(params["amount"].as_f64().unwrap(), 5.0);
        assert!(params["raOnly"].as_bool().unwrap());
        assert!(params["settle"]["pixels"].as_f64().is_some());
    }

    #[test]
    fn test_pause_request_params_full() {
        let params = serde_json::json!({"paused": true, "full": "full"});
        assert!(params["paused"].as_bool().unwrap());
        assert_eq!(params["full"].as_str().unwrap(), "full");
    }

    #[test]
    fn test_pause_request_params_partial() {
        let params = serde_json::json!({"paused": true});
        assert!(params["paused"].as_bool().unwrap());
        assert!(params.get("full").is_none());
    }

    #[test]
    fn test_resume_request_params() {
        let params = serde_json::json!({"paused": false});
        assert!(!params["paused"].as_bool().unwrap());
    }

    #[test]
    fn test_get_current_equipment_response_parsing() {
        // Simulate PHD2's get_current_equipment response
        let response_json = serde_json::json!({
            "camera": {"name": "ZWO ASI120MM Mini", "connected": true},
            "mount": {"name": "EQMOD ASCOM HEQ5/6", "connected": true},
            "aux_mount": null,
            "AO": null,
            "rotator": null
        });

        let equipment: Equipment = serde_json::from_value(response_json).unwrap();

        let camera = equipment.camera.unwrap();
        assert_eq!(camera.name, "ZWO ASI120MM Mini");
        assert!(camera.connected);

        let mount = equipment.mount.unwrap();
        assert_eq!(mount.name, "EQMOD ASCOM HEQ5/6");
        assert!(mount.connected);

        assert!(equipment.aux_mount.is_none());
        assert!(equipment.ao.is_none());
        assert!(equipment.rotator.is_none());
    }

    #[test]
    fn test_client_auto_reconnect_default_enabled() {
        let config = Phd2Config::default();
        let client = Phd2Client::new(config);
        assert!(client.is_auto_reconnect_enabled());
    }

    #[test]
    fn test_client_auto_reconnect_disabled_in_config() {
        let config = Phd2Config {
            reconnect: ReconnectConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let client = Phd2Client::new(config);
        assert!(!client.is_auto_reconnect_enabled());
    }

    #[test]
    fn test_client_toggle_auto_reconnect() {
        let config = Phd2Config::default();
        let client = Phd2Client::new(config);

        assert!(client.is_auto_reconnect_enabled());
        client.set_auto_reconnect_enabled(false);
        assert!(!client.is_auto_reconnect_enabled());
        client.set_auto_reconnect_enabled(true);
        assert!(client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_client_initial_state() {
        let config = Phd2Config::default();
        let client = Phd2Client::new(config);

        assert!(!client.is_connected().await);
        assert!(!client.is_reconnecting().await);
        assert!(client.get_phd2_version().await.is_none());
    }

    // ========================================================================
    // Star Selection Method Tests
    // ========================================================================

    #[test]
    fn test_find_star_request_params_no_roi() {
        let params: Option<serde_json::Value> = None;
        assert!(params.is_none());
    }

    #[test]
    fn test_find_star_request_params_with_roi() {
        let roi = Rect::new(100, 200, 300, 400);
        let params = serde_json::json!([roi.x, roi.y, roi.width, roi.height]);

        let arr = params.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0].as_i64().unwrap(), 100);
        assert_eq!(arr[1].as_i64().unwrap(), 200);
        assert_eq!(arr[2].as_i64().unwrap(), 300);
        assert_eq!(arr[3].as_i64().unwrap(), 400);
    }

    #[test]
    fn test_get_lock_position_response_parsing() {
        let response = serde_json::json!([256.5, 512.3]);
        let arr = response.as_array().unwrap();
        let x = arr[0].as_f64().unwrap();
        let y = arr[1].as_f64().unwrap();

        assert_eq!(x, 256.5);
        assert_eq!(y, 512.3);
    }

    #[test]
    fn test_get_lock_position_null_response() {
        let response = serde_json::json!(null);
        assert!(response.is_null());
    }

    #[test]
    fn test_set_lock_position_request_params() {
        let params = serde_json::json!({
            "X": 256.5,
            "Y": 512.3,
            "EXACT": true
        });

        assert_eq!(params["X"].as_f64().unwrap(), 256.5);
        assert_eq!(params["Y"].as_f64().unwrap(), 512.3);
        assert!(params["EXACT"].as_bool().unwrap());
    }

    #[test]
    fn test_set_lock_position_request_params_not_exact() {
        let params = serde_json::json!({
            "X": 100.0,
            "Y": 200.0,
            "EXACT": false
        });

        assert_eq!(params["X"].as_f64().unwrap(), 100.0);
        assert_eq!(params["Y"].as_f64().unwrap(), 200.0);
        assert!(!params["EXACT"].as_bool().unwrap());
    }

    // ========================================================================
    // Calibration Method Tests
    // ========================================================================

    #[test]
    fn test_get_calibration_data_request_params_mount() {
        let params = serde_json::json!({
            "which": CalibrationTarget::Mount.to_get_api_string()
        });
        assert_eq!(params["which"].as_str().unwrap(), "Mount");
    }

    #[test]
    fn test_get_calibration_data_request_params_ao() {
        let params = serde_json::json!({
            "which": CalibrationTarget::AO.to_get_api_string()
        });
        assert_eq!(params["which"].as_str().unwrap(), "AO");
    }

    #[test]
    fn test_clear_calibration_request_params_mount() {
        let params = serde_json::json!({
            "which": CalibrationTarget::Mount.to_clear_api_string()
        });
        assert_eq!(params["which"].as_str().unwrap(), "mount");
    }

    #[test]
    fn test_clear_calibration_request_params_ao() {
        let params = serde_json::json!({
            "which": CalibrationTarget::AO.to_clear_api_string()
        });
        assert_eq!(params["which"].as_str().unwrap(), "ao");
    }

    #[test]
    fn test_clear_calibration_request_params_both() {
        let params = serde_json::json!({
            "which": CalibrationTarget::Both.to_clear_api_string()
        });
        assert_eq!(params["which"].as_str().unwrap(), "both");
    }

    #[test]
    fn test_get_calibration_data_response_parsing() {
        let response = serde_json::json!({
            "calibrated": true,
            "xAngle": 45.5,
            "xRate": 15.2,
            "xParity": "+",
            "yAngle": 135.5,
            "yRate": 14.8,
            "yParity": "-",
            "declination": 30.0
        });

        let data: CalibrationData = serde_json::from_value(response).unwrap();
        assert!(data.calibrated);
        assert_eq!(data.x_angle, 45.5);
        assert_eq!(data.x_rate, 15.2);
        assert_eq!(data.x_parity, "+");
        assert_eq!(data.y_angle, 135.5);
        assert_eq!(data.y_rate, 14.8);
        assert_eq!(data.y_parity, "-");
        assert_eq!(data.declination, Some(30.0));
    }

    #[test]
    fn test_get_calibration_data_response_not_calibrated() {
        let response = serde_json::json!({
            "calibrated": false,
            "xAngle": 0.0,
            "xRate": 0.0,
            "xParity": "+",
            "yAngle": 0.0,
            "yRate": 0.0,
            "yParity": "+"
        });

        let data: CalibrationData = serde_json::from_value(response).unwrap();
        assert!(!data.calibrated);
        assert_eq!(data.x_rate, 0.0);
        assert!(data.declination.is_none());
    }

    // ========================================================================
    // Camera Exposure Method Tests
    // ========================================================================

    #[test]
    fn test_get_exposure_response_parsing() {
        let response = serde_json::json!(2000);
        let exposure = response.as_u64().map(|v| v as u32).unwrap();
        assert_eq!(exposure, 2000);
    }

    #[test]
    fn test_set_exposure_request_params() {
        let params = serde_json::json!(1500);
        assert_eq!(params.as_u64().unwrap(), 1500);
    }

    #[test]
    fn test_get_exposure_durations_response_parsing() {
        let response = serde_json::json!([100, 200, 500, 1000, 2000, 3000, 5000]);
        let durations: Vec<u32> = serde_json::from_value(response).unwrap();
        assert_eq!(durations.len(), 7);
        assert_eq!(durations[0], 100);
        assert_eq!(durations[3], 1000);
        assert_eq!(durations[6], 5000);
    }

    #[test]
    fn test_get_camera_frame_size_response_parsing() {
        let response = serde_json::json!([1280, 960]);
        let arr = response.as_array().unwrap();
        let width = arr[0].as_u64().map(|v| v as u32).unwrap();
        let height = arr[1].as_u64().map(|v| v as u32).unwrap();
        assert_eq!(width, 1280);
        assert_eq!(height, 960);
    }

    #[test]
    fn test_get_use_subframes_response_parsing() {
        let response_true = serde_json::json!(true);
        assert!(response_true.as_bool().unwrap());

        let response_false = serde_json::json!(false);
        assert!(!response_false.as_bool().unwrap());
    }

    #[test]
    fn test_capture_single_frame_no_params() {
        let params: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        assert!(params.is_empty());
    }

    #[test]
    fn test_capture_single_frame_with_exposure() {
        let mut params = serde_json::Map::new();
        params.insert("exposure".to_string(), serde_json::json!(3000));
        assert_eq!(params["exposure"].as_u64().unwrap(), 3000);
    }

    #[test]
    fn test_capture_single_frame_with_subframe() {
        let rect = Rect::new(100, 100, 200, 200);
        let mut params = serde_json::Map::new();
        params.insert(
            "subframe".to_string(),
            serde_json::json!([rect.x, rect.y, rect.width, rect.height]),
        );

        let subframe = params["subframe"].as_array().unwrap();
        assert_eq!(subframe.len(), 4);
        assert_eq!(subframe[0].as_i64().unwrap(), 100);
        assert_eq!(subframe[1].as_i64().unwrap(), 100);
        assert_eq!(subframe[2].as_i64().unwrap(), 200);
        assert_eq!(subframe[3].as_i64().unwrap(), 200);
    }

    #[test]
    fn test_capture_single_frame_with_all_params() {
        let rect = Rect::new(50, 50, 256, 256);
        let mut params = serde_json::Map::new();
        params.insert("exposure".to_string(), serde_json::json!(2000));
        params.insert(
            "subframe".to_string(),
            serde_json::json!([rect.x, rect.y, rect.width, rect.height]),
        );

        assert_eq!(params["exposure"].as_u64().unwrap(), 2000);
        let subframe = params["subframe"].as_array().unwrap();
        assert_eq!(subframe[0].as_i64().unwrap(), 50);
        assert_eq!(subframe[2].as_i64().unwrap(), 256);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod mock_tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use crate::io::{ConnectionFactory, ConnectionPair, LineReader, MessageWriter};
    use crate::types::{CalibrationTarget, GuideAxis, Rect};
    use async_trait::async_trait;

    // ============================================================================
    // Mock implementations for testing
    // ============================================================================

    /// Mock line reader that returns pre-configured responses
    struct MockLineReaderWithResponses {
        responses: StdMutex<VecDeque<Option<String>>>,
    }

    impl MockLineReaderWithResponses {
        fn new(responses: Vec<Option<String>>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl LineReader for MockLineReaderWithResponses {
        async fn read_line(&mut self) -> crate::Result<Option<String>> {
            let mut responses = self.responses.lock().unwrap();
            match responses.pop_front() {
                Some(response) => Ok(response),
                None => Ok(None), // EOF
            }
        }
    }

    /// Mock message writer that records sent messages
    struct MockMessageWriterWithRecorder {
        sent_messages: Arc<StdMutex<Vec<String>>>,
    }

    impl MockMessageWriterWithRecorder {
        fn new(sent_messages: Arc<StdMutex<Vec<String>>>) -> Self {
            Self { sent_messages }
        }
    }

    #[async_trait]
    impl MessageWriter for MockMessageWriterWithRecorder {
        async fn write_message(&mut self, message: &str) -> crate::Result<()> {
            self.sent_messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        async fn shutdown(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    type MockPair = (Vec<Option<String>>, Arc<StdMutex<Vec<String>>>);

    /// Mock connection factory that returns pre-configured reader/writer pairs
    struct MockConnectionFactoryWithPairs {
        pairs: StdMutex<VecDeque<MockPair>>,
    }

    impl MockConnectionFactoryWithPairs {
        fn new() -> Self {
            Self {
                pairs: StdMutex::new(VecDeque::new()),
            }
        }

        fn add_connection(&self, responses: Vec<Option<String>>) -> Arc<StdMutex<Vec<String>>> {
            let sent_messages = Arc::new(StdMutex::new(Vec::new()));
            self.pairs
                .lock()
                .unwrap()
                .push_back((responses, sent_messages.clone()));
            sent_messages
        }
    }

    #[async_trait]
    impl ConnectionFactory for MockConnectionFactoryWithPairs {
        async fn connect(&self, _addr: &str, _timeout: Duration) -> crate::Result<ConnectionPair> {
            let mut pairs = self.pairs.lock().unwrap();
            if let Some((responses, sent_messages)) = pairs.pop_front() {
                Ok(ConnectionPair {
                    reader: Box::new(MockLineReaderWithResponses::new(responses)),
                    writer: Box::new(MockMessageWriterWithRecorder::new(sent_messages)),
                })
            } else {
                Err(Phd2Error::ConnectionFailed(
                    "No mock connections available".to_string(),
                ))
            }
        }

        async fn can_connect(&self, _addr: &str) -> bool {
            !self.pairs.lock().unwrap().is_empty()
        }
    }

    /// Helper to create a test client with mock responses
    fn create_test_client_with_responses(
        responses: Vec<Option<String>>,
    ) -> (Phd2Client, Arc<StdMutex<Vec<String>>>) {
        let factory = Arc::new(MockConnectionFactoryWithPairs::new());
        let sent_messages = factory.add_connection(responses);

        let config = Phd2Config {
            host: "localhost".to_string(),
            port: 4400,
            connection_timeout: std::time::Duration::from_secs(1),
            command_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        };

        let client = Phd2Client::with_connection_factory(config, factory);
        (client, sent_messages)
    }

    /// Helper to create a version event response
    fn version_event() -> String {
        r#"{"Event":"Version","PHDVersion":"2.6.11","PHDSubver":"","MsgVersion":1}"#.to_string()
    }

    /// Helper to create an RPC response
    fn rpc_response(id: u64, result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","result":{result},"id":{id}}}"#)
    }

    /// Helper to create an RPC error response
    fn rpc_error(id: u64, code: i32, message: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","error":{{"code":{code},"message":"{message}"}},"id":{id}}}"#)
    }

    // ============================================================================
    // Connection tests
    // ============================================================================

    #[tokio::test]
    async fn test_client_connect_success() {
        let (client, _sent) = create_test_client_with_responses(vec![Some(version_event())]);

        client.connect().await.unwrap();
        assert!(client.is_connected().await);
    }

    #[tokio::test]
    async fn test_client_disconnect() {
        let (client, _sent) = create_test_client_with_responses(vec![Some(version_event())]);

        client.connect().await.unwrap();
        client.disconnect().await.unwrap();
        assert!(!client.is_connected().await);
    }

    #[tokio::test]
    async fn test_client_not_connected_error() {
        let factory = Arc::new(MockConnectionFactoryWithPairs::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        let result = client.get_app_state().await;
        assert!(matches!(result, Err(Phd2Error::NotConnected)));
    }

    // ============================================================================
    // State and status method tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_app_state_stopped() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#""Stopped""#)),
        ]);

        client.connect().await.unwrap();
        let state = client.get_app_state().await.unwrap();
        assert_eq!(state, AppState::Stopped);
    }

    #[tokio::test]
    async fn test_get_app_state_guiding() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#""Guiding""#)),
        ]);

        client.connect().await.unwrap();
        let state = client.get_app_state().await.unwrap();
        assert_eq!(state, AppState::Guiding);
    }

    #[tokio::test]
    async fn test_get_app_state_looping() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#""Looping""#)),
        ]);

        client.connect().await.unwrap();
        let state = client.get_app_state().await.unwrap();
        assert_eq!(state, AppState::Looping);
    }

    #[tokio::test]
    async fn test_get_app_state_calibrating() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#""Calibrating""#)),
        ]);

        client.connect().await.unwrap();
        let state = client.get_app_state().await.unwrap();
        assert_eq!(state, AppState::Calibrating);
    }

    // ============================================================================
    // Equipment and profile method tests
    // ============================================================================

    #[tokio::test]
    async fn test_is_equipment_connected_true() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "true")),
        ]);

        client.connect().await.unwrap();
        let connected = client.is_equipment_connected().await.unwrap();
        assert!(connected);
    }

    #[tokio::test]
    async fn test_is_equipment_connected_false() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "false")),
        ]);

        client.connect().await.unwrap();
        let connected = client.is_equipment_connected().await.unwrap();
        assert!(!connected);
    }

    #[tokio::test]
    async fn test_connect_equipment() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.connect_equipment().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_connected"));
        assert!(messages[0].contains("true"));
    }

    #[tokio::test]
    async fn test_disconnect_equipment() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.disconnect_equipment().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_connected"));
        assert!(messages[0].contains("false"));
    }

    #[tokio::test]
    async fn test_get_profiles() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(
                1,
                r#"[{"id":1,"name":"Profile 1"},{"id":2,"name":"Profile 2"}]"#,
            )),
        ]);

        client.connect().await.unwrap();
        let profiles = client.get_profiles().await.unwrap();

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, 1);
        assert_eq!(profiles[0].name, "Profile 1");
        assert_eq!(profiles[1].id, 2);
        assert_eq!(profiles[1].name, "Profile 2");
    }

    #[tokio::test]
    async fn test_get_current_profile() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#"{"id":1,"name":"Default"}"#)),
        ]);

        client.connect().await.unwrap();
        let profile = client.get_current_profile().await.unwrap();

        assert_eq!(profile.id, 1);
        assert_eq!(profile.name, "Default");
    }

    #[tokio::test]
    async fn test_set_profile() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_profile(2).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_profile"));
        assert!(messages[0].contains("\"id\":2"));
    }

    #[tokio::test]
    async fn test_get_current_equipment() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(
                1,
                r#"{"camera":{"name":"Camera","connected":true},"mount":{"name":"Mount","connected":true},"aux_mount":null,"AO":null,"rotator":null}"#,
            )),
        ]);

        client.connect().await.unwrap();
        let equipment = client.get_current_equipment().await.unwrap();

        assert!(equipment.camera.is_some());
        let camera = equipment.camera.unwrap();
        assert_eq!(camera.name, "Camera");
        assert!(camera.connected);
    }

    // ============================================================================
    // Guiding control method tests
    // ============================================================================

    #[tokio::test]
    async fn test_start_guiding() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let settle = SettleParams {
            pixels: 0.5,
            time: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_mins(1),
        };
        client.start_guiding(&settle, false, None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("guide"));
        assert!(messages[0].contains("\"pixels\":0.5"));
        assert!(messages[0].contains("\"recalibrate\":false"));
    }

    #[tokio::test]
    async fn test_start_guiding_with_recalibrate() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let settle = SettleParams::default();
        client.start_guiding(&settle, true, None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"recalibrate\":true"));
    }

    #[tokio::test]
    async fn test_start_guiding_with_roi() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let settle = SettleParams::default();
        let roi = Rect {
            x: 100,
            y: 100,
            width: 200,
            height: 200,
        };
        client
            .start_guiding(&settle, false, Some(roi))
            .await
            .unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"roi\":[100,100,200,200]"));
    }

    #[tokio::test]
    async fn test_stop_guiding() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.stop_guiding().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("loop"));
    }

    #[tokio::test]
    async fn test_stop_capture() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.stop_capture().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("stop_capture"));
    }

    #[tokio::test]
    async fn test_start_loop() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.start_loop().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("loop"));
    }

    #[tokio::test]
    async fn test_is_paused_true() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "true")),
        ]);

        client.connect().await.unwrap();
        let paused = client.is_paused().await.unwrap();
        assert!(paused);
    }

    #[tokio::test]
    async fn test_is_paused_false() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "false")),
        ]);

        client.connect().await.unwrap();
        let paused = client.is_paused().await.unwrap();
        assert!(!paused);
    }

    #[tokio::test]
    async fn test_pause_partial() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.pause(false).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_paused"));
        assert!(messages[0].contains("\"paused\":true"));
        assert!(!messages[0].contains("\"full\""));
    }

    #[tokio::test]
    async fn test_pause_full() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.pause(true).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_paused"));
        assert!(messages[0].contains("\"full\":\"full\""));
    }

    #[tokio::test]
    async fn test_resume() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.resume().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_paused"));
        assert!(messages[0].contains("\"paused\":false"));
    }

    #[tokio::test]
    async fn test_dither() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let settle = SettleParams {
            pixels: 0.5,
            time: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_mins(1),
        };
        client.dither(5.0, false, &settle).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("dither"));
        assert!(messages[0].contains("\"amount\":5"));
        assert!(messages[0].contains("\"raOnly\":false"));
    }

    #[tokio::test]
    async fn test_dither_ra_only() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let settle = SettleParams::default();
        client.dither(3.0, true, &settle).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"raOnly\":true"));
    }

    // ============================================================================
    // Star selection method tests
    // ============================================================================

    #[tokio::test]
    async fn test_find_star() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.find_star(None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("find_star"));
    }

    #[tokio::test]
    async fn test_find_star_with_roi() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let roi = Rect {
            x: 50,
            y: 50,
            width: 100,
            height: 100,
        };
        client.find_star(Some(roi)).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("[50,50,100,100]"));
    }

    #[tokio::test]
    async fn test_get_lock_position() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "[320.5,240.3]")),
        ]);

        client.connect().await.unwrap();
        let (x, y) = client.get_lock_position().await.unwrap();

        assert!((x - 320.5).abs() < 0.01);
        assert!((y - 240.3).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_get_lock_position_no_star() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "null")),
        ]);

        client.connect().await.unwrap();
        let result = client.get_lock_position().await;

        assert!(matches!(result, Err(Phd2Error::InvalidState(_))));
    }

    #[tokio::test]
    async fn test_set_lock_position() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_lock_position(320.0, 240.0, true).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_lock_position"));
        assert!(messages[0].contains("\"X\":320"));
        assert!(messages[0].contains("\"Y\":240"));
        assert!(messages[0].contains("\"EXACT\":true"));
    }

    #[tokio::test]
    async fn test_set_lock_position_not_exact() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_lock_position(100.0, 100.0, false).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"EXACT\":false"));
    }

    // ============================================================================
    // Calibration method tests
    // ============================================================================

    #[tokio::test]
    async fn test_is_calibrated_true() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "true")),
        ]);

        client.connect().await.unwrap();
        let calibrated = client.is_calibrated().await.unwrap();
        assert!(calibrated);
    }

    #[tokio::test]
    async fn test_is_calibrated_false() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "false")),
        ]);

        client.connect().await.unwrap();
        let calibrated = client.is_calibrated().await.unwrap();
        assert!(!calibrated);
    }

    #[tokio::test]
    async fn test_get_calibration_data() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(
                1,
                r#"{"calibrated":true,"xAngle":45.0,"xParity":"+","xRate":10.0,"yAngle":135.0,"yParity":"-","yRate":10.0}"#,
            )),
        ]);

        client.connect().await.unwrap();
        let data = client
            .get_calibration_data(CalibrationTarget::Mount)
            .await
            .unwrap();

        assert!(data.calibrated);
        assert!((data.x_angle - 45.0).abs() < 0.01);
        assert_eq!(data.x_parity, "+");
    }

    #[tokio::test]
    async fn test_clear_calibration_mount() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client
            .clear_calibration(CalibrationTarget::Mount)
            .await
            .unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("clear_calibration"));
        assert!(messages[0].contains("\"which\":\"mount\""));
    }

    #[tokio::test]
    async fn test_clear_calibration_both() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client
            .clear_calibration(CalibrationTarget::Both)
            .await
            .unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"which\":\"both\""));
    }

    #[tokio::test]
    async fn test_flip_calibration() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.flip_calibration().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("flip_calibration"));
    }

    // ============================================================================
    // Camera exposure method tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_exposure() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "2000")),
        ]);

        client.connect().await.unwrap();
        let exposure = client.get_exposure().await.unwrap();
        assert_eq!(exposure, 2000);
    }

    #[tokio::test]
    async fn test_set_exposure() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_exposure(3000).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_exposure"));
        assert!(messages[0].contains("3000"));
    }

    #[tokio::test]
    async fn test_get_exposure_durations() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "[100,200,500,1000,2000,3000]")),
        ]);

        client.connect().await.unwrap();
        let durations = client.get_exposure_durations().await.unwrap();

        assert_eq!(durations, vec![100, 200, 500, 1000, 2000, 3000]);
    }

    #[tokio::test]
    async fn test_get_camera_frame_size() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "[640,480]")),
        ]);

        client.connect().await.unwrap();
        let (width, height) = client.get_camera_frame_size().await.unwrap();

        assert_eq!(width, 640);
        assert_eq!(height, 480);
    }

    #[tokio::test]
    async fn test_get_use_subframes_true() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "true")),
        ]);

        client.connect().await.unwrap();
        let use_subframes = client.get_use_subframes().await.unwrap();
        assert!(use_subframes);
    }

    #[tokio::test]
    async fn test_capture_single_frame() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.capture_single_frame(None, None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("capture_single_frame"));
    }

    #[tokio::test]
    async fn test_capture_single_frame_with_exposure() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.capture_single_frame(Some(5000), None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"exposure\":5000"));
    }

    #[tokio::test]
    async fn test_capture_single_frame_with_subframe() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        let subframe = Rect {
            x: 100,
            y: 100,
            width: 200,
            height: 200,
        };
        client
            .capture_single_frame(None, Some(subframe))
            .await
            .unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"subframe\":[100,100,200,200]"));
    }

    // ============================================================================
    // Guide algorithm parameter tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_algo_param_names() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#"["Aggressiveness","MinMove","MaxMove"]"#)),
        ]);

        client.connect().await.unwrap();
        let names = client.get_algo_param_names(GuideAxis::Ra).await.unwrap();

        assert_eq!(names, vec!["Aggressiveness", "MinMove", "MaxMove"]);
    }

    #[tokio::test]
    async fn test_get_algo_param() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0.7")),
        ]);

        client.connect().await.unwrap();
        let value = client
            .get_algo_param(GuideAxis::Ra, "Aggressiveness")
            .await
            .unwrap();

        assert!((value - 0.7).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_set_algo_param() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client
            .set_algo_param(GuideAxis::Dec, "MinMove", 0.3)
            .await
            .unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_algo_param"));
        assert!(messages[0].contains("\"axis\":\"dec\""));
        assert!(messages[0].contains("\"name\":\"MinMove\""));
        assert!(messages[0].contains("\"value\":0.3"));
    }

    // ============================================================================
    // Camera cooling tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_ccd_temperature() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "-10.5")),
        ]);

        client.connect().await.unwrap();
        let temp = client.get_ccd_temperature().await.unwrap();
        assert!((temp - (-10.5)).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_get_cooler_status() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(
                1,
                r#"{"coolerOn":true,"temperature":-20.0,"power":50.0,"setpoint":-20.0}"#,
            )),
        ]);

        client.connect().await.unwrap();
        let status = client.get_cooler_status().await.unwrap();

        assert!(status.cooler_on);
        assert!((status.temperature - (-20.0)).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_set_cooler_state_enable() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_cooler_state(true, Some(-20.0)).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("set_cooler_state"));
        assert!(messages[0].contains("\"enabled\":true"));
        assert!(messages[0].contains("\"temperature\":-20"));
    }

    #[tokio::test]
    async fn test_set_cooler_state_disable() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.set_cooler_state(false, None).await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("\"enabled\":false"));
    }

    // ============================================================================
    // Image operations tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_star_image() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(
                1,
                r#"{"frame":1,"width":31,"height":31,"pixels":"AAAA","star_pos":[15.5,15.5]}"#,
            )),
        ]);

        client.connect().await.unwrap();
        let image = client.get_star_image(15).await.unwrap();

        assert_eq!(image.frame, 1);
        assert_eq!(image.width, 31);
        assert_eq!(image.height, 31);
    }

    #[tokio::test]
    async fn test_save_image() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, r#""/tmp/phd2_image.fits""#)),
        ]);

        client.connect().await.unwrap();
        let path = client.save_image().await.unwrap();

        assert_eq!(path, "/tmp/phd2_image.fits");
    }

    // ============================================================================
    // Application control tests
    // ============================================================================

    #[tokio::test]
    async fn test_shutdown_phd2() {
        let (client, sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_response(1, "0")),
        ]);

        client.connect().await.unwrap();
        client.shutdown_phd2().await.unwrap();

        let messages = sent.lock().unwrap();
        assert!(messages[0].contains("shutdown"));
    }

    // ============================================================================
    // Error handling tests
    // ============================================================================

    #[tokio::test]
    async fn test_rpc_error_handling() {
        let (client, _sent) = create_test_client_with_responses(vec![
            Some(version_event()),
            Some(rpc_error(1, -1, "Equipment not connected")),
        ]);

        client.connect().await.unwrap();
        let result = client.get_app_state().await;

        match result {
            Err(Phd2Error::RpcError { code, message }) => {
                assert_eq!(code, -1);
                assert_eq!(message, "Equipment not connected");
            }
            _ => panic!("Expected RpcError"),
        }
    }

    // ============================================================================
    // Auto-reconnect control tests
    // ============================================================================

    #[tokio::test]
    async fn test_auto_reconnect_enabled_by_default() {
        let factory = Arc::new(MockConnectionFactoryWithPairs::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        assert!(client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_set_auto_reconnect_disabled() {
        let factory = Arc::new(MockConnectionFactoryWithPairs::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.set_auto_reconnect_enabled(false);
        assert!(!client.is_auto_reconnect_enabled());
    }

    #[tokio::test]
    async fn test_toggle_auto_reconnect() {
        let factory = Arc::new(MockConnectionFactoryWithPairs::new());
        let config = Phd2Config::default();
        let client = Phd2Client::with_connection_factory(config, factory);

        client.set_auto_reconnect_enabled(false);
        assert!(!client.is_auto_reconnect_enabled());

        client.set_auto_reconnect_enabled(true);
        assert!(client.is_auto_reconnect_enabled());
    }
}

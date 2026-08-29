use crate::Result;
// `QHYError` is only used on the real-SDK paths (`Sdk::new`, `version`, `Drop`),
// which are `#[cfg]`'d out under `simulation`.
#[cfg(not(feature = "simulation"))]
use crate::QHYError;
use crate::{Camera, FilterWheel, SDKVersion};

#[cfg(not(feature = "simulation"))]
use std::ffi::{c_char, CStr};

#[cfg(feature = "simulation")]
use crate::simulation;

// These imports are only used when NOT simulating (real hardware path in Sdk::new
// / version / Drop); under `simulation` those methods are `#[cfg]`'d out. The FFI
// is called directly here (not behind a per-method backend fork like the camera
// modules), so `sdk.rs` gates itself out under `simulation` rather than forking.
#[cfg(not(feature = "simulation"))]
use crate::sys::{
    GetQHYCCDId, GetQHYCCDSDKVersion, InitQHYCCDResource, ReleaseQHYCCDResource, ScanQHYCCD,
    QHYCCD_ERROR, QHYCCD_SUCCESS,
};

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
/// The representation of the SDK. It automatically allocates the SDK when constructed
/// and automatically frees resource when deconstructed.
///
/// # Lifetime
///
/// Construct **at most one `Sdk` at a time**. `Sdk::new` calls the process-global
/// `InitQHYCCDResource` and [`Drop`] calls `ReleaseQHYCCDResource`; these are NOT
/// reference-counted, so a second live `Sdk` re-inits the global resource and
/// dropping either releases it for both. Hold a single `Sdk` for the program's
/// lifetime.
///
/// # Example
/// ```no_run
/// use qhyccd_rs::Sdk;
///
/// let sdk = Sdk::new().expect("SDK::new failed");
/// let sdk_version = sdk.version().expect("get_sdk_version failed");
/// println!("SDK version: {:?}", sdk_version);
/// let camera = sdk.cameras().last().expect("no camera found");
/// println!("Camera: {:?}", camera);
/// println!("{} filter wheels connected.", sdk.filter_wheels().count());
/// ```
pub struct Sdk {
    cameras: Vec<Camera>,
    filter_wheels: Vec<FilterWheel>,
}

#[allow(unused_unsafe)]
impl Sdk {
    /// Creates a new instance of the SDK
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let sdk = Sdk::new();
    /// assert!(sdk.is_ok());
    /// ```
    #[cfg(not(feature = "simulation"))]
    pub fn new() -> Result<Self> {
        // Retry the CFW-plugged probe a few times (200ms apart), like the
        // reference driver: a single post-init probe occasionally misses the wheel.
        const CFW_PROBE_ATTEMPTS: u32 = 3;
        match unsafe { InitQHYCCDResource() } {
            QHYCCD_SUCCESS => {
                let num_cameras = match unsafe { ScanQHYCCD() } {
                    QHYCCD_ERROR => {
                        let error = QHYError::Sdk { op: "scan_cameras" };
                        tracing::error!(error = ?error);
                        Err(error)
                    }
                    num => Ok(num),
                }?;

                // Capacity is only a hint, so a count too large to address just
                // means no preallocation — the scan below is bounded by that
                // same count.
                let hint = usize::try_from(num_cameras).unwrap_or(0);
                let mut cameras = Vec::with_capacity(hint);
                let mut filter_wheels = Vec::with_capacity(hint);
                for index in 0..num_cameras {
                    let id = {
                        let mut c_id: [c_char; 32] = [0; 32];
                        unsafe {
                            match GetQHYCCDId(index, c_id.as_mut_ptr()) {
                                QHYCCD_SUCCESS => {
                                    let id = CStr::from_ptr(c_id.as_ptr())
                                        .to_str()
                                        .inspect_err(|error| tracing::error!(error = ?error))?;
                                    Ok(id.to_owned())
                                }
                                _ => {
                                    let error = QHYError::Sdk {
                                        op: "get_camera_id",
                                    };
                                    tracing::error!(error = ?error);
                                    Err(error)
                                }
                            }
                        }
                    }?;
                    let camera = Camera::new(id.clone());
                    let mut has_filter_wheel = false;
                    match camera.open() {
                        Ok(()) => {
                            // The CFW-plugged probe is a live transaction over the
                            // camera USB link that `InitQHYCCD` must bring up first;
                            // the reference driver (indi-qhy) calls `InitQHYCCD`
                            // (return-checked) BEFORE `IsQHYCCDCFWPlugged`, and
                            // retries the probe because a single post-init probe
                            // still occasionally misses the wheel. Reachable only on
                            // real hardware — the `simulation` `Sdk::new` decides
                            // `has_filter_wheel` from config and never runs this
                            // open -> init -> probe -> close path.
                            match camera.init() {
                                Ok(()) => {
                                    for attempt in 0..CFW_PROBE_ATTEMPTS {
                                        match camera.is_cfw_plugged_in() {
                                            Ok(true) => {
                                                tracing::trace!(
                                                    "Camera {} reporting a filter wheel",
                                                    id
                                                );
                                                has_filter_wheel = true;
                                                break;
                                            }
                                            Ok(false) => {
                                                if attempt + 1 < CFW_PROBE_ATTEMPTS {
                                                    std::thread::sleep(
                                                        std::time::Duration::from_millis(200),
                                                    );
                                                } else {
                                                    tracing::trace!(
                                                        "Camera {} has no filter wheel",
                                                        id
                                                    );
                                                }
                                            }
                                            Err(error) => {
                                                tracing::error!(error = ?error);
                                                break;
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::error!(
                                        error = ?error,
                                        "init before CFW probe failed; assuming no filter wheel"
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(error = ?error);
                            continue;
                        }
                    }
                    match camera.close() {
                        Ok(()) => (),
                        Err(error) => {
                            tracing::error!(error = ?error);
                            continue;
                        }
                    }
                    if has_filter_wheel {
                        // Share the camera's handle cell (Arc) instead of opening
                        // the same id a second time: a QHY CFW is driven through the
                        // camera handle, and the SDK keeps one open device per id.
                        filter_wheels.push(FilterWheel::new(camera.clone()));
                    }
                    cameras.push(camera);
                }

                Ok(Self {
                    cameras,
                    filter_wheels,
                })
            }
            _ => {
                let error = QHYError::Sdk { op: "init_sdk" };
                tracing::error!(error = ?error);
                Err(error)
            }
        }
    }

    /// Creates a new SDK instance with automatic simulation when the feature is enabled
    ///
    /// When compiled with the `simulation` feature, this automatically returns a simulated
    /// SDK with a default camera (QHY178M-Simulated) that includes a 7-position filter wheel
    /// and cooler support. This allows the same code to work seamlessly in both real and
    /// simulated environments.
    ///
    /// For custom simulated camera configurations, use `new_simulated()` and
    /// `add_simulated_camera()` instead.
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    ///
    /// // Same code works with or without simulation feature
    /// let sdk = Sdk::new().expect("Failed to initialize SDK");
    /// let cameras = sdk.cameras();
    /// println!("Found {} camera(s)", cameras.count());
    /// ```
    #[cfg(feature = "simulation")]
    pub fn new() -> Result<Self> {
        let mut sdk = Self::new_simulated();

        // Add default simulated camera with 7-position filter wheel and cooler
        let config = simulation::SimulatedCameraConfig::default()
            .with_id("SIM-QHY178M")
            .with_model("QHY178M-Simulated")
            .with_filter_wheel(7)
            .with_cooler();

        sdk.add_simulated_camera(config);

        Ok(sdk)
    }

    /// Creates a new SDK instance for simulation without scanning for real hardware
    ///
    /// This creates an empty SDK that can be populated with simulated cameras
    /// using `add_simulated_camera()`.
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// use qhyccd_rs::simulation::SimulatedCameraConfig;
    ///
    /// let mut sdk = Sdk::new_simulated();
    /// let config = SimulatedCameraConfig::default().with_filter_wheel(5);
    /// sdk.add_simulated_camera(config);
    ///
    /// assert_eq!(sdk.cameras().count(), 1);
    /// ```
    #[cfg(feature = "simulation")]
    pub fn new_simulated() -> Self {
        Self {
            cameras: Vec::new(),
            filter_wheels: Vec::new(),
        }
    }

    /// Adds a simulated camera to the SDK
    ///
    /// If the camera configuration includes a filter wheel (filter_wheel_slots > 0),
    /// a corresponding FilterWheel will also be added.
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// use qhyccd_rs::simulation::SimulatedCameraConfig;
    ///
    /// let mut sdk = Sdk::new_simulated();
    /// let config = SimulatedCameraConfig::default()
    ///     .with_id("SIM-001")
    ///     .with_filter_wheel(5);
    /// sdk.add_simulated_camera(config);
    ///
    /// let camera = sdk.cameras().next().unwrap();
    /// assert_eq!(camera.id(), "SIM-001");
    /// ```
    #[cfg(feature = "simulation")]
    pub fn add_simulated_camera(&mut self, config: simulation::SimulatedCameraConfig) {
        let has_filter_wheel = config.filter_wheel_slots > 0;
        let camera = Camera::new_simulated(config);

        if has_filter_wheel {
            // Share the camera's backend state (Arc) with the wheel, mirroring the
            // real path (one handle per device). The camera's own config already
            // carries the CFW controls whenever it has a wheel: `with_filter_wheel`
            // sets both the slot count and the `CfwPort`/`CfwSlotsNum` controls.
            self.filter_wheels.push(FilterWheel::new(camera.clone()));
        }

        self.cameras.push(camera);
    }

    /// Returns an iterator over all cameras found by the SDK
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// for camera in sdk.cameras() {
    ///    println!("Camera: {:?}", camera);
    /// }
    /// ```
    pub fn cameras(&self) -> impl Iterator<Item = &Camera> {
        self.cameras.iter()
    }

    /// Returns an iterator over all filter wheels found by the SDK
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    ///
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// println!("{} filter wheels connected.", sdk.filter_wheels().count());
    /// ```
    pub fn filter_wheels(&self) -> impl Iterator<Item = &FilterWheel> {
        self.filter_wheels.iter()
    }

    /// Returns the version of the SDK
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let sdk_version = sdk.version().expect("get_sdk_version failed");
    /// println!("SDK version: {:?}", sdk_version);
    /// ```
    #[cfg(not(feature = "simulation"))]
    pub fn version(&self) -> Result<SDKVersion> {
        let mut year: u32 = 0;
        let mut month: u32 = 0;
        let mut day: u32 = 0;
        let mut subday: u32 = 0;
        match unsafe {
            GetQHYCCDSDKVersion(&raw mut year, &raw mut month, &raw mut day, &raw mut subday)
        } {
            QHYCCD_SUCCESS => Ok(SDKVersion {
                year,
                month,
                day,
                subday,
            }),
            _ => {
                let error = QHYError::Sdk {
                    op: "get_sdk_version",
                };
                tracing::error!(error = ?error);
                Err(error)
            }
        }
    }

    /// Synthetic version for the simulated SDK — mirrors the SDK release this crate
    /// targets (see `libqhyccd-sys/build.rs`). No FFI is linked under `simulation`.
    #[cfg(feature = "simulation")]
    pub fn version(&self) -> Result<SDKVersion> {
        Ok(SDKVersion {
            year: 26,
            month: 6,
            day: 4,
            subday: 0,
        })
    }
}

#[allow(unused_unsafe)]
impl Drop for Sdk {
    fn drop(&mut self) {
        // Real-SDK cleanup only. Under `simulation` no QHYCCD SDK is linked (every
        // such SDK is simulated), so this whole body is compiled out and `drop` is
        // a no-op.
        #[cfg(not(feature = "simulation"))]
        {
            // Close every still-open camera handle BEFORE releasing the SDK. The
            // QHYCCD manual documents Close-each-then-Release and warns the reverse
            // is unsafe (Close destroys the handle). `HandleCell::Drop` alone is not
            // enough: a consumer may hold a clone of a camera handle (the qhy-camera
            // service does) that outlives this `Sdk`, so its last-drop close would
            // otherwise run AFTER `ReleaseQHYCCDResource`. Closing here on the shared
            // cell sets it to `None`, making any surviving clone's `HandleCell::Drop`
            // a harmless no-op. Because the filter wheel shares its camera's handle
            // cell, closing the cameras also releases the wheels.
            for camera in &self.cameras {
                if let Err(error) = camera.close() {
                    tracing::error!(error = ?error, "failed to close camera during SDK teardown");
                }
            }
            match unsafe { ReleaseQHYCCDResource() } {
                QHYCCD_SUCCESS => (),
                _ => {
                    let error = QHYError::Sdk { op: "close_sdk" };
                    tracing::error!(error = ?error);
                }
            }
        }
    }
}

use crate::Result;
use crate::{Camera, FilterWheel, QHYError, SDKVersion};

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
/// # Two stages
///
/// [`Sdk::new`] **enumerates** and nothing more: it scans the bus and reads each
/// camera's id (`ScanQHYCCD` + `GetQHYCCDId`), and opens no camera. Every camera
/// it found is in [`Sdk::cameras`]. Finding a filter wheel is a second, separate
/// stage, [`Sdk::probe_filter_wheels`], made only for the cameras its caller
/// names: it opens each of them (it never initialises one), so it is not a
/// passive call, and a caller decides which cameras it may touch before making
/// it.
/// [`Sdk::filter_wheels`] lists the wheels that stage found, and nothing until
/// it has run.
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
/// let mut sdk = Sdk::new().expect("SDK::new failed");
/// let sdk_version = sdk.version().expect("get_sdk_version failed");
/// println!("SDK version: {:?}", sdk_version);
/// let camera = sdk.cameras().last().expect("no camera found");
/// println!("Camera: {:?}", camera);
/// let id = camera.id().to_string();
/// sdk.probe_filter_wheels(&[id.as_str()]);
/// println!("{} filter wheels connected.", sdk.filter_wheels().count());
/// ```
pub struct Sdk {
    cameras: Vec<Camera>,
    filter_wheels: Vec<FilterWheel>,
}

/// How many times the CFW-plugged probe is asked before a camera is taken to
/// have no wheel. The reference driver retries because a single probe
/// occasionally misses the wheel; a simulated camera answers from its
/// configuration, the same every time, so it is asked once.
#[cfg(not(feature = "simulation"))]
const CFW_PROBE_ATTEMPTS: u32 = 3;
#[cfg(feature = "simulation")]
const CFW_PROBE_ATTEMPTS: u32 = 1;

/// Probe whether `camera` reports a plugged CFW: open, ask
/// `IsQHYCCDCFWPlugged` up to [`CFW_PROBE_ATTEMPTS`] times 200 ms apart, close.
///
/// A failed open or close is the camera's error. A probe failure is logged and
/// answers `Ok(false)`: the camera is assumed wheel-less.
///
/// **No `InitQHYCCD`.** The reference driver (indi-qhy) inits before it asks,
/// and QHY documents the init as one that may home a filter wheel, which a
/// probe made at a service's startup must not risk. The plug query does not
/// need it: on a QHY178M + CFW3 (Linux, SDK 26.06.04), asked in a process that
/// had run no init, it answered as an init would have (a wheel 3 of 3 with the
/// CFW's 12 V on, none with it off), and a wheel resting off its home slot
/// stayed there.
fn cfw_probe(camera: &Camera) -> Result<bool> {
    let id = camera.id();
    camera.open().inspect_err(|error| {
        tracing::error!(error = ?error, id, "camera open failed before the CFW probe");
    })?;
    let mut has_filter_wheel = false;
    for attempt in 1..=CFW_PROBE_ATTEMPTS {
        match camera.is_cfw_plugged_in() {
            Ok(true) => {
                tracing::trace!("Camera {} reporting a filter wheel", id);
                has_filter_wheel = true;
                break;
            }
            Ok(false) => {
                if attempt < CFW_PROBE_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                } else {
                    tracing::trace!("Camera {} has no filter wheel", id);
                }
            }
            Err(error) => {
                tracing::error!(error = ?error, id, "CFW-plugged probe failed");
                break;
            }
        }
    }
    camera.close().inspect_err(|error| {
        tracing::error!(error = ?error, id, "camera close failed after the CFW probe");
    })?;
    Ok(has_filter_wheel)
}

impl Sdk {
    /// Creates a new instance of the SDK and enumerates the cameras on the bus:
    /// `InitQHYCCDResource`, `ScanQHYCCD`, and `GetQHYCCDId` for each camera
    /// found. **No camera is opened.** [`Sdk::cameras`] lists every camera the
    /// scan found; [`Sdk::filter_wheels`] is empty until
    /// [`Sdk::probe_filter_wheels`] runs.
    /// # Errors
    /// Returns [`QHYError::Sdk`] if SDK initialisation, the camera scan, or
    /// reading a scanned camera's id fails, or [`QHYError::InvalidUtf8`] if
    /// an id is not UTF-8.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let sdk = Sdk::new();
    /// assert!(sdk.is_ok());
    /// ```
    #[cfg(not(feature = "simulation"))]
    pub fn new() -> Result<Self> {
        if unsafe { InitQHYCCDResource() } != QHYCCD_SUCCESS {
            let error = QHYError::Sdk { op: "init_sdk" };
            tracing::error!(error = ?error);
            return Err(error);
        }
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
        for index in 0..num_cameras {
            let id = {
                let mut c_id: [c_char; 32] = [0; 32];
                unsafe {
                    if GetQHYCCDId(index, c_id.as_mut_ptr()) == QHYCCD_SUCCESS {
                        let id = CStr::from_ptr(c_id.as_ptr())
                            .to_str()
                            .inspect_err(|error| tracing::error!(error = ?error))?;
                        Ok(id.to_owned())
                    } else {
                        let error = QHYError::Sdk {
                            op: "get_camera_id",
                        };
                        tracing::error!(error = ?error);
                        Err(error)
                    }
                }
            }?;
            cameras.push(Camera::new(id));
        }

        Ok(Self {
            cameras,
            filter_wheels: Vec::new(),
        })
    }

    /// Creates a new SDK instance with automatic simulation when the feature is enabled
    ///
    /// When compiled with the `simulation` feature, this automatically returns a simulated
    /// SDK with a default camera (QHY178M-Simulated) that has a 7-position filter wheel
    /// and cooler support. As on real hardware, the wheel is found by
    /// [`Sdk::probe_filter_wheels`], not by this call. This allows the same code to work
    /// seamlessly in both real and simulated environments.
    ///
    /// For custom simulated camera configurations, use `new_simulated()` and
    /// `add_simulated_camera()` instead.
    ///
    /// # Errors
    /// Infallible in the simulated build — the `Result` keeps the real
    /// constructor's signature.
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
        sdk.add_simulated_camera(simulation::SimulatedCameraConfig::sdk_default());
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
    #[must_use]
    pub const fn new_simulated() -> Self {
        Self {
            cameras: Vec::new(),
            filter_wheels: Vec::new(),
        }
    }

    /// Adds a simulated camera to the SDK
    ///
    /// A camera configured with a filter wheel (`filter_wheel_slots` > 0) reports
    /// it to [`Sdk::probe_filter_wheels`], which then adds the [`FilterWheel`],
    /// exactly as a real camera's wheel is found.
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
        self.cameras.push(Camera::new_simulated(config));
    }

    /// Finds the filter wheel, if any, on each camera named in `ids` — the
    /// second stage, after [`Sdk::new`]'s enumeration. Each named camera is
    /// opened, asked `IsQHYCCDCFWPlugged` and closed — never initialised, so
    /// this sends no `InitQHYCCD`, the call that may home a wheel. It still
    /// **touches the device**: a second process's open of a camera another
    /// holds disturbs that process's CFW status reads while it lasts, and its
    /// plug query answers no. A caller names only the cameras it owns. A camera
    /// that is not named is not opened.
    ///
    /// Returns one outcome per id, in the order given: `Ok(true)` when a wheel
    /// answered, `Ok(false)` when none did (or the probe failed, which is
    /// logged), and an error when the camera could not be opened or
    /// closed, or when the id names no camera [`Sdk::cameras`] lists
    /// ([`QHYError::UnknownCameraId`]). Each wheel found is added to
    /// [`Sdk::filter_wheels`], sharing its camera's handle; probing a camera
    /// twice adds its wheel once.
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let mut sdk = Sdk::new().expect("SDK::new failed");
    /// let ids: Vec<String> = sdk.cameras().map(|c| c.id().to_string()).collect();
    /// let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    /// for (id, outcome) in sdk.probe_filter_wheels(&ids) {
    ///     println!("{id}: {outcome:?}");
    /// }
    /// ```
    pub fn probe_filter_wheels(&mut self, ids: &[&str]) -> Vec<(String, Result<bool>)> {
        let mut outcomes = Vec::with_capacity(ids.len());
        for &id in ids {
            let outcome = match self.cameras.iter().find(|camera| camera.id() == id) {
                None => Err(QHYError::UnknownCameraId { id: id.to_owned() }),
                Some(camera) => {
                    let outcome = cfw_probe(camera);
                    if matches!(outcome, Ok(true))
                        && !self.filter_wheels.iter().any(|wheel| wheel.id() == id)
                    {
                        // Share the camera's handle cell (Arc) instead of opening
                        // the same id a second time: a QHY CFW is driven through
                        // the camera handle, and the SDK keeps one open device per
                        // id.
                        self.filter_wheels.push(FilterWheel::new(camera.clone()));
                    }
                    outcome
                }
            };
            outcomes.push((id.to_owned(), outcome));
        }
        outcomes
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

    /// Returns an iterator over the filter wheels [`Sdk::probe_filter_wheels`]
    /// has found — none before it runs.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    ///
    /// let mut sdk = Sdk::new().expect("SDK::new failed");
    /// let ids: Vec<String> = sdk.cameras().map(|c| c.id().to_string()).collect();
    /// let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    /// sdk.probe_filter_wheels(&ids);
    /// println!("{} filter wheels connected.", sdk.filter_wheels().count());
    /// ```
    pub fn filter_wheels(&self) -> impl Iterator<Item = &FilterWheel> {
        self.filter_wheels.iter()
    }

    /// Returns the version of the SDK
    /// # Errors
    /// Returns [`QHYError::Sdk`] if the SDK call fails.
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
        let status = unsafe {
            GetQHYCCDSDKVersion(&raw mut year, &raw mut month, &raw mut day, &raw mut subday)
        };
        if status == QHYCCD_SUCCESS {
            Ok(SDKVersion {
                year,
                month,
                day,
                subday,
            })
        } else {
            let error = QHYError::Sdk {
                op: "get_sdk_version",
            };
            tracing::error!(error = ?error);
            Err(error)
        }
    }

    /// Synthetic version for the simulated SDK — mirrors the SDK release this crate
    /// targets (see `libqhyccd-sys/build.rs`). No FFI is linked under `simulation`.
    /// # Errors
    /// Infallible in the simulated build — the `Result` mirrors the real
    /// SDK query.
    #[expect(
        clippy::missing_const_for_fn,
        reason = "const only in this simulated twin; the real `version` calls into the SDK, and the two keep the same signature (the zwo-rs const-twin convention)"
    )]
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
            if unsafe { ReleaseQHYCCDResource() } != QHYCCD_SUCCESS {
                let error = QHYError::Sdk { op: "close_sdk" };
                tracing::error!(error = ?error);
            }
        }
    }
}

//! A camera the unit tests script, answering in the process with no
//! server, so that a test can hold a call open or run on paused time.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ascom_alpaca::api::camera::{ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMResult};

/// A 320 × 240 monochrome camera of 2 µm pixels.
#[derive(Debug)]
pub struct FakeCamera {
    /// `StartExposure` never answers.
    pub start_hangs: bool,
    /// `StartExposure` answers `NOT_CONNECTED`.
    pub start_refused: bool,
    /// What `ImageReady` answers.
    pub ready: AtomicBool,
    /// The frame `ImageArray` answers, as width and height.
    pub frame: (usize, usize),
    pub pixel_size_um: f64,
    /// `AbortExposure` calls.
    pub aborts: AtomicUsize,
    /// Static property reads, counted at `SensorType`.
    pub static_reads: AtomicUsize,
}

impl Default for FakeCamera {
    fn default() -> Self {
        Self {
            start_hangs: false,
            start_refused: false,
            ready: AtomicBool::new(true),
            frame: (320, 240),
            pixel_size_um: 2.0,
            aborts: AtomicUsize::new(0),
            static_reads: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Device for FakeCamera {
    fn static_name(&self) -> &'static str {
        "fake camera"
    }

    fn unique_id(&self) -> &'static str {
        "fake-camera"
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn set_connected(&self, _connected: bool) -> ASCOMResult<()> {
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok("a camera the unit tests script".to_owned())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("guider tests".to_owned())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok("0".to_owned())
    }
}

#[async_trait::async_trait]
impl Camera for FakeCamera {
    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        self.static_reads.fetch_add(1, Ordering::SeqCst);
        Ok(SensorType::Monochrome)
    }

    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        Ok(320)
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        Ok(240)
    }

    async fn max_bin_x(&self) -> ASCOMResult<u8> {
        Ok(2)
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        Ok(Duration::from_secs(3_600))
    }

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        Ok(65_535)
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        Ok(self.pixel_size_um)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        Ok(self.pixel_size_um)
    }

    async fn start_x(&self) -> ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_x(&self, _start_x: u32) -> ASCOMResult<()> {
        Ok(())
    }

    async fn start_y(&self) -> ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_y(&self, _start_y: u32) -> ASCOMResult<()> {
        Ok(())
    }

    async fn start_exposure(&self, _duration: Duration, _light: bool) -> ASCOMResult<()> {
        if self.start_hangs {
            std::future::pending::<()>().await;
        }
        if self.start_refused {
            return Err(ASCOMError::NOT_CONNECTED);
        }
        Ok(())
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        Ok(self.ready.load(Ordering::SeqCst))
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        let (width, height) = self.frame;
        Ok(ImageArray::from(ndarray::Array2::<u16>::zeros((
            width, height,
        ))))
    }

    async fn abort_exposure(&self) -> ASCOMResult<()> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

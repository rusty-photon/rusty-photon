//! What the guider makes of a camera's sensor.
//!
//! How it reads the sensor, the binning of the frame it measures, the frame
//! settings it writes, and the pixel scale (docs/services/guider.md
//! § Sensor handling, § Units).

use ascom_alpaca::api::camera::SensorType;

use crate::config::Config;
use crate::devices::CameraSensor;

/// The arcseconds a micrometre spans at a focal length of one millimetre.
const ARCSEC_PER_UM_PER_MM: f64 = 206.265;

/// `SensorType` as the API names it.
#[must_use]
pub const fn sensor_type_name(sensor_type: SensorType) -> &'static str {
    match sensor_type {
        SensorType::Monochrome => "monochrome",
        SensorType::Color => "color",
        SensorType::RGGB => "rggb",
        SensorType::CMYG => "cmyg",
        SensorType::CMYG2 => "cmyg2",
        SensorType::LRGB => "lrgb",
    }
}

/// The binning of the measured frame: `camera.binning` for a
/// `Monochrome` camera, 2 for an `RGGB` one, whose 2 × 2 cells are summed,
/// and none for a sensor the guider cannot measure.
#[must_use]
pub const fn effective_binning(sensor_type: SensorType, binning: u8) -> Option<u8> {
    match sensor_type {
        SensorType::Monochrome => Some(binning),
        SensorType::RGGB => Some(2),
        SensorType::Color | SensorType::CMYG | SensorType::CMYG2 | SensorType::LRGB => None,
    }
}

/// The pixel scale of the measured frame, in arcseconds a pixel: `None`
/// until the pixel size has been read, for a sensor the guider cannot
/// measure, and with no focal length configured.
#[must_use]
pub fn pixel_scale(config: &Config, sensor: Option<&CameraSensor>) -> Option<f64> {
    let sensor = sensor?;
    let binning = effective_binning(sensor.sensor_type, config.camera.binning.get())?;
    let focal_length = config.optics.focal_length_mm?.get();
    Some(ARCSEC_PER_UM_PER_MM * sensor.pixel_size_um * f64::from(binning) / focal_length)
}

/// How the guider reads a camera it can measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// How the camera's frame becomes the measured frame.
    pub sensor: rp_guiding::SensorType,
    /// The hardware binning written to the camera.
    pub bin: u8,
}

/// Why the guider cannot measure a camera's frames.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Unsupported(String);

/// How the guider reads `sensor` at the configured `binning`: a
/// `Monochrome` camera binned in hardware up to `MaxBinX`, an `RGGB`
/// camera read unbinned.
///
/// # Errors
///
/// [`Unsupported`] for another sensor type, an `RGGB` camera with a
/// binning other than 1, or a binning above `MaxBinX`.
pub fn reading(sensor: &CameraSensor, binning: u8) -> Result<Reading, Unsupported> {
    match sensor.sensor_type {
        SensorType::Monochrome if binning > sensor.max_bin => Err(Unsupported(format!(
            "the camera bins up to {}, and camera.binning is {binning}",
            sensor.max_bin
        ))),
        SensorType::Monochrome => Ok(Reading {
            sensor: rp_guiding::SensorType::Monochrome,
            bin: binning,
        }),
        SensorType::RGGB if binning != 1 => Err(Unsupported(format!(
            "an RGGB camera is read unbinned, and camera.binning is {binning}"
        ))),
        SensorType::RGGB => Ok(Reading {
            sensor: rp_guiding::SensorType::Rggb,
            bin: 1,
        }),
        other @ (SensorType::Color | SensorType::CMYG | SensorType::CMYG2 | SensorType::LRGB) => {
            Err(Unsupported(format!(
                "the guider cannot measure a {} sensor",
                sensor_type_name(other)
            )))
        }
    }
}

/// What the guider writes to the camera when it leaves `idle`, and again
/// after every frame failure (guider.md § The camera loop, "Frame
/// settings").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSettings {
    pub bin: u8,
    /// `NumX` and `NumY`: the full frame at `bin`, rounded down.
    pub width: u32,
    pub height: u32,
    /// Written when set; `None` leaves the camera's value alone.
    pub gain: Option<i32>,
    pub offset: Option<i32>,
}

impl FrameSettings {
    /// The settings of `sensor` read as `reading`, with the configured
    /// gain and offset.
    #[must_use]
    pub fn new(sensor: &CameraSensor, reading: Reading, config: &Config) -> Self {
        let bin = u32::from(reading.bin.max(1));
        Self {
            bin: reading.bin,
            width: sensor.width.checked_div(bin).unwrap_or(0),
            height: sensor.height.checked_div(bin).unwrap_or(0),
            gain: config.camera.gain,
            offset: config.camera.offset,
        }
    }
}

/// What identifies the sensor a loop belongs to: a camera that comes back
/// with another one stops the loop (guider.md § The camera loop, "A
/// camera that comes back changed").
#[derive(Debug, Clone, Copy, PartialEq)]
struct Identity {
    sensor_type: SensorType,
    width: u32,
    height: u32,
    pixel_size_um: f64,
}

impl Identity {
    const fn of(sensor: &CameraSensor) -> Self {
        Self {
            sensor_type: sensor.sensor_type,
            width: sensor.width,
            height: sensor.height,
            pixel_size_um: sensor.pixel_size_um,
        }
    }
}

/// How `now` differs from `then` in the sensor size, the pixel size or the
/// sensor type, as a message; `None` when it does not.
#[must_use]
pub fn changed(then: &CameraSensor, now: &CameraSensor) -> Option<String> {
    let (then, now) = (Identity::of(then), Identity::of(now));
    if then == now {
        return None;
    }
    let mut changes = Vec::new();
    if then.sensor_type != now.sensor_type {
        changes.push(format!(
            "sensor type {} → {}",
            sensor_type_name(then.sensor_type),
            sensor_type_name(now.sensor_type)
        ));
    }
    if (then.width, then.height) != (now.width, now.height) {
        changes.push(format!(
            "sensor size {} × {} → {} × {}",
            then.width, then.height, now.width, now.height
        ));
    }
    if then.pixel_size_um.to_bits() != now.pixel_size_um.to_bits() {
        changes.push(format!(
            "pixel size {} µm → {} µm",
            then.pixel_size_um, now.pixel_size_um
        ));
    }
    Some(format!(
        "the camera came back with another sensor: {}",
        changes.join(", ")
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn sensor(sensor_type: SensorType) -> CameraSensor {
        CameraSensor {
            sensor_type,
            pixel_size_um: 2.0,
            width: 321,
            height: 241,
            max_adu: 65_535,
            max_bin: 2,
            can_abort_exposure: true,
        }
    }

    fn config(binning: u8, focal_length_mm: Option<f64>) -> Config {
        serde_json::from_value(serde_json::json!({
            "camera": { "binning": binning, "gain": 120 },
            "optics": { "focal_length_mm": focal_length_mm },
        }))
        .unwrap()
    }

    #[test]
    fn a_monochrome_camera_is_binned_as_configured() {
        assert_eq!(effective_binning(SensorType::Monochrome, 2), Some(2));
    }

    #[test]
    fn an_rggb_camera_is_summed_two_by_two() {
        assert_eq!(effective_binning(SensorType::RGGB, 1), Some(2));
    }

    #[test]
    fn a_sensor_the_guider_cannot_measure_has_no_binning() {
        for sensor_type in [
            SensorType::Color,
            SensorType::CMYG,
            SensorType::CMYG2,
            SensorType::LRGB,
        ] {
            assert_eq!(effective_binning(sensor_type, 1), None, "{sensor_type:?}");
        }
    }

    #[test]
    fn every_sensor_type_has_its_api_name() {
        let names = [
            SensorType::Monochrome,
            SensorType::Color,
            SensorType::RGGB,
            SensorType::CMYG,
            SensorType::CMYG2,
            SensorType::LRGB,
        ]
        .map(sensor_type_name);
        assert_eq!(
            names,
            ["monochrome", "color", "rggb", "cmyg", "cmyg2", "lrgb"]
        );
    }

    #[test]
    fn a_monochrome_camera_is_binned_in_hardware_up_to_max_bin() {
        let camera = sensor(SensorType::Monochrome);
        assert_eq!(
            reading(&camera, 2).unwrap(),
            Reading {
                sensor: rp_guiding::SensorType::Monochrome,
                bin: 2
            }
        );
        let error = reading(&camera, 3).unwrap_err();
        assert!(error.to_string().contains("bins up to 2"), "{error}");
    }

    #[test]
    fn an_rggb_camera_is_read_unbinned_and_summed() {
        let camera = sensor(SensorType::RGGB);
        assert_eq!(
            reading(&camera, 1).unwrap(),
            Reading {
                sensor: rp_guiding::SensorType::Rggb,
                bin: 1
            }
        );
        let error = reading(&camera, 2).unwrap_err();
        assert!(error.to_string().contains("read unbinned"), "{error}");
    }

    #[test]
    fn a_sensor_type_the_guider_cannot_measure_is_refused() {
        for sensor_type in [
            SensorType::Color,
            SensorType::CMYG,
            SensorType::CMYG2,
            SensorType::LRGB,
        ] {
            let error = reading(&sensor(sensor_type), 1).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "the guider cannot measure a {} sensor",
                    sensor_type_name(sensor_type)
                )
            );
        }
    }

    #[test]
    fn the_frame_settings_are_the_full_frame_at_the_binning_rounded_down() {
        let camera = sensor(SensorType::Monochrome);
        let config = config(2, None);
        let settings = FrameSettings::new(&camera, reading(&camera, 2).unwrap(), &config);
        assert_eq!(
            settings,
            FrameSettings {
                bin: 2,
                width: 160,
                height: 120,
                gain: Some(120),
                offset: None,
            }
        );
    }

    #[test]
    fn the_pixel_scale_is_taken_at_the_effective_binning() {
        let config = config(2, Some(206.265));
        let mono = sensor(SensorType::Monochrome);
        assert_eq!(pixel_scale(&config, Some(&mono)), Some(4.0));
        let rggb = sensor(SensorType::RGGB);
        let config = self::config(1, Some(206.265));
        assert_eq!(pixel_scale(&config, Some(&rggb)), Some(4.0));
    }

    #[test]
    fn the_pixel_scale_is_unknown_without_a_pixel_size_a_focal_length_or_a_measurable_sensor() {
        let config_without = config(1, None);
        assert_eq!(
            pixel_scale(&config_without, Some(&sensor(SensorType::Monochrome))),
            None
        );
        let config = config(1, Some(206.265));
        assert_eq!(pixel_scale(&config, None), None);
        assert_eq!(pixel_scale(&config, Some(&sensor(SensorType::Color))), None);
    }

    #[test]
    fn a_sensor_that_differs_only_in_what_does_not_identify_it_is_the_same() {
        let then = sensor(SensorType::Monochrome);
        let now = CameraSensor {
            max_adu: 4_095,
            max_bin: 4,
            can_abort_exposure: false,
            ..then
        };
        assert_eq!(changed(&then, &now), None);
    }

    #[test]
    fn a_sensor_with_another_type_size_or_pixel_size_has_changed() {
        let then = sensor(SensorType::Monochrome);
        let now = CameraSensor {
            sensor_type: SensorType::RGGB,
            width: 640,
            pixel_size_um: 3.75,
            ..then
        };
        assert_eq!(
            changed(&then, &now).unwrap(),
            "the camera came back with another sensor: sensor type monochrome → rggb, \
             sensor size 321 × 241 → 640 × 241, pixel size 2 µm → 3.75 µm"
        );
    }
}

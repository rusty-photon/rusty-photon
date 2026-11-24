# `guide-simulator-alpaca` Crate Design

[`guide-simulator`](guide-simulator.md)'s mount and sky behind an
ASCOM Alpaca Camera and an ASCOM Alpaca Telescope, on the wall clock. The
`guider` service's BDD scenarios run the service against these devices
([guider.md § Testing](../services/guider.md#testing)): the camera's frames
show the stars where the simulated mount points, and the guider's pulses
move them. A scenario scripts the devices from the test process: it parks
the mount, makes the camera fail its frames or stop answering, and reads
back every pulse and every exposure the guider asked for.

This is test infrastructure: a dev-dependency, `testonly` in Bazel. It
serves plain HTTP on the loopback interface, with no TLS and no
authentication.

## Two servers, one simulation

`SimDevices::start` binds two Alpaca servers on `127.0.0.1`, each on a port
the system picks:

- **the camera's server** serves Camera 0;
- **the mount's server** serves Telescope 0.

Both draw on one `guide-simulator` `Simulator`, so a pulse on the mount
moves the stars in the camera's next frame. They are two servers because
a rig's camera and mount are served by two drivers: a scenario can make
one stop answering, its device list included, while the other answers.
Each server lists its device in `GET /management/v1/configureddevices`
with the `UniqueID` the configuration gives it.

## The wall clock

The simulation's time is the time since `SimDevices::start`, read from
the system's monotonic clock. Each call runs the simulation up to that
time before it acts, so the mount drifts, tracks and ends its pulses as
real time passes, whether anyone calls or not.

**An exposure is rendered when it starts.** `StartExposure` runs the
simulation over the whole exposure at once and keeps the frame. The
simulation's clock then stands at the exposure's end, ahead of the wall
clock, until the wall clock reaches it: the simulation never runs
backward. So a pulse sent during an exposure starts at the exposure's
end and does not show in that frame. The guider never pulses during an
exposure ([guider.md § The camera loop](../services/guider.md#the-camera-loop)),
and a scenario that wants a pulse in a frame sends it before the
exposure.

## The camera

| Member | Behaviour |
|---|---|
| `CameraXSize`, `CameraYSize` | The simulated sensor's width and height |
| `PixelSizeX`, `PixelSizeY` | `camera.pixel_size_um`. The simulation's pixel scale is its own; a scenario chooses the focal length it gives the guider so the two agree |
| `SensorType` | `Monochrome`, or `RGGB` with Bayer offsets 0, as the simulated sensor is |
| `MaxADU` | `camera.max_adu`, which may lie above the sensor's clip, as `qhy-camera`'s does |
| `MaxBinX`, `MaxBinY` | `camera.max_bin` |
| `BinX`, `BinY` | 1 up to `camera.max_bin`, the same on both axes. A binned pixel is the sum of the sensor's pixels in its cell, held to the clip |
| `StartX`, `StartY`, `NumX`, `NumY` | The region, in binned pixels. A connect sets the full frame at bin 1 |
| `Gain`, `Offset` | Kept and reported, 0 to 1,000; they change nothing in the frame |
| `CanAbortExposure` | `camera.can_abort_exposure` |
| `ExposureMin`, `ExposureMax` | 1 ms and one hour |
| `StartExposure` | Renders the frame and keeps it. Refused for a dark frame (`Light` false), for a duration outside `ExposureMin` to `ExposureMax`, while an exposure is in progress, and for a region that leaves the binned sensor |
| `ImageReady` | True from `camera.readout` after the exposure's end until the next `StartExposure` |
| `ImageArray` | The frame as 16-bit values, the region cut from the binned sensor |
| `AbortExposure` | Drops the exposure in progress, and does nothing when none is |
| `CameraState` | `Exposing` until the exposure's end, `Reading` until `ImageReady`, `Idle` otherwise |

A camera that is not connected answers every Camera member with
`NOT_CONNECTED`. A connect starts it at the full frame at bin 1, with no
frame.

## The mount

| Member | Behaviour |
|---|---|
| `CanPulseGuide` | `mount.can_pulse_guide`, which a scenario can change |
| `Tracking` | The simulated mount's. Writing it turns tracking on or off; turning it on while parked is refused with `INVALID_WHILE_PARKED` |
| `Slewing`, `AtPark` | The simulated mount's |
| `Park`, `Unpark` | Park at once, and unpark with tracking off, as the model does |
| `SideOfPier` | `pierEast` or `pierWest`. With `mount.reports_pier_side` false it is `pierUnknown` |
| `RightAscension`, `Declination` | Where the mount points: `mount.right_ascension_hours` and the simulated declination at the start, moved by the pointing |
| `SiderealTime` | `mount.right_ascension_hours` at the start, so the mount points at the meridian, advancing at the sidereal rate |
| `Altitude`, `Azimuth` | `mount.altitude_deg` and `mount.azimuth_deg`, fixed: the guider reads them only for its guide log |
| `GuideRateRightAscension`, `GuideRateDeclination` | The simulated guide rates in degrees a second. With `mount.reports_guide_rates` false both answer `NOT_IMPLEMENTED` |
| `PulseGuide` | Starts the pulse on the simulated mount. Refused while `CanPulseGuide` is false (`NOT_IMPLEMENTED`), while parked (`INVALID_WHILE_PARKED`), and while slewing or while a pulse on the same axis is in progress (`INVALID_OPERATION`) |
| `IsPulseGuiding` | Whether a pulse on either axis is in progress |

A mount that is not connected answers every Telescope member with
`NOT_CONNECTED`. The mount slews only when a scenario slews it: it has no
Alpaca slew members.

## What a scenario scripts

`SimDevices`' methods change the devices while they run and read back
what they were asked:

- **Connections.** `set_connected` sets a device's `Connected`, as `rp`
  sets it on a rig. A client can write it too.
- **A server that stops answering.** `set_reachable(device, false)` makes
  every request to that device's server, its device list included,
  answer `503 Service Unavailable` with no Alpaca body, until it is set
  back.
- **Failed frames.** `fail_frames` makes exposures fail, for a count or
  until `clear_frame_faults`, in one of three ways: `StartExposure`
  answers an error; the frame never becomes ready; or `ImageArray`
  answers a frame one pixel narrower than the region asked for.
- **The mount.** `slew` moves the pointing over a duration and ends on a
  pier side; `park`, `set_tracking` and `set_can_pulse_guide` change
  what the mount check reads; `set_rate_factors` makes pulses move the
  mount less, or the wrong way; `set_pulses_stuck` keeps `IsPulseGuiding`
  true from the next pulse the mount accepts, past its end, until it is
  released.
- **What was asked.** `pulses` lists every pulse the mount accepted, with
  its direction, its duration and the simulated time it started.
  `exposures` lists every exposure the camera started, with its
  duration, binning, region, gain and offset. `last_truth` is the truth
  of the latest frame rendered.

## Testing

The crate's tests start the devices and drive them through the Alpaca
client, as the guider does: each device's listing and `UniqueID`, the
refusals of a device that is not connected, an exposure from start to
image, binning and the region, a pulse that moves the stars in the next
frame and ends on the wall clock, the mount check's reads after a park
and a slew, and each scripted fault.

## Consumers

The `guider` service's BDD scenarios, as
[guider.md § Testing](../services/guider.md#testing) designs them.

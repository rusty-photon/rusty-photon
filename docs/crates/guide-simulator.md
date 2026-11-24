# `guide-simulator` Crate Design

A guide camera's view through a mount, in simulated time. A mount model
drifts, wobbles and answers guide pulses the way a real mount does, and a
renderer draws the stars the camera would see through it. The guiding
core's tests close the loop through the simulator: it renders a frame, the
core measures it and sends a pulse, and the simulator applies the pulse.
Hours of simulated night run in seconds, and the true offset of every frame
is known ([guider.md § Testing](../services/guider.md#testing)).

This is test infrastructure: a dev-dependency, `testonly` in Bazel. It
depends on `ndarray`, `rand`, `num-traits` and `thiserror`. It has no
runtime, does no I/O and reads no clock.

## Scope

- **The mount model:** where the telescope points over time, under drift,
  periodic error and tracking. It takes guide pulses as a mount delivers
  them, short ones included, and models Dec backlash, slews, parking and
  the pier side.
- **The sky:** a star field, seeing and clouds.
- **The renderer:** a frame from a monochrome or RGGB sensor, integrated
  over its exposure, with the noise and defects of a real one. Each frame
  carries its truth.

Out of scope:

- **The Alpaca devices.** The Alpaca Camera and Telescope that put the
  simulator behind the guider's own clients, on the wall clock, are
  [`guide-simulator-alpaca`](guide-simulator-alpaca.md)'s.
- **Hardware binning and subframes.** The guider reads full frames, and a
  test that wants binned pixels sets a coarser pixel scale.
- **Other physics:** field rotation, dark current, refraction, and any star
  image but a Gaussian.
- **RA backlash.** Tracking loads the RA axis, and a guide pulse only
  changes its speed, never its direction.

## Simulated time and randomness

- **One clock.** `SimTime` is an instant of the simulation, a `Duration`
  since its start. The simulator has one clock, the mount's, and only the
  caller moves it: with `advance`, with `expose`, which ends at the
  exposure's end, or with the mount's `advance_to`. Time never runs
  backward: advancing to an instant that has passed does nothing.
- **One seed.** The seed sets every random draw, so the same seed gives the
  same run. The generator is `rand::rngs::Xoshiro256PlusPlus`, a named
  algorithm, not one of the defaults `rand` may replace in a later
  version.
- **Separate streams.** The mount, the star placement, the seeing and the
  pixel noise each draw from a stream of their own, all derived from the
  seed. A change in what one of them draws, such as a bigger frame or more
  stars, leaves the others' draws as they were.

## Coordinates and units

- **The sky.** A `SkyOffset` is an offset on the sky in arcseconds, `east`
  toward increasing RA and `north` toward increasing Dec.
- **The pointing.** The pointing is where the telescope points, relative to
  where it pointed at the start, as a `SkyOffset`. A star's place in the
  field is its sky position less the pointing.
- **The RA axis.** The guide rate, a pulse's travel and periodic error are
  in arcseconds of the RA axis. An arcsecond of axis moves the sky by the
  cosine of the declination, as on a real mount.
- **The sensor.** A `SensorPoint` is in pixels. The centre of the pixel in
  row `r` and column `c` lies at `x = c`, `y = r`, the convention of
  `rp-image-analysis`. A frame is an `Array2<u16>` indexed `[[y, x]]`.

A sky offset lands on the sensor at:

```
(u, v) = (east / scale, parity · north / scale)    parity = −1 when mirrored, else +1
θ      = angle, plus 180° on the pier side the mount did not start on
x      = x₀ + u·cos θ − v·sin θ
y      = y₀ + u·sin θ + v·cos θ
```

`scale` is the pixel scale in arcseconds per pixel, and `(x₀, y₀)` is the
frame's centre, `((width − 1) / 2, (height − 1) / 2)`. With the angle at 0
and no mirror, a West pulse moves the pointing West and the stars toward
+x, and a North pulse moves the stars toward −y.

## The mount model

The pointing is the sum of the contributions below. Each runs in simulated
time as the clock moves.

### Tracking, drift and periodic error

- **Drift** is a constant rate on each sky axis while tracking, in
  arcseconds per minute: what a polar-alignment error and a tracking-rate
  error do over a run.
- **Periodic error** is a sum of sine terms on the RA axis. Each term has
  an amplitude in arcseconds (half the peak-to-peak), a period and a phase.
  Its time is tracked time: the worm turns only while the mount tracks.
- **With tracking off**, the pointing moves East at the sidereal rate,
  15.041″ of RA axis a second: its hour angle stands still while the sky
  turns, so its RA rises. The stars drift West out of the field, as they
  do on a mount that has stopped. Drift and periodic error stand still.

### Guide pulses

`pulse(direction, duration)` starts a pulse at the current instant.

- **Motion.** North and South move the pointing in Dec, East and West in
  RA. The speed is the guide rate the mount reports, a fraction of
  sidereal, times the axis's rate factor. A factor other than 1 is a mount
  whose guide rate differs from the one it reports. A factor of 0 is a
  mount that does not follow the pulses, and a negative one is a mount
  that moves the wrong way. The factor scales all of a pulse's motion,
  including the steps below. `set_rate_factors` changes both factors for
  the pulses that follow, as when a mount stops following its pulses
  part way through a run.
- **Tracking off.** An RA pulse starts the stopped axis at the pulse's
  rate, as the GTi's driver does, so for its length the pointing moves
  against the sky as it does while tracking, and the stopped mount's
  sidereal motion pauses. A Dec pulse is the same either way. The GTi
  likewise accepts a pulse with tracking off.
- **Refusals.** A pulse is refused while the mount is parked (`Parked`),
  while it slews (`Slewing`), and while a pulse on the same axis is in
  progress (`AxisBusy`). A pulse on the other axis runs alongside.
- **Zero duration.** A pulse of zero duration is accepted and does
  nothing, even on a busy axis, as the GTi's driver accepts it; parked or
  slewing, it is refused like any other.
- **`is_pulse_guiding`** is true while a pulse on either axis is in
  progress: from its start to the end of the length the mount delivers.

### How a mount delivers a pulse

By default a pulse moves its axis at the guide rate for exactly the
duration asked. A `PulseDelivery` models what a real mount does to short
pulses instead. Its RA part applies while the mount tracks: an RA pulse
that starts the axis from rest is delivered as asked, as the GTi's driver
neither trims it nor gets the edge steps of a running motor.

- **RA, East and West each.** The delivered length is the requested length
  plus `lengthened_by`, less `shortened_by`, and at least `shortest`. The
  axis moves at the guide rate for the delivered length, then by a fixed
  `step_arcsec` when the pulse ends, in arcseconds of axis. A positive step
  is along the pulse's direction, a negative one against it.
- **Dec.** Dec moves continuously unless `step_arcsec` is set. With a step,
  the Dec axis moves in whole steps of that size. Its motor runs at the
  guide rate, and the axis moves one step each time the motor crosses a
  step boundary. The motor's position carries over from pulse to pulse,
  starting at a random fraction of a step, so a pulse shorter than a step
  moves the axis one step or none. Over many pulses the axis moves as far
  as the pulses asked.
- **A late last step.** With `late_last_step`, the last step of a Dec pulse
  no longer than its `max_pulse` arrives late: a random delay after the
  pulse ends, uniform between `earliest` and `latest`. By then
  `is_pulse_guiding` reads false.

`PulseDelivery::star_adventurer_gti()` is the Star Adventurer GTi as
[star-adventurer-gti.md § PulseGuide lifecycle](../services/star-adventurer-gti.md#pulseguide-lifecycle)
records it. The GTi was measured at its park pose, with the driver's
default trim and a guide rate of 0.5 × sidereal:

| | East | West | Dec |
|---|---|---|---|
| Delivered length | requested + 71 ms | requested − 71 ms, at least 2 ms | as requested |
| Step | 0.40″ against the pulse | 1.09″ with the pulse | whole steps of 0.446″, one per 59 ms at that guide rate |
| Late | — | — | the last step of a pulse up to 100 ms, 0.1–1.9 s after the pulse ends |

The 71 ms is the driver's trim. The two RA steps are the board's mean edge
steps at that pose, 1.13 and 3.06 ticks of 0.357″. At every RA length the
rig timed, from 20 ms to 1 s, the preset delivers within 0.3″ of the mean
the rig measured. The preset leaves out three things the rig showed: the
edge steps change with the RA axis's position, the Dec count flickers at
rest, and the rig's falling Dec pulses ran one count further than its
rising ones.

### Dec backlash

`dec_backlash.play_arcsec` is the play in the Dec gear. When Dec reverses,
the motor moves that far before the axis follows. `dec_backlash.meshed`
says which way the gear starts meshed. Meshed for North, a first North
pulse moves the axis at once, and a first South pulse first takes up the
play. Drift and periodic error do not pass through the gear.

### Slews, parking and the pier side

- **`slew(by, duration, pier_side)`** moves the pointing by `by` at an even
  pace over `duration`. `slewing` reads true until the slew ends, and from
  its end the pier side is `pier_side`. A slew stops any pulse in progress
  where it stands, and drops the rest of the pulse's motion and any Dec
  step still to come, a late one included. A slew that starts during
  another replaces it from where the first one stands. A parked mount
  refuses to slew (`Parked`).
- **`park()`** parks at once: `at_park` reads true, tracking turns off,
  and pulses and a slew stop where they stand, a Dec step still to come
  dropped as a slew drops it. **`unpark()`** clears `at_park`
  and leaves tracking off. Neither moves the pointing; with tracking off,
  the sky moves instead.
- **`set_tracking(on)`** stops an RA pulse in progress where it stands,
  even when tracking stays as it was, as every `Tracking` write does on the
  GTi's driver. A parked mount refuses to turn tracking on
  (`Parked`), as the GTi's driver refuses it, and may turn it off.
- **The declination** is the configured one for the whole run. A slew
  moves the pointing but not the declination the RA axis scales by; a
  test at another declination builds a simulator of its own, as the
  guider reads the declination once a run.

## The sky

### Stars

A star field is either listed or random:

- **Listed.** Each star is a sky position, relative to the pointing at the
  start, and a brightness in electrons per second at full transmission,
  summed over its image and before any Bayer response.
- **Random.** A count, with brightnesses drawn log-uniform between the
  faintest and the brightest, and positions drawn uniform over the frame
  as it lies at the start.

### Seeing

- **`fwhm_arcsec`** is the full width at half maximum of the Gaussian star
  image.
- **`common_motion_arcsec`** and **`star_motion_arcsec`** are seeing's image
  motion, averaged over an exposure. In each frame the whole field moves by
  a random offset with the first as its standard deviation on each sky
  axis, and each star by a further offset of its own with the second. A
  star's own motion is what measuring several stars together averages
  away.

### Clouds

A cloud has a start, a duration, a ramp and a transmission. Over the ramp
from its start, the stars' light falls to the transmission. It stays there,
and climbs back over the ramp before the cloud's end. The ramp is at most
half the duration.

- Clouds that overlap multiply.
- A transmission of 0 hides every star.
- A long cloud with a high transmission is a lasting change of brightness,
  such as thin cloud settling in.
- Clouds dim the stars only. The sky background stays as it is.

## The renderer

### The exposure

A frame covers its exposure, from its start for its duration. The
simulator splits the exposure into equal slices of at most 50 ms. At the
middle of each slice it samples the pointing, the pier side and the
transmission, and draws each star with the slice's share of its light. A
star that moves during the exposure trails as it would on a real sensor.

### The image

- **Star images.** A star's image is a circular Gaussian of the seeing's
  FWHM. Its light is integrated over each pixel's square with the error
  function, out to four standard deviations on each axis. The light beyond
  is dropped, 0.013 % of the star's.
- **Sensor type.** The sensor is `Monochrome` or `Rggb`. An `Rggb` sensor
  has a response for each of red, green and blue, and each pixel's light is
  multiplied by its colour's response. Red sits at an even row and an even
  column, blue at an odd row and an odd column, and green at the other two.
- **Sky background.** `sky_e_per_pixel_per_s` electrons per pixel per
  second, through the Bayer response as well.
- **Hot pixels.** `hot_pixels` of them, at random positions fixed for the
  run. Each gains `hot_pixel_e_per_s` electrons per second, outside the
  Bayer response.
- **Noise.** Each pixel's electrons are drawn from a Poisson distribution
  about the expected count, and Gaussian read noise of `read_noise_e` is
  added. At 20 expected electrons or more, both are drawn together as one
  Gaussian of the combined variance.
- **Conversion.** The ADU value is the electrons divided by
  `gain_e_per_adu`, plus `bias_adu`, rounded and held within
  `0..=full_scale_adu`. A pixel at `full_scale_adu` is clipped: saturated.

### The truth of a frame

Besides its pixels, a frame carries its truth:

- its start and duration;
- the pointing, averaged over the exposure;
- the field's shift on the sensor: where a star that sat at the frame's
  centre at the start now lies, less the frame's centre, in pixels. This is
  the pointing's effect alone, without seeing;
- the transmission, averaged over the exposure;
- for each star whose position lies on the frame: its index in the field,
  its position with seeing included, and the electrons of its light that
  land on the sensor, before any Bayer response. A star near the edge
  loses what falls off it.

The field's shift and the stars' positions are averaged over the exposure
with the light as the weight, as the pixels' centroid weighs them, so a
star that moves while a cloud passes is where its light puts it. With no
light at all they are plain averages.

The field's shift is what a guider should measure. The stars' positions are
what it can measure, given the seeing.

## Errors

- **`ConfigError`** names the field it rejects. Construction refuses a
  zero frame size; a pixel scale, gain, seeing FWHM, Dec step or
  periodic-error period that is not positive; a read noise, sky
  background, hot-pixel rate, Bayer response, motion, backlash or guide
  rate that is negative; a declination outside ±90°; a random star field
  whose faintest brightness is not positive or exceeds its brightest; a
  cloud transmission outside 0 to 1, or a ramp longer than half the
  cloud; a late step with no Dec step, or whose earliest delay exceeds its
  latest; and any value that is not finite.
- **`MountError`** is a refused pulse, slew or start of tracking: `Parked`,
  `Slewing` or `AxisBusy`.

## Testing

Unit tests live with the code:

- **The mount:** each contribution to the pointing, each delivery rule,
  backlash, the refusals, slews and parking. The GTi preset is checked
  against the table the rig measured.
- **The renderer:** the light of a star is conserved, a bright star's
  centroid lies where the truth puts it, the noise of a blank frame has the
  expected mean and spread, the clip, the Bayer pattern and the hot pixels.
- **The simulator:** the same seed gives the same frames, an exposure moves
  the clock by its length, a pulse during an exposure shifts the field by
  what the truth reports, and a cloud dims the stars.

## Consumers

The guiding core's tests, `rp-guiding`'s; the replay's tests,
[`guide-replay`](guide-replay.md)'s; and the `guider` service's BDD
scenarios behind [`guide-simulator-alpaca`](guide-simulator-alpaca.md)'s
devices, as [guider.md § Testing](../services/guider.md#testing) designs
them.

Feature: The guider's stats and its per-frame metrics
  `GET /api/v1/guiding/stats` reports the state, the pixel scale, the
  latest measured frame's signal-to-noise ratio, flux and star count, the
  latest stop and the running totals (docs/services/guider.md
  § `GET /api/v1/guiding/stats`, § Units, § Statistics). The pixel scale
  is `206.265 × PixelSizeX × effective binning ÷ optics.focal_length_mm`,
  `null` until the pixel size has been read. The frame's values are `null`
  when no frame has been measured since the loop started. The RMS fields
  are `null` while the RMS window holds no guide step.

  `GET /api/v1/guiding/metrics` returns the last 50 measured frames,
  oldest first, under the `epoch` of the running totals
  (§ `GET /api/v1/guiding/metrics`, § Metrics). `frame` counts the frames
  measured since the service started, and the window is cleared by every
  `loop` that starts the loop from `idle`.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels, binning up to 2, under a sky of 6″ seeing, and a mount, each on
  a loopback server of its own. The guider polls them every 100 ms and
  exposes for 200 ms. A scenario that lists its stars places them well
  inside the frame and apart from each other.

  Scenario: The stats of a guider that has measured nothing
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When the guider reports the camera connected
    Then the guider's stats report:
      | field              | value  |
      | state              | "idle" |
      | guiding            | false  |
      | settling           | false  |
      | pixel_scale_arcsec | 2.0    |
      | rms_ra_px          | null   |
      | rms_dec_px         | null   |
      | total_rms_px       | null   |
      | rms_ra_arcsec      | null   |
      | rms_dec_arcsec     | null   |
      | total_rms_arcsec   | null   |
      | snr                | null   |
      | star_mass          | null   |
      | star_count         | null   |
      | sample_count       | 0      |
      | last_stop          | null   |
    And the guider's stats report running totals of zero under an epoch

  Scenario Outline: The pixel scale is taken at the binning of the measured frame
    Given the simulated camera and mount, both connected
    And the simulated camera has a <sensor> sensor
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/binning" is the JSON <binning>
    And the guider is started
    When the guider reports the camera connected
    Then the guider's stats report:
      | field              | value   |
      | pixel_scale_arcsec | <scale> |

    Examples:
      | sensor     | binning | scale |
      | monochrome | 2       | 4.0   |
      | RGGB       | 1       | 4.0   |

  Scenario: The pixel scale is null until the camera's pixel size has been read
    Given the simulated camera and mount, neither connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When the guider reports the camera reachable and not connected
    Then the guider's stats report:
      | field              | value |
      | pixel_scale_arcsec | null  |

  Scenario: The stats of a loop report its latest frame
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When the guider is looping
    Then the guider's stats report:
      | field      | value     |
      | state      | "looping" |
      | guiding    | false     |
      | star_count | 3         |
    And the guider's stats report a positive snr and star_mass

  Scenario: The metrics hold each measured frame with its stars' values
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When the guider is looping
    Then the guider's metrics hold at least 3 frames, each with 3 usable stars and a positive hfd, snr and star_mass
    And the guider's metrics number their frames one after another, oldest first
    And the guider's metrics report the state "looping" under the epoch of the running totals

  Scenario: The metrics window holds the last 50 frames
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "1ms"
    And the guider is started
    When the guider is looping
    Then the guider's metrics come to hold 50 frames and no more

  Scenario: A loop from idle clears the metrics window, and the frame count goes on
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    And the guider's metrics hold at least 3 frames
    And a client asks the guider to stop
    When a client asks the guider to loop
    Then the request answers 200 with the state "looping"
    And the guider's metrics hold only frames numbered above those they held before the request

  Scenario: A stop's reason and time are reported as the last stop
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    When a client asks the guider to stop
    Then the guider's last stop has the reason "requested" and a time in RFC 3339

@serial
Feature: Rig state on the exposure document
  Every frame's sidecar records what the rest of the rig was doing while
  the frame was exposed, since each of these facts is lost once the rig
  moves on (rp.md, Core Fields: Rig state): the mount's pier side,
  tracking, altitude and azimuth and the airmass they put the frame at,
  each focuser and rotator in the camera's train, the position angle the
  frame was meant to be taken at, what the guider did over the exposure,
  and the readings of the site's observing-conditions devices and safety
  monitors. They are copied in at capture time. A device's name, and a
  rotator's step size, are what it said about itself when its session
  was established. The sidecar is the record; the FITS header exports
  most of these facts under the cards capture_fits_header.feature pins.

  A device that is not configured or not connected, or a read that
  fails, leaves its field out rather than writing a placeholder, and no
  rig-state read fails a capture.

  The whole rig is a camera behind a focuser, a rotator and a filter
  wheel in train "main", a mount, an observing conditions device
  "sim-weather" and a safety monitor "weather-watcher", all on the
  simulator, at a configured site. The simulator's mount starts at its
  home position, on the east side of the pier, due south at 90 degrees
  less the site's latitude, 38.92 degrees above the horizon, where
  Gueymard's formula gives an airmass of 1.589. Its rotator moves in
  steps of 0.75 degrees. Its observing conditions
  device reports every ASCOM sensor, and its safety monitor starts safe.
  The simulator lets its focuser's probe and its weather readings drift,
  so where a table says "a number", the frame records whatever was read.

  The guider is a stub of the guider service whose running totals grow
  by the same amount on every stats read. A capture reads them twice,
  once as its exposure starts and once when its image is ready, so a
  frame's guiding block covers one read's growth.

  Scenario Outline: A frame records the mount's pier side and whether it was tracking
    Given rp is running with the whole rig on the simulator
    And the mount tracking is set to <tracking>
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "pier_side" should be "East"
    And the document at "/tracking" should be the JSON <tracking>

    Examples:
      | tracking |
      | true     |
      | false    |

  Scenario: A frame records the mount's altitude and azimuth and the airmass at that altitude
    Given rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/altitude_degrees" should be 38.92 within 0.01
    And the document at "/azimuth_degrees" should be 180.0 within 0.01
    And the document at "/airmass" should be 1.589 within 0.001

  Scenario: A frame records the name and position of each focuser and rotator in its train
    Given rp is running with the whole rig on the simulator
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25200
    And the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    And the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/focusers/0/id" should be the JSON "main-focuser"
    And the document at "/focusers/0/name" should be the JSON "Alpaca Focuser Simulator"
    And the document at "/focusers/0/position" should be the JSON 25200
    And the document at "/focusers/0/temperature_c" should be a number
    And the document at "/rotators" should be the JSON [{"id": "main-rotator", "name": "Alpaca Rotator Simulator", "angle_degrees": 10.0, "mechanical_angle_degrees": 10.0, "step_size_degrees": 0.75}]

  Scenario: A rotator whose session is down is left out of the frame's rotators
    Given rp is running with the whole rig on the simulator and its rotator offline
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document should have no value at "/rotators"
    And the document at "/focusers/0/id" should be the JSON "main-focuser"

  Scenario: A frame of a target is intended at the target's own position angle
    Given rp is running with the whole rig on the simulator and default position angle 254.0
    And the MCP client has added the always-visible target "M31" with position angle 30.0
    When the MCP client calls "capture" with camera "main-cam" for 100 ms, the added target, and frame_type "Light"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/intended_position_angle_degrees" should be the JSON 30.0

  Scenario: A frame of a target without an angle is intended at the train's default
    Given rp is running with the whole rig on the simulator and default position angle 254.0
    And the MCP client has added a target named "M31" at ra_hours 0.7123 dec_degrees 41.2689
    When the MCP client calls "capture" with camera "main-cam" for 100 ms, the added target, and frame_type "Light"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/intended_position_angle_degrees" should be the JSON 254.0

  Scenario: A frame of no target is intended at the train's default
    Given rp is running with the whole rig on the simulator and default position angle 254.0
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/intended_position_angle_degrees" should be the JSON 254.0

  Scenario: A frame of a target with no angle anywhere is intended north-up
    Given rp is running with the whole rig on the simulator
    And the MCP client has added a target named "M31" at ra_hours 0.7123 dec_degrees 41.2689
    When the MCP client calls "capture" with camera "main-cam" for 100 ms, the added target, and frame_type "Light"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/intended_position_angle_degrees" should be the JSON 0.0

  Scenario: A frame records every sensor of each observing-conditions device
    Given rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/observing_conditions" should hold 1 entry
    And the document at "/observing_conditions/0" should carry these fields:
      | field                       | json          |
      | id                          | "sim-weather" |
      | temperature_c               | a number      |
      | humidity_pct                | a number      |
      | dew_point_c                 | a number      |
      | pressure_hpa                | a number      |
      | cloud_cover_pct             | a number      |
      | rain_rate_mm_per_hour       | a number      |
      | sky_brightness_lux          | a number      |
      | sky_quality_mag_per_arcsec2 | a number      |
      | sky_temperature_c           | a number      |
      | star_fwhm_arcsec            | a number      |
      | wind_direction_degrees      | a number      |
      | wind_speed_m_per_s          | a number      |
      | wind_gust_m_per_s           | a number      |

  Scenario: A frame records each safety monitor's reading
    Given rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/safety_monitors" should be the JSON [{"id": "weather-watcher", "is_safe": true}]

  Scenario: A dark taken while a monitor reads unsafe records the unsafe reading
    Given rp is running with the whole rig on the simulator
    When the safety monitor reports unsafe
    And the MCP client calls "capture" with camera "main-cam" for 100 ms and frame_type "Dark"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/safety_monitors" should be the JSON [{"id": "weather-watcher", "is_safe": false}]

  Scenario: A guided frame records the guide error over its exposure
    Given a guider service reporting "Guiding" whose running totals grow on every read by:
      | guide_steps | ra_sum_sq_px2 | dec_sum_sq_px2 | star_lost | interruptions |
      | 10          | 5.625         | 10.0           | 1         | 0             |
    And rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/guiding" should be the JSON {"guided_throughout": true, "guide_steps": 10, "rms_ra_px": 0.75, "rms_dec_px": 1.0, "total_rms_px": 1.25, "star_lost": 1}

  Scenario: A frame whose guiding was interrupted records the guide steps it got
    Given a guider service reporting "Guiding" whose running totals grow on every read by:
      | guide_steps | ra_sum_sq_px2 | dec_sum_sq_px2 | star_lost | interruptions |
      | 10          | 5.625         | 10.0           | 0         | 1             |
    And rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/guiding" should be the JSON {"guided_throughout": false, "guide_steps": 10, "rms_ra_px": 0.75, "rms_dec_px": 1.0, "total_rms_px": 1.25, "star_lost": 0}

  Scenario: A frame taken while PHD2 was not guiding records no guide steps
    Given a guider service reporting "Stopped" whose running totals grow on every read by:
      | guide_steps | ra_sum_sq_px2 | dec_sum_sq_px2 | star_lost | interruptions |
      | 0           | 0.0           | 0.0            | 0         | 0             |
    And rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/guiding" should be the JSON {"guided_throughout": false, "guide_steps": 0, "star_lost": 0}

  Scenario: A guider service that restarted during the exposure records only that guiding was not throughout
    Given a guider service reporting "Guiding" that starts a new epoch on every read, its running totals growing on every read by:
      | guide_steps | ra_sum_sq_px2 | dec_sum_sq_px2 | star_lost | interruptions |
      | 10          | 5.625         | 10.0           | 0         | 0             |
    And rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document at "/guiding" should be the JSON {"guided_throughout": false}

  Scenario: A guider service without running totals leaves the guiding block out
    Given a guider service reporting "Guiding" without running totals
    And rp is running with the whole rig on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document should have no value at "/guiding"

  Scenario: A camera outside every train, with no other device, records no rig state
    Given rp is running with a camera on the simulator
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document should have no value at any of these paths:
      | path                             |
      | /pier_side                       |
      | /tracking                        |
      | /altitude_degrees                |
      | /azimuth_degrees                 |
      | /airmass                         |
      | /focusers                        |
      | /rotators                        |
      | /intended_position_angle_degrees |
      | /guiding                         |
      | /observing_conditions            |
      | /safety_monitors                 |

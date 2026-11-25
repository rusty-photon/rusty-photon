Feature: Calibrating
  `POST /api/v1/calibrate` measures, for the pier side the mount is on, the
  direction and the speed with which a West and a North pulse move the
  stars on the sensor, and answers with the record (docs/services/guider.md
  § Calibration, § `POST /api/v1/calibrate`). Each step is one pulse, then
  one frame: West until the stars have moved `calibration.distance_px`,
  East as many steps back, North to take up the backlash, North the
  distance, South as many steps back. The guider returns to the state the
  request found it in. Before every pulse, and once in every cycle that
  sends none, it checks the mount; a mount that is parked, slewing, not
  tracking or on another pier side stops the run, and so do
  `mount.max_failures` cycles with a failed mount call and a pulse still
  in progress `mount.pulse_timeout` after its duration (§ The mount: checks
  and pulses).

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate,
  7.5 px/s at the guider's 2″ a pixel. The guider polls them every 100 ms,
  exposes for 200 ms, and calibrates over 6 px in 4 steps of 200 ms, with 1
  px of backlash to clear and at most 8 steps to a leg.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration calibrates over 6 px in 4 steps

  Scenario: A calibration from idle answers its record, stores it and returns to idle
    Given the guider is started
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "idle"
    And the calibration answered was made on the "east" pier side at declination 0
    And the calibration answered is of a 2 µm sensor of 320 × 240 at binning 1, a pixel scale of 2″ and a focal length of 206.265 mm
    And the calibration answered moves the stars at about 0° for West and 270° for North, within 20% of the rates the guide rates give
    And the calibration answered has no warning
    And the simulated mount received West pulses, then East, then North, then South
    And the guider's stats report the state "idle"
    And the guider's stored calibration for the "east" pier side is the one answered

  Scenario: A calibration from looping returns to looping
    Given the guider is started
    And the guider is looping
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "looping"
    And the guider's stats report the state "looping"
    And the guider's metrics gain at least 3 frames after the calibration

  Scenario: A calibration with Dec off measures the RA axis alone
    Given the guider configuration's "/control/dec/mode" is the JSON "off"
    And the guider is started
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "idle"
    And the calibration answered has no Dec axis
    And the simulated mount received West pulses, then East, and no other

  Scenario: A calibration made more than 60° from the celestial equator is answered and not stored
    Given the simulated mount points at declination 70°
    And the guider is started
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "idle"
    And the calibration answered was made on the "east" pier side at declination 70
    And the guider stores no calibration

  Scenario: A mount that reports no guide rate is calibrated with the fallback step
    Given the simulated mount reports no guide rate
    And the guider configuration's "/calibration/fallback_step" is the JSON "200ms"
    And the guider is started
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "idle"
    And the calibration answered has no guide rates and no expected rates
    And the calibration answered steps each axis by "200ms"

  Scenario: A mount that reports no pier side is calibrated for the unknown side
    Given the simulated mount reports no pier side
    And the guider is started
    When a client asks the guider to calibrate
    Then the request answers 200 with the state "idle"
    And the calibration answered was made on the "unknown" pier side at declination 0
    And the guider's stored calibration for the "unknown" pier side is the one answered

  Scenario Outline: A mount that is not ready refuses the calibration with mount_not_ready
    Given the guider is started
    And <condition>
    When a client asks the guider to calibrate
    Then the request answers 409 with the error "mount_not_ready" and a message naming "<what>"
    And the simulated mount has received no pulse
    And the guider's stats report the state "idle"

    Examples:
      | condition                                 | what           |
      | the simulated mount is parked             | parked         |
      | the simulated mount stops tracking        | not tracking   |
      | the simulated mount is slewing            | slewing        |
      | the simulated mount cannot pulse guide now | pulse guide    |

  Scenario Outline: A mount that does not answer or is not connected refuses the calibration
    Given the guider is started
    And <condition>
    When a client asks the guider to calibrate
    Then the request answers <status> with the error "<error>"
    And the simulated mount has received no pulse

    Examples:
      | condition                            | status | error                |
      | the mount's server stops answering   | 502    | device_unreachable   |
      | the simulated mount is disconnected  | 409    | device_not_connected |

  Scenario: A calibration from idle on a frame without enough stars answers no_stars
    Given the simulated sky holds 0 stars
    And the guider is started
    When a client asks the guider to calibrate
    Then the request answers 422 with the error "no_stars"
    And the simulated mount has received no pulse
    And the guider's stats report the state "idle"

  Scenario: A leg that runs out of steps fails the calibration and stores nothing
    Given the guider is started
    And the simulated mount's pulses move it at 0 of their rate
    When a client asks the guider to calibrate
    Then the request answers 422 with the error "calibration_failed" and the details {"leg": "west"}
    And the guider's stats report the state "idle"
    And the guider stores no calibration

  Scenario: A rate that fails its cross-check fails the calibration
    Given the guider is started
    And the simulated mount's pulses move it at 1.8 of their rate
    When a client asks the guider to calibrate
    Then the request answers 422 with the error "calibration_failed" and the cross-check "ra_rate" failing above 0.5
    And the guider stores no calibration

  Scenario Outline: A mount that stops being ready during the calibration stops the run
    Given the guider is started
    And a client asks the guider to calibrate in the background
    And the simulated mount has received a pulse
    When <event>
    Then the background request answers 422 with the error "guide_failed" and the stop reason "<reason>"
    And the guider's stats report the state "idle"
    And the guider's last stop has the reason "<reason>"

    Examples:
      | event                                          | reason             |
      | the simulated mount starts a slew of an hour   | mount_slewing      |
      | the simulated mount is parked                  | mount_parked       |
      | the simulated mount stops tracking             | mount_not_tracking |
      | the simulated mount turns to the West side     | pier_side_changed  |
      | the mount's server stops answering             | mount_unreachable  |

  Scenario: A pulse still in progress after mount.pulse_timeout stops the run
    Given the guider configuration's "/mount/pulse_timeout" is the JSON "500ms"
    And the guider is started
    And the simulated mount's next pulse sticks
    When a client asks the guider to calibrate
    Then the request answers 422 with the error "guide_failed" and the stop reason "pulse_timeout"
    And the guider's stats report the state "idle"

  Scenario: A calibration whose client gave up holds the requests' turn until it ends
    Given the guider is started
    And a client asks the guider to calibrate and gives up waiting
    And the simulated mount has received a pulse
    When a client asks the guider to clear the calibration store
    Then the request answers 200 with the body {"state": "idle"}
    And the simulated mount received West pulses, then East, then North, then South

  Scenario: A stop during the calibration answers stopped
    Given the guider is started
    And a client asks the guider to calibrate in the background
    And the simulated mount has received a pulse
    When a client asks the guider to stop
    Then the request answers 200 with the body {"state": "idle"}
    And the background request answers 409 with the error "stopped"
    And the guider's last stop has the reason "requested"

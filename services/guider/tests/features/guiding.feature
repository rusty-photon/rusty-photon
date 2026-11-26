Feature: Guiding
  `POST /api/v1/guiding/start` selects guide stars on a fresh frame,
  calibrates when no stored calibration can be used, takes the lock, guides
  and answers once guiding has settled (docs/services/guider.md
  § `POST /api/v1/guiding/start`, § Control, § Dither, settle and abort).
  In `guiding` it keeps the guide stars and the lock and runs the settle
  check again; in `paused` it resumes first; in `star_lost` it selects
  anew. A settle that does not come within its timeout answers
  `settle_timeout`, and guiding goes on. A run whose corrections the error
  does not follow stops with `runaway`. Requests that need `idle` or
  `looping` answer `not_idle` while the guider guides.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate. The
  guider polls them every 100 ms, exposes for 200 ms, calibrates over 6 px
  in 4 steps, and settles within 1.5 px for 1 s, in at most 30 s, unless the
  scenario says otherwise. The simulated sky holds still, so a mount that
  does not drift gives the same offset frame after frame; a scenario whose
  settle must not come has the seeing move the field 0.5″ a frame, which
  keeps the stars off the lock by more than its tolerance. A scenario that has a
  stored calibration starts with one for the simulated rig: West moves the
  stars along +x and North along −y, at the rate the guide rates give.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration calibrates over 6 px in 4 steps
    And the guider configuration settles within 1.5 px for 1 s in at most 30 s

  Scenario: A start from idle with a stored calibration guides and answers once settled
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer
    And the guider's stored calibration for the "east" pier side is still the one the state directory held
    And the guider's stats report the state "guiding"
    And the guider's stats report guiding true and settling false

  Scenario: A start from idle reads looping while it waits for its fresh frame
    Given the state directory holds a calibration of the simulated rig
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to start guiding in the background with the body {}
    And the simulated camera has started an exposure
    Then the guider's stats report the state "looping"
    And the guider's stats report guiding false and settling false

  Scenario: A start with no stored calibration calibrates first, then guides
    Given the guider is started
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer
    And the simulated mount received West pulses, then East, then North, then South, before it guided
    And the guider's stored calibration for the "east" pier side is a record

  Scenario: A start with recalibrate calibrates whatever the store holds
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding with the body {"recalibrate": true}
    Then the request answers 200 with the state "guiding" and a settled answer
    And the simulated mount received West pulses, then East, then North, then South, before it guided

  Scenario: A start from looping guides
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is looping
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer

  Scenario: A start while guiding settles again and answers
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer

  Scenario: A start on a frame without enough stars answers no_stars and leaves the guider idle
    Given the simulated sky holds 0 stars
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding
    Then the request answers 422 with the error "no_stars"
    And the guider's stats report the state "idle"

  Scenario: A settle that does not come within its timeout answers settle_timeout, and guiding goes on
    Given the simulated sky's seeing moves the field 0.5″ a frame
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding with the body {"settle": {"pixels": 0.0001, "time": "1s", "timeout": "2s"}}
    Then the request answers 504 with the error "settle_timeout"
    And the guider's stats report the state "guiding"

  Scenario Outline: A settle request the guider cannot read is refused before anything else
    Given the guider is started
    When a client asks the guider to start guiding with the body <body>
    Then the request answers 400 with the error "invalid_request"
    And the simulated camera has started no exposure

    Examples:
      | body                                                   |
      | {"settle": {"pixels": 1.0, "arcsec": 2.0}}             |
      | {"settle": {"pixels": -1.0}}                           |
      | {"settle": {"time": "soon"}}                           |
      | {"recalibrate": "yes"}                                 |

  Scenario: A settle in arcseconds is taken at the pixel scale
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding with the body {"settle": {"arcsec": 3.0}}
    Then the request answers 200 with the state "guiding" and a settled answer

  Scenario: Requests that need idle or looping answer not_idle while the guider guides
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When a client asks the guider to calibrate
    Then the request answers 409 with the error "not_idle"
    And a client asks the guider to clear the calibration store
    And the request answers 409 with the error "not_idle"
    And a client asks the guider to loop
    And the request answers 409 with the error "not_idle"

  Scenario: Guiding sends corrections that hold the stars against a drift
    Given the simulated mount drifts 30″ a minute east
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the guider's totals count at least 20 guide steps
    Then the simulated mount has received a pulse
    And the guider's stats report an RA RMS below 1.5 px

  Scenario: A mount that moves the stars the wrong way stops the run with runaway
    Given the guider configuration's "/control/runaway/frames" is the JSON 3
    And the guider configuration's "/control/runaway/min_error_px" is the JSON 1.0
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the simulated mount's pulses move it at -1 of their rate
    And the simulated field jumps 6″ east
    Then the guider's last stop has the reason "runaway"
    And the guider's stats report the state "idle"

  Scenario: Stars that leave the frame put the run in star_lost, and their return takes it back
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the simulated field jumps 100″ east
    Then the guider's stats report the state "star_lost"
    And the simulated field jumps 100″ west
    And the guider's stats report the state "guiding"
    And the guider's totals count at least 1 star-lost frame

  Scenario: A start in star_lost selects anew on a fresh frame and settles
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    And the simulated field jumps 100″ east
    And the guider's stats report the state "star_lost"
    And the simulated field jumps 60″ west
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer
    And the guider's totals count at least 1 interruption

  Scenario: A mount that slews during guiding stops the run
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the simulated mount starts a slew of an hour
    Then the guider's last stop has the reason "mount_slewing"
    And the guider's stats report the state "idle"

  Scenario: A stop while a start waits to settle answers stopped
    Given the simulated sky's seeing moves the field 0.5″ a frame
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    And a client asks the guider to start guiding in the background with the body {"settle": {"pixels": 0.0001, "time": "1s", "timeout": "60s"}}
    And the guider's stats report settling true
    When a client asks the guider to stop
    Then the background request answers 409 with the error "stopped"
    And the guider's last stop has the reason "requested"

Feature: Selecting the guide stars anew
  `POST /api/v1/star/reselect` selects the guide stars anew on the next
  frame and answers `{"state": ..., "star_count": N}`
  (docs/services/guider.md § `POST /api/v1/star/reselect`). It is valid
  while frames are taken, and refused with `not_exposing` in `idle`. A
  frame with fewer guide stars than `stars.min` answers `no_stars` and
  keeps the previous guide stars.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing, and a mount, each on a loopback server
  of its own. The guider polls them every 100 ms and exposes for 200 ms. A
  scenario that lists its stars places them well inside the frame and
  apart from each other.

  Scenario: A reselect in idle is refused with not_exposing
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to reselect the guide stars
    Then the request answers 409 with the error "not_exposing"
    And the simulated camera has started no exposure

  Scenario: A reselect while looping answers with the stars of the next frame
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    When a client asks the guider to reselect the guide stars
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}

  Scenario: A reselect on a frame without enough stars answers no_stars, and the loop goes on with its guide stars
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    And the simulated mount has slewed 1° east and stopped
    When a client asks the guider to reselect the guide stars
    Then the request answers 422 with the error "no_stars"
    And the simulated mount slews 1° west and stops
    And the guider's metrics gain a frame with 3 usable stars

  Scenario: A reselect with a field in its body is refused before anything else
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to reselect the guide stars with the body {"stars": 3}
    Then the request answers 400 with the error "invalid_request"

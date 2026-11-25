Feature: Starting the camera loop
  `POST /api/v1/loop` starts the camera loop without guiding
  (docs/services/guider.md § `POST /api/v1/loop`, § The camera loop,
  § Sensor handling). From `idle` it reads both devices' `Connected` afresh,
  checks that the camera's sensor can be measured, writes the frame
  settings, takes the first frame, selects guide stars on it and answers
  `{"state": "looping", "star_count": N}`. A first frame with fewer guide
  stars than `stars.min` still answers `looping`, with `star_count` 0, and
  the guider selects again on each frame until one has enough. In
  `looping` the request answers with the current values and changes
  nothing. An empty body is read as `{}`.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels, binning up to 2, under a sky of 6″ seeing, and a mount, each on
  a loopback server of its own. The guider polls them every 100 ms and
  exposes for 200 ms. A scenario that lists its stars places them well
  inside the frame and apart from each other.

  Scenario: A loop from idle answers with the guide stars of its first frame
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}
    And the guider's stats report the state "looping"
    And the simulated camera starts exposure after exposure

  Scenario: The guide stars are held to stars.max
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/stars/max" is the JSON 2
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 2}

  Scenario: A first frame without enough stars answers looping with no guide star, and the guider selects again on each frame
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/stars/min" is the JSON 4
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 0}
    And the guider's metrics hold at least 3 frames, each star-lost with no usable star

  Scenario: A loop while looping answers the current values and changes nothing
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    And the guider's metrics hold at least 3 frames
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}
    And the guider's metrics still hold the frames they held before the request

  Scenario: A loop with no body and a loop with an empty object are the same request
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to loop with the body {}
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}

  Scenario: A loop with a field the request does not have is refused before anything else
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to loop with the body {"exposure": "1s"}
    Then the request answers 400 with the error "invalid_request"
    And the simulated camera has started no exposure

  Scenario Outline: A loop with a configuration gap is refused with not_configured
    Given the simulated camera and mount, both connected
    And <configuration>
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 409 with the error "not_configured"
    And the simulated camera has started no exposure
    And the guider's stats report the state "idle"

    Examples:
      | configuration                                                                     |
      | a guider configuration naming both devices and no focal length                    |
      | a guider configuration naming the camera only and a focal length of 206.265 mm    |
      | a guider configuration naming no devices and no focal length                      |

  Scenario Outline: A loop with a device that does not answer is refused with device_unreachable
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the <device>'s server stops answering
    When a client asks the guider to loop
    Then the request answers 502 with the error "device_unreachable"
    And the simulated camera has started no exposure

    Examples:
      | device |
      | camera |
      | mount  |

  Scenario Outline: A loop with a device that is not connected is refused with device_not_connected
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the simulated <device> is disconnected
    When a client asks the guider to loop
    Then the request answers 409 with the error "device_not_connected"
    And the simulated camera has started no exposure

    Examples:
      | device |
      | camera |
      | mount  |

  Scenario: A loop reads Connected afresh rather than taking the poll's last answer
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/devices/poll_interval" is the JSON "1h"
    And the guider is started
    And the guider reports the camera connected
    And the simulated camera is disconnected
    When a client asks the guider to loop
    Then the request answers 409 with the error "device_not_connected"

  Scenario Outline: A binning the camera cannot do is refused with camera_unsupported
    Given the simulated camera and mount, both connected
    And the simulated camera has a <sensor> sensor
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/binning" is the JSON <binning>
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 422 with the error "camera_unsupported"
    And the simulated camera has started no exposure

    Examples:
      | sensor     | binning |
      | RGGB       | 2       |
      | monochrome | 3       |

  Scenario Outline: The frame settings are the full frame at the binning the sensor is read at
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated camera has a <sensor> sensor
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/binning" is the JSON <binning>
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}
    And every exposure of the simulated camera is at bin <bin> of the region <width> × <height> at 0, 0

    Examples:
      | sensor     | binning | bin | width | height |
      | monochrome | 1       | 1   | 320   | 240    |
      | monochrome | 2       | 2   | 160   | 120    |
      | RGGB       | 1       | 1   | 320   | 240    |

  Scenario: A configured gain and offset are written to the camera
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/gain" is the JSON 120
    And the guider configuration's "/camera/offset" is the JSON 30
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 200 with the state "looping"
    And every exposure of the simulated camera is at gain 120 and offset 30

  Scenario: A null gain and offset leave the camera's values as another client left them
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And another client sets the simulated camera's gain to 75 and its offset to 12
    When a client asks the guider to loop
    Then the request answers 200 with the state "looping"
    And every exposure of the simulated camera is at gain 75 and offset 12

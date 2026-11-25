Feature: The latest frame as a picture
  `GET /api/v1/frame/latest` answers the latest measured frame as a PNG,
  stretched to eight bits, with the search region of each guide star drawn
  in (docs/services/guider.md § `GET /api/v1/frame/latest`). The frame is
  the measured one, after binning or summing. It answers `no_frame` when no
  frame has been measured since the loop last started, and the guider
  keeps one frame in memory.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels, binning up to 2, under a sky of 6″ seeing, and a mount, each on
  a loopback server of its own. The guider polls them every 100 ms and
  exposes for 200 ms. A scenario that lists its stars places them well
  inside the frame and apart from each other.

  Scenario: A guider that has measured no frame answers no_frame
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks for the latest frame
    Then the request answers 404 with the error "no_frame"

  Scenario Outline: The latest frame is a PNG of the measured frame
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated camera has a <sensor> sensor
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/binning" is the JSON <binning>
    And the guider is started
    And the guider is looping
    When a client asks for the latest frame
    Then the request answers 200 with a PNG of <width> × <height> pixels

    Examples:
      | sensor     | binning | width | height |
      | monochrome | 1       | 320   | 240    |
      | monochrome | 2       | 160   | 120    |
      | RGGB       | 1       | 160   | 120    |

  Scenario: The latest frame outlasts a stop
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    And a client asks the guider to stop
    When a client asks for the latest frame
    Then the request answers 200 with a PNG of 320 × 240 pixels

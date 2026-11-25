Feature: Frames that fail, and the camera's stop reasons
  A frame fails when a camera call fails, when it has not arrived
  `camera.frame_timeout` after its exposure should have ended, or when its
  dimensions are not the ones the guider asked for
  (docs/services/guider.md § The camera loop). A failed frame is
  discarded, and the next attempt begins no sooner than an exposure after
  the failed one did. After every failure the guider reads the camera's
  static properties again and writes the frame settings again. After
  `camera.max_frame_failures` failures in a row the loop stops with the
  reason `camera_error`, and so does a camera that comes back with another
  sensor size, pixel size or sensor type than it had when the guider left
  `idle` (§ Stopping on its own). A request that waits while the loop stops
  by itself answers `guide_failed`, with the reason in
  `details.stop_reason`.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing, and a mount, each on a loopback server
  of its own. The guider polls them every 100 ms and exposes for 200 ms.

  Scenario Outline: Fewer failures in a row than camera.max_frame_failures leave the loop going
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/max_frame_failures" is the JSON 5
    And the guider is started
    And the guider is looping
    When the next 2 exposures of the simulated camera fail as <fault>
    Then the guider's metrics gain at least 3 frames after the failures
    And the guider's stats report the state "looping"

    Examples:
      | fault                |
      | a start that fails   |
      | a frame never ready  |
      | a frame of the wrong size |

  Scenario Outline: camera.max_frame_failures failures in a row stop the loop with camera_error
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/max_frame_failures" is the JSON 3
    And the guider configuration's "/camera/frame_timeout" is the JSON "2s"
    And the guider is started
    And the guider is looping
    When every exposure of the simulated camera fails as <fault>
    Then the guider's stats report the state "idle"
    And the guider's last stop has the reason "camera_error"

    Examples:
      | fault                |
      | a start that fails   |
      | a frame never ready  |
      | a frame of the wrong size |

  Scenario: Failures that are not in a row do not add up
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/max_frame_failures" is the JSON 3
    And the guider is started
    And the guider is looping
    When the next 2 exposures of the simulated camera fail as a start that fails
    And the guider's metrics gain at least 3 frames after the failures
    And the next 2 exposures of the simulated camera fail as a start that fails
    Then the guider's metrics gain at least 3 frames after the failures
    And the guider's stats report the state "looping"

  Scenario: Each failed attempt takes an exposure before the next begins
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "1s"
    And the guider configuration's "/camera/max_frame_failures" is the JSON 5
    And the guider is started
    And the guider is looping
    When the next 2 exposures of the simulated camera fail as a start that fails
    Then the simulated camera's next exposure starts no sooner than 3 s after the last one before the failures
    And the guider's stats report the state "looping"

  Scenario: A loop whose first frames all fail answers guide_failed with the stop reason
    Given the simulated camera and mount, both connected
    And every exposure of the simulated camera fails as a start that fails
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/max_frame_failures" is the JSON 3
    And the guider is started
    When a client asks the guider to loop
    Then the request answers 422 with the error "guide_failed" and the stop reason "camera_error"
    And the guider's stats report the state "idle"
    And the simulated camera has started no exposure

  Scenario: The frame settings are written again after a frame fails
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/gain" is the JSON 120
    And the guider is started
    And the guider is looping
    When another client sets the simulated camera's gain to 75 and its offset to 12
    And the next 1 exposures of the simulated camera fail as a start that fails
    Then the simulated camera's gain reads 120 again
    And the guider's stats report the state "looping"

  Scenario: A camera that comes back with another pixel size stops the loop with camera_error
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When the simulated camera is disconnected
    And the guider reports the camera reachable and not connected
    And the simulated camera's pixel size becomes 3.75 µm
    And the simulated camera is connected
    Then the background request answers 422 with the error "guide_failed" and the stop reason "camera_error"
    And the guider's last stop has the reason "camera_error" and a message naming "pixel size 2 µm → 3.75 µm"
    And the guider's stats report the state "idle"

  Scenario: A frame failure that finds another pixel size stops the loop with camera_error
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/max_frame_failures" is the JSON 3
    And the guider is started
    And the guider is looping
    When the simulated camera's pixel size becomes 3.75 µm
    And the next 1 exposures of the simulated camera fail as a start that fails
    Then the guider's stats report the state "idle"
    And the guider's last stop has the reason "camera_error" and a message naming "pixel size 2 µm → 3.75 µm"

  Scenario: A camera that comes back as it was leaves the loop going
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When the simulated camera is disconnected
    And the guider reports the camera reachable and not connected
    And the simulated camera is connected
    And the guider reports the camera connected
    Then the guider's stats report the state "looping" at every read for 2 seconds
    And the guider's last stop is null

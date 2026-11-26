@serial
Feature: Checking a camera for a wedge
  check_camera takes one dark exposure (1 s unless the call names a
  duration) through the camera's capture slot and the guide camera guard,
  keeps nothing (no FITS file, no exposure document, no exposure events),
  and says whether the camera is wedged. A camera whose first exposure
  after a power cycle under a live SDK wedges reads out for a minute or
  more and returns an all-zero frame.

  The verdict is ok for a frame ready within duration plus the camera's
  readout_time_estimate plus 30 s with at least one pixel lit; wedged for
  a later frame, one not ready by duration plus 120 s (its exposure is
  then aborted), or an all-zero one; unavailable for anything else, with
  the reason. elapsed_s runs from the camera accepting the exposure to the
  frame ready, and is null when the exposure never started.

  Scenario: A camera that reads out a lit frame in time is ok, and nothing is kept
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_started" and "exposure_complete"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "check_camera" with the JSON {"camera_id": "main-cam", "duration": "100ms"}
    Then the tool result at "/verdict" should be the JSON "ok"
    And the tool result at "/camera_id" should be the JSON "main-cam"
    And the tool result should contain "elapsed_s" as a positive number
    And the test webhook receiver should not have received any events

  Scenario: A camera rp does not know is refused
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "check_camera" with the JSON {"camera_id": "no-such-cam"}
    Then the tool call should return an error
    And the error message should contain "no-such-cam"

  Scenario: A camera held out of rp is unavailable, not wedged
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "disconnect_device" with the JSON {"device_id": "main-cam"}
    And the MCP client calls "check_camera" with the JSON {"camera_id": "main-cam"}
    Then the tool result at "/verdict" should be the JSON "unavailable"
    And the tool result at "/elapsed_s" should be the JSON null

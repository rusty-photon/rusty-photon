@serial
Feature: The guider's camera and mount are rp's
  The guider's devices are configured apart from rp's roster. So before
  rp asks the guider to start guiding, it reads the guider's GET
  /api/v1/devices and compares the unique_id reported there for each
  device with the one its own session bound. The guider's mount has to
  be rp's mount. The guider's camera has to be the guiding train's
  camera, and on a rig with no guiding train it has to be none of the
  cameras rp holds a session with, since rp guards no camera there. A
  difference refuses the call, naming both ids. An id that is missing
  or blank on either side refuses it too: an identity that cannot be
  checked never passes. A refused call has sent the guider no start. A
  stop, a pause and a resume run no check, and neither do a capture
  through the guide camera and a guiding train's lease: a run rp
  started is on the guide camera by the check, and the guide camera
  guard's read refuses whatever run the guider has.

  The stub guider's device report names the simulator's mount and a
  camera of its own unless a scenario gives it another.

  Scenario: A start goes through when the guider's mount is rp's and its camera is none of rp's
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"
    And the stub guider should have received a "/devices" request

  Scenario: A start goes through when the guider's camera is the guiding train's camera
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"

  Scenario: A start is refused when the guider corrects another mount, naming both ids
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider reports the mount unique_id "another-mount"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "another-mount"
    And the error message should name the simulator mount's unique id
    And the stub guider should not have received a "/guiding/start" request

  Scenario: A start is refused when the guider's camera is not the guiding train's, naming both ids
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider reports the camera unique_id "another-camera"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "another-camera"
    And the error message should name the simulator camera's unique id
    And the stub guider should not have received a "/guiding/start" request

  Scenario: A start is refused on a rig with no guiding train when the guider's camera is one rp captures through
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should name the simulator camera's unique id
    And the stub guider should not have received a "/guiding/start" request

  Scenario Outline: A start is refused when the guider reports no identity for its <device>
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider reports <report>
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "<device>"
    And the stub guider should not have received a "/guiding/start" request

    Examples:
      | device | report                   |
      | camera | no camera unique_id      |
      | camera | a blank camera unique_id |
      | mount  | no mount unique_id       |
      | mount  | a blank mount unique_id  |

  # rp's mount is the one the guider reports, and the guider reports a
  # camera id, so the one id missing is rp's for the train's camera.
  Scenario: A start is refused when rp holds no session with the guiding train's camera
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a mount on the simulator and an offline camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "camera"
    And the stub guider should not have received a "/guiding/start" request

  Scenario: A guiding train's lease and a capture through the guide camera run no check
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider reports the camera unique_id "another-camera"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-a"
    Then the train lease call should succeed
    When the MCP client calls "capture" with camera "main-cam" for 200 ms under lease "lease-a"
    Then the train lease call should succeed
    And the stub guider should not have received a "/devices" request

  Scenario Outline: <tool> goes through whatever devices the guider reports
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider reports the mount unique_id "another-mount"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls the guider tool "<tool>" with empty arguments
    Then the guider result should contain "state" with value "<answer>"

    Examples:
      | state   | tool           | answer  |
      | guiding | stop_guiding   | idle    |
      | guiding | pause_guiding  | paused  |
      | paused  | resume_guiding | guiding |

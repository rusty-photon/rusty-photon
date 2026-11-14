@wip @serial
Feature: The guider's camera and mount are rp's
  The guider's devices are configured apart from rp's roster. So before
  rp asks the guider to start guiding, or for its camera lease, it
  reads the guider's GET /api/v1/devices and compares the unique_id
  reported there for each device with the one its own session bound.
  The guider's mount has to be rp's mount. The guider's camera has to
  be the guiding train's camera, and on a rig with no guiding train it
  has to be none of the cameras rp holds a session with, since rp would
  expose on it with no camera lease. A difference refuses the call,
  naming both ids. An id that is missing or blank on either side
  refuses it too: an identity that cannot be checked never passes. A
  refused call has sent the guider no start and has asked it for no
  camera lease. A stop, a pause and a resume run no check.

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

  Scenario: A start is refused when rp holds no session with the guiding train's camera
    Given a stub guider in state "idle"
    And rp is running with an offline camera in a guiding train
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the stub guider should not have received a "/guiding/start" request

  Scenario: A camera lease is refused when the guider's camera is not the train's, naming both ids
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider reports the camera unique_id "another-camera"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-a"
    Then the tool call should return an error
    And the error message should contain "another-camera"
    And the error message should name the simulator camera's unique id
    And the stub guider should not have received a "/camera/lease" request

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

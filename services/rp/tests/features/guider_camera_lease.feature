@wip @serial
Feature: rp exposes on the guide camera only under the guider's camera lease
  The guider exposes on the guiding train's camera whenever it is not
  idle, so rp exposes on that camera only while it holds the guider's
  camera lease. A guiding train's lease takes it under a token rp
  names, 32 lowercase hex digits and new for each lease, with a ttl of
  60 s. rp renews it every 20 s and releases it by its token when the
  train lease has given everything back, however the train lease
  ended, and on every failure of the acquire after it asked, one whose
  answer never came included. camera_lease in the acquire's result
  says whether the train lease holds one, and an imaging train's lease
  takes none. A guider that refuses has its refusal relayed, naming
  the guider's state when it is not idle. A renewal the guider answers
  with lease_not_found ends the train lease: the camera lease lives in
  the guider's memory, and a restart of the guider drops it. A capture
  through the guiding train's camera holds the camera lease for as
  long as it holds the camera's slot: the train lease's when it runs
  under one, and otherwise one of its own. A guider that is not idle
  refuses that one and the capture fails at once, naming the guider's
  state, and a guider that does not answer fails the capture too. A
  camera lease the guider drops mid-exposure does not end the capture.
  A capture through any other camera takes no camera lease, whatever
  the guider's state.

  The stub guider grants its camera lease by the contract's rules: in
  the state idle only, to one token at a time, to a token of 32
  lowercase hex digits, and it refuses with not_idle and a message
  naming its state otherwise. While a token holds the lease it refuses
  a start with camera_leased. A stub guider that restarts is in the
  state idle and holds no lease. Its camera is the simulator's camera
  in every scenario with a guiding train, so the check of
  guider_devices_check.feature passes.

  Scenario: A guiding train's lease takes the guider's camera lease and gives it back by its token
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the train lease call should succeed
    And the train lease result field "camera_lease" should be true
    And the stub guider should hold a camera lease under a token of 32 lowercase hex digits with a ttl of "60s"
    When the MCP client calls "release_train_lease" with lease id "lease-g"
    Then the stub guider should hold no camera lease
    And the stub guider's last camera lease release should name the token it granted

  Scenario: An imaging train's lease takes no camera lease
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the train lease call should succeed
    And the train lease result field "camera_lease" should be false
    And the stub guider should not have received a "/camera/lease" request

  Scenario Outline: A guider in the state <state> refuses the camera lease, and the acquire fails naming the state
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "the guider refused the camera lease"
    And the error message should contain "<state>"
    And the stub guider should hold no camera lease

    Examples:
      | state       |
      | looping     |
      | calibrating |
      | guiding     |
      | paused      |
      | star_lost   |

  Scenario: A camera lease another client holds is refused, and the guider's refusal reaches the caller
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider answers "/camera/lease" requests with error code "camera_leased" and message "another client holds the camera lease"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "the guider refused the camera lease"
    And the error message should contain "another client holds the camera lease"

  Scenario: A camera lease whose answer never comes fails the acquire and is released all the same
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider grants a camera lease and never answers the request
    And rp's guiding config sets "timeout" to the JSON "2s"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the stub guider should have received a camera lease release naming the token it granted within 5 seconds
    And the stub guider should hold no camera lease

  Scenario: A held camera lease is renewed before its ttl runs out
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the train lease call should succeed
    And the stub guider should receive a renewal of the camera lease it granted within 30 seconds

  Scenario: A guider that restarts drops the camera lease, and the train lease ends saying so
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    And the MCP client holds train lease "lease-g" on train "guide"
    When the stub guider restarts
    Then the stub guider should have received a camera lease release naming the token it granted within 30 seconds
    When the MCP client calls "move_focuser" with focuser "main-focuser" to position 25100 under lease "lease-g"
    Then the tool call should return an error
    And the error message should contain "lease 'lease-g' is not held"
    And the error message should contain "lost the guider's camera lease"

  Scenario: A train lease that reaches its ends_by gives the camera lease back
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g" for "2s"
    Then the train lease call should succeed
    And the stub guider should have received a camera lease release naming the token it granted within 10 seconds
    And the stub guider should hold no camera lease

  Scenario: A capture under a guiding train's lease runs under the train lease's camera lease
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    And the MCP client holds train lease "lease-g" on train "guide"
    When the MCP client calls "capture" with camera "main-cam" for 200 ms under lease "lease-g"
    Then the tool call should succeed
    And the stub guider should have been asked for 1 camera lease

  Scenario: A capture through the guide camera with no train lease takes a camera lease of its own and gives it back
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should succeed
    And the stub guider should have been asked for 1 camera lease
    And the stub guider should hold no camera lease
    And the stub guider's last camera lease release should name the token it granted

  Scenario: A start right after a guide-camera capture goes through
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    And the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"

  Scenario Outline: A capture through the guide camera fails at once while the guider is <state>, naming the state
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "exposure_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should return an error
    And the error message should contain "<state>"
    And no "exposure_started" event arrives within 500 ms

    Examples:
      | state   |
      | guiding |
      | paused  |

  Scenario: A capture through the guide camera fails when the guider does not answer
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "exposure_started"
    And rp's guiding config sets "timeout" to the JSON "2s"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the stub guider stops answering
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should return an error
    And no "exposure_started" event arrives within 500 ms

  Scenario: A camera lease the guider drops mid-exposure does not end the capture
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "exposure_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    When a second MCP client starts a "3s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the stub guider restarts
    Then the background "capture" call should succeed

  Scenario: A capture through an imaging train's camera takes no camera lease, whatever the guider's state
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera-only imaging train "main" on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should succeed
    And the stub guider should not have received a "/camera/lease" request

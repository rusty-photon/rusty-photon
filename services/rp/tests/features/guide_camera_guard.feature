@serial
Feature: rp keeps its captures through the guide camera and its starts of guiding apart
  The guider and rp are both Alpaca clients of the guiding train's
  camera, the guide camera, and a camera has one exposure, one
  settings cache and one image slot for all its clients. rp keeps its
  own captures through that camera and the starts of guiding it sends
  off each other with the guide camera guard, which lives in its
  memory; the guider serves nothing for it. A capture through the
  guide camera holds the guard until its image is downloaded, a
  guiding train's train lease from step 2 of its acquire until it has
  given everything back, and start_guiding, or the start of the
  rotate-while-guiding ladder, for the length of its request to the
  guider. Captures and a guiding train's lease hold the guard
  together, and a start holds it alone. Whichever finds it held by the
  other kind is refused at once, naming the holder, and a start's tool
  leads its error. Having taken the guard, a capture or a guiding
  train's lease reads the guider's stats and goes ahead only with the
  guider idle, naming the state otherwise, and a read that fails
  refuses it, naming the error. Every capture through the guide camera
  reads, one under a train lease included. A refusal sends the guider
  nothing but that read. Through a camera of no guiding train there is
  no guard, and nothing reads the guider for one.

  The stub guider's camera is the simulator's camera in every scenario
  with a guiding train, so the check of guider_devices_check.feature
  passes before a start. A long exposure holds a capture in flight,
  and a stub guider that settles after a delay holds a start in
  flight.

  Scenario Outline: A guiding train's lease is refused while the guider is <state>, naming the state
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "acquire_train_lease: the guider is <state>; rp uses the guide camera 'main-cam' only while the guider is idle"
    And the stub guider should have received no request other than a read

    Examples:
      | state       |
      | looping     |
      | calibrating |
      | guiding     |
      | paused      |
      | star_lost   |

  Scenario Outline: A capture through the guide camera is refused while the guider is <state>, naming the state
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "exposure_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should return an error
    And the error message should contain "the guider is <state>; rp uses the guide camera 'main-cam' only while the guider is idle"
    And no "exposure_started" event arrives within 500 ms
    And the stub guider should have received no request other than a read

    Examples:
      | state       |
      | looping     |
      | calibrating |
      | guiding     |
      | paused      |
      | star_lost   |

  Scenario: A capture under a guiding train's lease reads the guider too
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "exposure_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    And the MCP client holds train lease "lease-g" on train "guide"
    When another client starts the stub guider guiding
    And the MCP client calls "capture" with camera "main-cam" for 200 ms under lease "lease-g"
    Then the tool call should return an error
    And the error message should contain "the guider is guiding; rp uses the guide camera 'main-cam' only while the guider is idle"
    And no "exposure_started" event arrives within 500 ms

  Scenario: A capture through the guide camera is refused when the guider's state cannot be read, naming the error
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider answers "/guiding/stats" requests with error code "internal" and message "the stats are unavailable"
    And a test webhook receiver subscribed to "exposure_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should return an error
    And the error message should contain "could not read the guider's state before using the guide camera 'main-cam'"
    And the error message should contain "the stats are unavailable"
    And no "exposure_started" event arrives within 500 ms

  Scenario: A guiding train's lease is refused when the guider does not answer the read within the guiding config's timeout
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp's guiding config sets "timeout" to the JSON "2s"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the stub guider stops answering
    And the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "acquire_train_lease: could not read the guider's state before using the guide camera 'main-cam'"

  Scenario: A refused lease gives the guide camera back, to a capture and then to a start
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    When another client stops the stub guider's run and leaves it in state "idle"
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should succeed
    When the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"

  Scenario: start_guiding is refused while a guiding train's lease holds the guide camera, naming the lease, and goes through after the release
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "guide_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    And the MCP client holds train lease "lease-g" on train "guide"
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "start_guiding: the guide camera 'main-cam' is in use by train lease 'lease-g'"
    And the stub guider should not have received a "/guiding/start" request
    And the test webhook receiver should not have received a "guide_started" event
    When the MCP client calls "release_train_lease" with lease id "lease-g"
    And the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"

  Scenario: start_guiding is refused at once while a capture through the guide camera is in flight, naming the capture
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to the events "exposure_started, guide_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When a second MCP client starts a "3s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "start_guiding: the guide camera 'main-cam' is in use by a capture"
    And the stub guider should not have received a "/guiding/start" request
    And the test webhook receiver should not have received a "guide_started" event
    And the background "capture" call should succeed

  Scenario: A start right after a guide-camera capture goes through
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 200 ms
    And the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"

  Scenario: A capture through the guide camera is refused at once while a start is in flight, naming start_guiding
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider settles after 3000 ms
    And a test webhook receiver subscribed to the events "exposure_started, guide_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When a second MCP client starts guiding in the background
    And the test webhook receiver has received a "guide_started" event
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should return an error
    And the error message should contain "the guide camera 'main-cam' is in use by start_guiding"
    And no "exposure_started" event arrives within 500 ms
    And the background "start_guiding" call should succeed

  Scenario: A guiding train's lease is refused at once while a start is in flight, naming start_guiding
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And the stub guider settles after 3000 ms
    And a test webhook receiver subscribed to "guide_started"
    And rp is running with guiding train "guide" of a focuser and a camera on the simulator
    And an MCP client connected to rp
    When a second MCP client starts guiding in the background
    And the test webhook receiver has received a "guide_started" event
    And the MCP client calls "acquire_train_lease" for train "guide" with lease id "lease-g"
    Then the tool call should return an error
    And the error message should contain "acquire_train_lease: the guide camera 'main-cam' is in use by start_guiding"
    And the background "start_guiding" call should succeed

  # Another client of the guider's starts it while rp's capture exposes,
  # so the ladder finds a run to pause and starts it again after the
  # turn, with the capture still holding the guide camera.
  Scenario: The ladder's start is refused at once while a capture holds the guide camera, naming the capture
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to the events "exposure_started, guide_started"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When a second MCP client starts a "10s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And another client starts the stub guider guiding
    And the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the tool call should return an error
    And the error message should contain "move_rotator: the guide camera 'main-cam' is in use by a capture"
    And the stub guider should have received the requests "/guiding/pause, /guiding/stop, /calibration/clear" in that order
    And the stub guider should not have received a "/guiding/start" request
    And the test webhook receiver should not have received a "guide_started" event
    And the background "capture" call should succeed

  Scenario: A capture through a camera of no guiding train reads nothing from the guider for a guard, even with a start in flight
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider settles after 3000 ms
    And a test webhook receiver subscribed to "guide_started"
    And rp is running with a camera-only imaging train "main" on the simulator
    And an MCP client connected to rp
    When a second MCP client starts guiding in the background
    And the test webhook receiver has received a "guide_started" event
    And the MCP client calls "capture" with camera "main-cam" for 200 ms
    Then the tool call should succeed
    And the background "start_guiding" call should succeed

  Scenario: An imaging train's lease takes no guard, whatever the guider's state
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with imaging train "main" of a focuser, a filter wheel and a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "acquire_train_lease" for train "main" with lease id "lease-a"
    Then the train lease call should succeed

Feature: The camera facade and guiding never share the guide camera

  PHD2 runs one exposure at a time, but a guiding request sent during a
  facade frame still spoils it: a guide start turns the frame's end into
  a calibration that fails at once, and a full pause throws the frame
  away and leaves PHD2 unable to capture until it restarts. So while a
  facade frame is in flight, this service refuses every guiding
  operation except stop with camera_busy (HTTP 409), and never queues it
  behind the exposure. Stop is never refused. It ends PHD2's frame, and
  the facade's exposure then fails as aborted.

  The facade captures only from a Stopped PHD2: an allowlist, so a PHD2
  that is guiding, looping or paused gets an invalid-operation error
  (0x40B) naming its state.

  After a crash the facade leaves a marker, phd2-guider-capture.json in
  image_dir, naming the frame in flight and its deadline. A service that
  starts on a marker whose deadline is still ahead keeps guiding refused
  until that deadline, then removes the marker.

  Scenario Outline: A guiding operation during a facade frame is refused with camera_busy
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 5 s light exposure
    And the client sends <operation>
    Then the response status should be 409
    And the response error should be "camera_busy"
    And the mock PHD2 should not have received a "<rpc>" request

    Examples:
      | operation           | rpc               |
      | a guiding start     | guide             |
      | a pause             | set_paused        |
      | a resume            | set_paused        |
      | a dither            | dither            |
      | a calibration clear | clear_calibration |
      | a star reselect     | find_star         |

  Scenario: Stopping guiding during a facade frame ends the frame and fails the exposure as aborted
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 5 s light exposure
    And the client stops guiding
    Then the response status should be 200
    And the response field "state" should be "stopped"
    And the mock PHD2 should have received a "stop_capture" request
    And the facade camera should report a failed exposure within 5 s
    And reading the facade image should fail with ASCOM error 0x500 mentioning "aborted"

  Scenario: Guiding is accepted again once the facade frame has ended
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 0.2 s light exposure
    And the facade image becomes ready within 10 s
    And the client starts guiding
    Then the response status should be 200
    And the response field "state" should be "guiding"

  Scenario: A capture is refused while PHD2 is guiding
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts guiding
    And the client tries to start a 1 s light exposure
    Then the facade call should fail with ASCOM error 0x40B mentioning "Guiding"
    And the mock PHD2 should not have received a "capture_single_frame" request

  Scenario: A crash marker keeps guiding refused until its frame's deadline
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And image_dir holds a capture marker whose deadline is 3 s away
    And the guider service is running with the camera facade enabled
    When the client starts guiding
    Then the response status should be 409
    And the response error should be "camera_busy"
    When the capture marker's deadline has passed
    And the client starts guiding
    Then the response status should be 200
    And image_dir should hold no capture marker

Feature: Camera facade — PHD2's guide frames as an ASCOM Alpaca Camera

  With camera.enabled, phd2-guider serve also serves the guide camera as
  an ASCOM Alpaca Camera: device 0 on its own listener, bound to
  loopback. PHD2 captures every frame. For each exposure the facade asks
  PHD2's capture_single_frame for one frame saved to a file it names in
  image_dir, phd2-guider- followed by 32 hex digits and .fits. It waits
  for PHD2's SingleFrameComplete event naming that file, reads the file,
  deletes it, and serves the frame as the image.

  Connecting only reads PHD2: its version, its camera's frame size and
  its camera's name. The facade needs PHD2 2.6.14 or later, the first
  release whose capture_single_frame saves to a named file and reports
  the frame with an event.

  The mock PHD2 here reports an 8 x 6 camera named "Mock Camera". Its
  frames are an unsigned 16-bit ramp in row-major order: the pixel at
  x, y holds (8 y + x) x 1365, so x 1, y 0 holds 1365, x 0, y 1 holds
  10920 and x 2, y 1 holds 13650.

  ASCOM error numbers: 0x401 invalid value, 0x407 not connected, 0x40B
  invalid operation, 0x500 unspecified.

  Scenario: Connecting reads PHD2's camera without changing anything in PHD2
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When the client connects the facade camera
    Then the facade camera should be connected
    And the facade camera should report a sensor of 8 x 6 pixels named "Mock Camera"
    And the mock PHD2 should have received no request that changes its state

  Scenario: A PHD2 older than 2.6.14 cannot be connected
    Given a mock PHD2 2.6.13 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When the client tries to connect the facade camera
    Then the facade call should fail with ASCOM error 0x407 mentioning "2.6.14"
    And the facade camera should not be connected

  Scenario: A PHD2 whose camera is disconnected cannot be connected
    Given a mock PHD2 2.6.14 whose camera is disconnected
    And the guider service is running with the camera facade enabled
    When the client tries to connect the facade camera
    Then the facade call should fail with ASCOM error 0x407 mentioning "camera not connected"

  Scenario: A light frame is captured by PHD2 into image_dir and served as the image
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 0.5 s light exposure
    Then the facade image should become ready within 10 s
    And the facade image should be 8 x 6 pixels
    And the facade image pixel at x 1, y 0 should be 1365
    And the facade image pixel at x 0, y 1 should be 10920
    And the facade camera's last exposure should have lasted 0.5 s
    And the mock PHD2 should have received a capture of 500 ms saved under image_dir as a phd2-guider FITS file
    And image_dir should hold no phd2-guider file

  Scenario: A subframe is cut from PHD2's full frame
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client sets the subframe to start at x 2, y 1 and span 4 x 3 pixels
    And the client starts a 0.1 s light exposure
    Then the facade image should become ready within 10 s
    And the facade image should be 4 x 3 pixels
    And the facade image pixel at x 0, y 0 should be 13650

  Scenario: An exposure is rounded to PHD2's whole milliseconds
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 0.1236 s light exposure
    Then the facade image should become ready within 10 s
    And the mock PHD2 should have received a capture of 124 ms saved under image_dir as a phd2-guider FITS file

  Scenario: A dark frame is refused, since PHD2 cannot take one
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client tries to start a 1 s dark exposure
    Then the facade call should fail with ASCOM error 0x401
    And the mock PHD2 should not have received a "capture_single_frame" request

  Scenario Outline: An exposure outside PHD2's 1 ms to 600 s is refused
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client tries to start a <duration> s light exposure
    Then the facade call should fail with ASCOM error 0x401
    And the mock PHD2 should not have received a "capture_single_frame" request

    Examples:
      | duration |
      | 0.0004   |
      | 600.001  |

  Scenario: A subframe outside the sensor is refused
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client sets the subframe to start at x 6, y 0 and span 4 x 3 pixels
    And the client tries to start a 1 s light exposure
    Then the facade call should fail with ASCOM error 0x401
    And the mock PHD2 should not have received a "capture_single_frame" request

  Scenario: A frame PHD2 fails surfaces PHD2's error text
    Given a mock PHD2 2.6.14 with an 8 x 6 camera whose frames fail
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 0.1 s light exposure
    Then the facade camera should report a failed exposure within 10 s
    And reading the facade image should fail with ASCOM error 0x500 mentioning "Mock camera error"
    And image_dir should hold no phd2-guider file

  Scenario: A frame PHD2 never reports fails at its deadline
    Given a mock PHD2 2.6.14 with an 8 x 6 camera whose frames are never reported
    And the guider service is running with the camera facade enabled and a capture grace of "1s"
    And the facade camera is connected
    When the client starts a 0.1 s light exposure
    Then the facade camera should report a failed exposure within 10 s
    And reading the facade image should fail with ASCOM error 0x500 mentioning "did not report"

  Scenario: Aborting ends PHD2's frame and leaves no image
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 5 s light exposure
    And the client aborts the facade exposure
    Then the mock PHD2 should have received a "stop_capture" request
    And the facade camera should be idle within 5 s
    And the facade image should not be ready
    And image_dir should hold no phd2-guider file

  Scenario: An orphaned facade file is swept before a capture, and other files are left alone
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And image_dir holds a file named "phd2-guider-0123456789abcdef0123456789abcdef.fits"
    And image_dir holds a file named "save_image_abc123"
    And the guider service is running with the camera facade enabled
    And the facade camera is connected
    When the client starts a 0.1 s light exposure
    Then the facade image should become ready within 10 s
    And image_dir should hold no phd2-guider file
    And image_dir should still hold "save_image_abc123"

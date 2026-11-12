Feature: Restarting PHD2

  POST /api/v1/phd2/restart stops PHD2, lets the service manager start it
  again, and connects its equipment, so that PHD2's camera SDK meets the
  guide camera in a fresh process. rp's restart_guider sends it. Its body
  is a JSON object, {}: a body of any other media type is refused with 415.

  Where nothing would start PHD2 again the restart is refused with
  restart_unsupported (501): phd2.managed is false, phd2.host is not this
  host, or the rusty-photon-phd2 unit is neither active nor activating. It
  is refused with camera_connected (409) while a client has the camera
  facade's camera connected, and with guiding_active (409) while PHD2
  answers that it guides, calibrates, is paused or has lost its star: a
  restart ends no guiding as a side effect.

  A PHD2 that answers is stopped through its RPC: stop_capture,
  set_connected false, then shutdown. One that has not exited 30 s after
  shutdown, or that accepts the connection and does not answer within
  phd2.command_timeout, is ended with SIGTERM and, 5 s later, SIGKILL.
  Once the unit has started PHD2 again and the new PHD2 has sent its
  Version event, the service sends set_connected true and waits until
  get_connected reads true. The answer is
  {"phd2": "restarted", "equipment": "connected", "elapsed_ms": N}.

  The scenarios tagged @unix stand in for systemd: the service finds on
  its PATH a systemctl that reports the unit's ActiveState and MainPID,
  whose main process is the mock PHD2, and the scenario starts the mock
  again on its port 2 s after it exits, as the unit's Restart=always does.
  The suite's phd2.command_timeout is 5s and its phd2.restart_timeout 60s.

  Scenario: A restart on a host not switched over is refused
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When the client restarts PHD2
    Then the response status should be 501
    And the response error should be "restart_unsupported" mentioning "phd2.managed is false"
    And the mock PHD2 should not have received a "shutdown" request

  Scenario: A restart of a PHD2 on another host is refused
    Given the guider service is running switched over to a PHD2 at "192.0.2.10"
    When the client restarts PHD2
    Then the response status should be 501
    And the response error should be "restart_unsupported" mentioning "loopback"

  Scenario: A restart posted as a form is refused with 415
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When the client posts a form to the PHD2 restart route
    Then the response status should be 415
    And the mock PHD2 should not have received a "shutdown" request

  @unix
  Scenario: A restart while the unit runs no PHD2 is refused
    Given the rusty-photon-phd2 unit is inactive
    And a mock PHD2 that settles successfully
    And the guider service is running switched over to its managed PHD2
    When the client restarts PHD2
    Then the response status should be 501
    And the response error should be "restart_unsupported" mentioning "rusty-photon-phd2 is inactive"
    And the mock PHD2 should not have received a "shutdown" request

  @unix
  Scenario: A restart while PHD2 guides is refused with guiding_active
    Given the rusty-photon-phd2 unit keeps PHD2 running
    And a mock PHD2 that is guiding
    And the guider service is running switched over to its managed PHD2
    When the client restarts PHD2
    Then the response status should be 409
    And the response error should be "guiding_active" mentioning "PHD2 is Guiding"
    And the mock PHD2 should not have received a "stop_capture" request
    And the mock PHD2 should not have received a "shutdown" request

  @unix
  Scenario: A restart while the facade camera is connected is refused with camera_connected
    Given the rusty-photon-phd2 unit keeps PHD2 running
    And a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running switched over to its managed PHD2 with the camera facade enabled
    And the facade camera is connected
    When the client restarts PHD2
    Then the response status should be 409
    And the response error should be "camera_connected"
    And the mock PHD2 should not have received a "shutdown" request

  @unix
  Scenario: PHD2 is stopped through its RPC, started again by its unit, and its equipment connected
    Given the rusty-photon-phd2 unit keeps PHD2 running
    And a mock PHD2 that settles successfully
    And the guider service is running switched over to its managed PHD2
    When the client restarts PHD2
    Then the response status should be 200
    And the response field "phd2" should be "restarted"
    And the response field "equipment" should be "connected"
    And the response field "elapsed_ms" should be a whole number
    And the first PHD2 exited on its own with code 0
    And the mock PHD2s should have received these requests in this order:
      | method        | params |
      | get_app_state |        |
      | stop_capture  |        |
      | set_connected | false  |
      | shutdown      |        |
      | set_connected | true   |
      | get_connected |        |

  @unix
  Scenario: A PHD2 that ignores shutdown is ended with SIGTERM 30 s later and started again
    Given the rusty-photon-phd2 unit keeps PHD2 running
    And a mock PHD2 that ignores shutdown
    And the guider service is running switched over to its managed PHD2
    When the client restarts PHD2
    Then the response status should be 200
    And the response field "elapsed_ms" should be at least 30000
    And the guider service should have logged "ending PHD2: it has not exited 30 s after shutdown"
    And the first PHD2 was ended by SIGTERM
    And the mock PHD2s should have received these requests in this order:
      | method        | params |
      | shutdown      |        |
      | set_connected | true   |
      | get_connected |        |

  @unix
  Scenario: A PHD2 that accepts the connection and never answers is ended with SIGTERM and started again
    Given the rusty-photon-phd2 unit keeps PHD2 running
    And a mock PHD2 that accepts connections and never answers
    And the guider service is running switched over to its managed PHD2
    When the client restarts PHD2
    Then the response status should be 200
    And the response field "equipment" should be "connected"
    And the guider service should have logged "ending PHD2: it accepted the connection and did not answer"
    And the first PHD2 was ended by SIGTERM
    And the mock PHD2s should have received these requests in this order:
      | method        | params |
      | set_connected | true   |
      | get_connected |        |
    And the mock PHD2 should not have received a "shutdown" request

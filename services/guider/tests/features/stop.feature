Feature: Stopping the guider
  `POST /api/v1/guiding/stop` stops the loop, whatever runs, and answers
  `{"state": "idle"}` (docs/services/guider.md § `POST
  /api/v1/guiding/stop`, § The camera loop). It is accepted in every state
  and idempotent: in `idle` it answers at once. It does not queue behind
  other requests, and it answers a request that is waiting with `stopped`.
  It answers when the guider's exposure has ended: the guider aborts its
  own exposure when the camera reports `CanAbortExposure`, and waits for it
  to end otherwise. If that takes longer than `stop_timeout` the answer is
  `stop_timeout`. The state stays what it was until the exposure has ended.
  `stats` reports the stop as `last_stop`, with the reason `requested`.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing, and a mount, each on a loopback server
  of its own. The guider polls them every 100 ms and exposes for 200 ms
  unless the scenario says otherwise.

  Scenario: A stop in idle answers at once and records no stop
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    When a client asks the guider to stop
    Then the request answers 200 with the body {"state": "idle"}
    And the guider's last stop is null

  Scenario: A stop ends the loop and aborts the guider's own exposure
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When a client asks the guider to stop
    Then the request answers 200 with the body {"state": "idle"}
    And the simulated camera has received 1 abort
    And the guider's stats report the state "idle"
    And the guider's last stop has the reason "requested"

  Scenario: A loop waiting for its first frame answers stopped
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When a client asks the guider to stop
    Then the background request answers 409 with the error "stopped"

  Scenario: A stop of a camera that cannot abort waits for the exposure to end
    Given the simulated camera and mount, both connected
    And the simulated camera cannot abort an exposure
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "3s"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When a client asks the guider to stop
    Then the request answers 200 with the body {"state": "idle"}
    And the simulated camera's last exposure has ended
    And the simulated camera has received 0 aborts

  Scenario: A stop that outlasts stop_timeout answers stop_timeout, and the state stays until the exposure ends
    Given the simulated camera and mount, both connected
    And the simulated camera cannot abort an exposure
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "10s"
    And the guider configuration's "/stop_timeout" is the JSON "1s"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When a client asks the guider to stop
    Then the request answers 504 with the error "stop_timeout"
    And the guider's stats report the state "looping"
    And the guider's stats report the state "idle" once the simulated camera's last exposure has ended
    And the simulated camera has started 1 exposure

  Scenario: A loop after a stop starts afresh
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider is started
    And the guider is looping
    And a client asks the guider to stop
    When a client asks the guider to loop
    Then the request answers 200 with the body {"state": "looping", "star_count": 3}
    And the guider's stats report the state "looping"

  Scenario: A guider told to shut down aborts its own exposure
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/exposure" is the JSON "2m"
    And the guider is started
    And a client asks the guider to loop in the background
    And the simulated camera has started an exposure
    When the guider is told to shut down
    Then the simulated camera has received 1 abort

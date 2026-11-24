Feature: Health
  `GET /health` answers `200 {"status": "ok"}` while the configuration names
  both devices and the focal length, and both devices answer and are
  connected (docs/services/guider.md § `GET /health`). Otherwise it answers
  `503 {"status": "unavailable", "message": "..."}`: alive but degraded,
  with `message` naming everything missing, the configuration's gaps first,
  then the camera's state, then the mount's, joined by "; ". A device
  counts as not answering until its first poll answers. The answer comes
  from the configuration and the device poll, so a probe costs no device
  call.

  Scenario: A configured rig with both devices connected is healthy
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then /health answers 200 with the body {"status": "ok"}

  Scenario: A configuration with no focal length is not healthy
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and no focal length
    When the guider is started
    Then /health answers 503 with the message "the configuration names no focal length"

  Scenario: A camera that is not connected is not healthy
    Given the simulated camera not connected and the simulated mount connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then /health answers 503 with the message "the camera is not connected"

  Scenario: A mount whose server stops answering is not healthy, and is healthy again once it answers
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    And /health answers 200 with the body {"status": "ok"}
    And the mount's server stops answering
    Then /health answers 503 with the message "the mount does not answer"
    And the mount's server answers again
    And /health answers 200 with the body {"status": "ok"}

  Scenario: A device number its server does not list is named
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/devices/mount/device_number" is the JSON 2
    When the guider is started
    Then /health answers 503 with the message "the mount's server lists no telescope at device number 2"

  Scenario: Every gap is named, the configuration's first
    Given the simulated camera and mount, neither connected
    And a guider configuration naming the camera only and no focal length
    When the guider is started
    Then /health answers 503 with the message "the configuration names no mount; the configuration names no focal length; the camera is not connected"

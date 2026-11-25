Feature: The calibration store
  A calibration made within 60° of the celestial equator is written to
  `calibration.json` in the state directory, in the slot of its pier side:
  `east`, `west` or `unknown` (docs/services/guider.md § Calibration, "The
  store"). `GET /api/v1/calibration` answers the store, a record or `null`
  for each slot. `POST /api/v1/calibration/clear` empties every slot. A
  `calibration.json` the guider cannot read is set aside under another
  name when the service starts, and the store starts empty. The store
  outlasts a restart.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate. The
  guider polls them every 100 ms, exposes for 200 ms, and calibrates over 6
  px in 4 steps.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration calibrates over 6 px in 4 steps

  Scenario: A guider that has calibrated nothing stores nothing
    Given the guider is started
    When a client asks for the calibration store
    Then the request answers 200 with the body {"east": null, "west": null, "unknown": null}

  Scenario: The file holds what the store answers
    Given the guider is started
    And a client asks the guider to calibrate
    When a client asks for the calibration store
    Then the state directory's calibration.json holds the store answered

  Scenario: Clearing empties every slot
    Given the guider is started
    And a client asks the guider to calibrate
    When a client asks the guider to clear the calibration store
    Then the request answers 200 with the body {"state": "idle"}
    And the guider stores no calibration
    And the state directory's calibration.json holds no record

  Scenario: A stored calibration outlasts a restart
    Given the guider is started
    And a client asks the guider to calibrate
    When the guider is started again
    Then the guider's stored calibration for the "east" pier side is the one answered

  Scenario: A calibration.json the guider cannot read is set aside and the store starts empty
    Given the state directory holds a calibration.json of "not a calibration"
    And the guider is started
    When a client asks for the calibration store
    Then the request answers 200 with the body {"east": null, "west": null, "unknown": null}
    And the state directory holds a file set aside from calibration.json holding "not a calibration"

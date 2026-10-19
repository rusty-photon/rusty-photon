@serial
Feature: Runtime pointing API
  The service exposes GET and POST endpoints under /sky-survey/position
  to read and update the simulated camera pointing at runtime. RA must
  be in [0, 360) and Dec in [-90, +90]. POST while disconnected returns
  409. Updates take effect on the next StartExposure.

  Scenario: GET returns the initial pointing
    Given the camera is connected with initial pointing RA 83.8221 Dec -5.3911
    When I GET the position endpoint
    Then the position response status is 200
    And the position response reports RA 83.8221 Dec -5.3911

  Scenario: POST updates pointing while connected
    Given the camera is connected with initial pointing RA 0.0 Dec 0.0
    When I POST RA 10.5 Dec 41.2 to the position endpoint
    Then the position response status is 204
    When I GET the position endpoint
    Then the position response reports RA 10.5 Dec 41.2

  Scenario: POST without rotation preserves the previous rotation
    Given the camera is connected with initial pointing RA 0.0 Dec 0.0
    When I POST RA 10.0 Dec 20.0 rotation 45.0 to the position endpoint
    And I POST RA 11.0 Dec 21.0 to the position endpoint
    When I GET the position endpoint
    Then the position response reports rotation 45.0

  Scenario Outline: Out-of-range coordinates are rejected
    Given the camera is connected with initial pointing RA 0.0 Dec 0.0
    When I POST RA <ra> Dec <dec> to the position endpoint
    Then the position response status is 400

    Examples:
      | ra     | dec   |
      | -1.0   | 0.0   |
      | 360.0  | 0.0   |
      | 720.0  | 0.0   |
      | 0.0    | -91.0 |
      | 0.0    | 91.0  |

  Scenario: Malformed JSON body is rejected
    Given the camera is connected with initial pointing RA 0.0 Dec 0.0
    When I POST a malformed JSON body to the position endpoint
    Then the position response status is 400

  Scenario: POST while disconnected returns 409
    Given the camera is started but not connected
    When I POST RA 10.0 Dec 20.0 to the position endpoint
    Then the position response status is 409

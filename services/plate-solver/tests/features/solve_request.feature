Feature: POST /api/v1/solve — request handling and error surface

  The solve endpoint accepts a FITS path plus optional pointing/FOV/
  search-radius hints, runs ASTAP under the request's deadline, and
  returns the parsed WCS solution or one of the five frozen error codes.

  The complete HTTP contract — request fields, response fields, error
  code table — lives in `docs/plans/archive/plate-solver.md` §"HTTP contract".
  Each scenario below exercises that contract one branch at a time.

  Background:
    Given the wrapper is running with mock_astap as its solver

  Scenario: Happy path returns the scalar WCS fields and the full matrix
    Given mock_astap is configured for "normal" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 200
    And the response field "ra_center" is approximately 10.6848
    And the response field "dec_center" is approximately 41.2690
    And the response field "pixel_scale_arcsec" is approximately 1.05
    And the response field "rotation_deg" is approximately 12.3
    And the response field "solver" contains "astap" case-insensitively
    And the response field "wcs_matrix.crpix1" is approximately 512.0
    And the response field "wcs_matrix.crpix2" is approximately 384.0
    And the response field "wcs_matrix.cd1_1" is approximately -0.000284972 within 0.000000001 degrees
    And the response field "wcs_matrix.cd1_2" is approximately -0.000062134 within 0.000000001 degrees
    And the response field "wcs_matrix.cd2_1" is approximately -0.000062134 within 0.000000001 degrees
    And the response field "wcs_matrix.cd2_2" is approximately 0.000284972 within 0.000000001 degrees

  Scenario: Non-existent fits_path returns fits_not_found
    Given a non-existent FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 404
    And the response field "error" is "fits_not_found"

  Scenario: fits_path pointing at a directory returns fits_not_found
    Given a fits_path pointing at a directory
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 404
    And the response field "error" is "fits_not_found"
    And the response field "message" contains "not a regular file"

  @unix
  Scenario: fits_path with read permission denied returns fits_not_found
    Given an unreadable FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 404
    And the response field "error" is "fits_not_found"

  Scenario: Non-absolute fits_path returns invalid_request
    When I POST to /api/v1/solve with fits_path "relative/path.fits"
    Then the response status is 400
    And the response field "error" is "invalid_request"

  Scenario: Malformed JSON body returns invalid_request
    When I POST to /api/v1/solve with raw body "{not json"
    Then the response status is 400
    And the response field "error" is "invalid_request"

  Scenario: Unparseable timeout returns invalid_request
    Given a writable FITS path
    When I POST to /api/v1/solve with that fits_path and timeout "not-a-duration"
    Then the response status is 400
    And the response field "error" is "invalid_request"

  Scenario: ASTAP exits non-zero returns solve_failed with stderr tail
    Given mock_astap is configured for "exit_failure" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 422
    And the response field "error" is "solve_failed"
    And the response field "details.exit_code" is 1
    And the response field "details.stderr_tail" contains "simulated solve failure"

  Scenario: ASTAP exits clean but writes no .wcs returns solve_failed
    Given mock_astap is configured for "no_wcs" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 422
    And the response field "error" is "solve_failed"
    And the response field "message" contains "did not write"

  Scenario: ASTAP writes a malformed .wcs returns solve_failed naming the missing key
    Given mock_astap is configured for "malformed_wcs" mode
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path
    Then the response status is 422
    And the response field "error" is "solve_failed"
    And the response field "message" contains "CRVAL2"

  Scenario Outline: Hint flags pass through to ASTAP argv
    Given mock_astap is configured for "normal" mode
    And mock_astap is configured to write argv to a side-channel file
    And a writable FITS path
    When I POST to /api/v1/solve with that fits_path and hint <field> set to <value>
    Then the response status is 200
    And the spawned argv contains the flag "<flag>"
    And the spawned argv value after "<flag>" is approximately <converted>

    Examples:
      | field             | value   | flag | converted |
      | ra_hint           | 10.6848 | -ra  | 0.71232   |
      | dec_hint          | 41.2690 | -spd | 131.2690  |
      | fov_hint_deg      | 1.5     | -fov | 1.5       |
      | search_radius_deg | 5.0     | -r   | 5.0       |

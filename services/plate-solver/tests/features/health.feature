Feature: GET /health — readiness probe

  The /health endpoint is the standard HTTP health-probe pattern,
  exposed for Sentinel's service health supervision and any other
  operational tooling.
  It checks both runtime dependencies — the configured ASTAP binary
  and the configured database directory — match the startup-validation
  set, so "healthy at probe time" means "still capable of solving."

  The probe is intentionally cheap: two filesystem stats, no subprocess
  spawn. Frequent polling does not cost wrapper performance.

  A 503 names the failed check in the "status" field and carries a
  human-readable "message" with the offending path. Sentinel counts a
  503 as alive-but-degraded — a missing binary or database is not
  curable by a service restart — and displays the message verbatim on
  its dashboard (see sentinel's service_health.feature).

  Scenario: Health returns 200 with status ok after a clean start
    Given the wrapper is running with mock_astap as its solver
    When I GET /health
    Then the response status is 200
    And the response field "status" is "ok"

  Scenario: Health returns 503 when the configured binary is removed
    Given the wrapper is running with a temp-dir copy of mock_astap as its binary path
    When I delete the configured astap_binary_path
    And I GET /health
    Then the response status is 503
    And the response field "status" is "binary_unavailable"
    And the response field "message" contains "ASTAP binary"

  Scenario: Health returns 503 when the configured db directory is removed
    Given the wrapper is running with a temp astap_db_directory
    When I delete the configured astap_db_directory
    And I GET /health
    Then the response status is 503
    And the response field "status" is "db_unavailable"
    And the response field "message" contains "star database"

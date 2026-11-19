Feature: Reported-position altitude floor
  The device keeps two positions. Virtual pointing is where the last slew or
  sync converged. The reported position — what RightAscension and Declination
  return — equals the virtual pointing while its computed altitude is at or
  above report_altitude_floor_deg (default 10.0); below that it snaps to the
  idle point: RA = current sidereal time, Dec = site latitude (the zenith).
  The floor is defense against a client that refuses GoTo while the scope
  reads below the horizon, never import policy: a below-floor Align still
  imports. A null floor disables the policy and reports raw virtual pointing.

  Background:
    Given the configured site is latitude 33.0 longitude -117.0

  Scenario: Below-floor pointing reports the zenith idle point under the default floor
    Given the bridge is running
    And a connected planetarium client
    When the client aligns at ra 5.0 hours dec -80.0 degrees
    Then the reported position should be the idle point: RA = sidereal time, Dec = site latitude

  Scenario: A below-floor slew converges but reports the idle point
    Given the simulated slew duration is "1s"
    And the bridge is running
    And a connected planetarium client
    When the client starts an async slew to ra 5.0 hours dec -80.0 degrees
    And all simulated slews have completed
    Then the reported position should be the idle point: RA = sidereal time, Dec = site latitude

  Scenario: A below-floor Align imports while reporting the idle point
    Given a stub rp MCP server is running
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 5.0 hours dec -80.0 degrees
    Then the stub rp should receive 1 add_target call
    And the import should carry ra_hours 5.0 and dec_degrees -80.0
    And the reported position should be the idle point: RA = sidereal time, Dec = site latitude

  Scenario: A null floor reports raw virtual pointing
    Given the reported-position altitude floor is disabled
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 5.0 hours dec -80.0 degrees
    Then the reported position should be ra 5.0 hours dec -80.0 degrees

  Scenario: Pointing at or above the floor is reported unchanged
    Given the reported-position altitude floor is 10.0 degrees
    And the bridge is running
    And a connected planetarium client
    When the client aligns at the zenith
    Then the reported position should be the zenith

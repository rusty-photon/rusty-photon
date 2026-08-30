Feature: Virtual telescope device contract
  planetarium-bridge serves one ASCOM Alpaca Telescope (device number 0) that
  planetarium apps connect to as if it were a mount, while it never touches
  hardware. The capability matrix is minimal-but-coherent: every false
  capability's verbs return NOT_IMPLEMENTED, every true capability behaves
  per the ASCOM contract. Sync verbs set the virtual pointing and the Target
  properties; slew verbs simulate motion with a configurable convergence
  window. Site pushes from the client are adopted live; the host clock is
  authoritative for UTC.

  Everything here runs with the reported-position altitude floor disabled so
  read-backs are wall-clock-independent — the floor is position_policy.feature's
  concern (mirroring the ConformU configuration, which also nulls the floor).

  Background:
    Given the reported-position altitude floor is disabled

  Scenario: The device declares itself a virtual target-entry device, not a mount
    Given the bridge is running
    And a connected planetarium client
    Then the device name should be "Planetarium Bridge (virtual target entry — NOT a mount)"
    And the device description should contain "NOT a mount"

  Scenario Outline: Capability matrix
    Given the bridge is running
    And a connected planetarium client
    Then <member> should read <value>

    Examples:
      | member                   | value       |
      | AlignmentMode            | GermanPolar |
      | EquatorialSystem         | J2000       |
      | CanSlew                  | true        |
      | CanSlewAsync             | true        |
      | CanSync                  | true        |
      | CanSyncAltAz             | false       |
      | CanSlewAltAz             | false       |
      | CanSlewAltAzAsync        | false       |
      | CanPark                  | false       |
      | CanUnpark                | false       |
      | CanFindHome              | false       |
      | CanSetPark               | false       |
      | AtPark                   | false       |
      | CanMoveAxis              | false       |
      | CanPulseGuide            | false       |
      | CanSetGuideRates         | false       |
      | CanSetTracking           | false       |
      | CanSetDeclinationRate    | false       |
      | CanSetRightAscensionRate | false       |
      | Tracking                 | true        |

  Scenario: SideOfPier is not implemented
    Given the bridge is running
    And a connected planetarium client
    When I try to read SideOfPier
    Then the call should be rejected with ASCOM NOT_IMPLEMENTED

  Scenario: The device advertises no MoveAxis rates
    Given the bridge is running
    And a connected planetarium client
    Then the primary axis should advertise no axis rates

  Scenario Outline: Verbs for declared-false capabilities are not implemented
    Given the bridge is running
    And a connected planetarium client
    When I try to invoke <verb>
    Then the call should be rejected with ASCOM NOT_IMPLEMENTED

    Examples:
      | verb                 |
      | Park                 |
      | Unpark               |
      | FindHome             |
      | SetPark              |
      | MoveAxis             |
      | PulseGuide           |
      | SetTracking          |
      | SetDeclinationRate   |
      | SetRightAscensionRate|
      | SyncToAltAz          |
      | SlewToAltAz          |
      | SlewToAltAzAsync     |

  Scenario: UTCDate reads the host clock and rejects writes
    Given the bridge is running
    And a connected planetarium client
    Then UTCDate should read within 5 seconds of the harness clock
    When I try to set the device UTC date
    Then the call should be rejected with ASCOM NOT_IMPLEMENTED

  Scenario: Sync sets the virtual pointing and the Target properties
    Given the bridge is running
    And a connected planetarium client
    When the client aligns at ra 5.5 hours dec 40.0 degrees
    Then the reported position should be ra 5.5 hours dec 40.0 degrees
    And TargetRightAscension should read 5.5 hours
    And TargetDeclination should read 40.0 degrees

  Scenario: An async slew returns immediately and reports Slewing while it converges
    Given the simulated slew duration is "30s"
    And the bridge is running
    And a connected planetarium client
    When the client starts an async slew to ra 5.5 hours dec 40.0 degrees
    Then Slewing should read true
    And TargetRightAscension should read 5.5 hours
    And TargetDeclination should read 40.0 degrees
    When the client aborts the slew
    Then Slewing should read false

  Scenario: An async slew converges to the requested coordinates
    Given the simulated slew duration is "1s"
    And the bridge is running
    And a connected planetarium client
    When the client starts an async slew to ra 5.5 hours dec 40.0 degrees
    Then Slewing should read false once the simulated slew converges
    And the reported position should be ra 5.5 hours dec 40.0 degrees

  Scenario: The blocking slew form completes after convergence
    Given the simulated slew duration is "1s"
    And the bridge is running
    And a connected planetarium client
    When the client slews blocking to ra 6.0 hours dec 30.0 degrees
    Then Slewing should read false
    And the reported position should be ra 6.0 hours dec 30.0 degrees

  Scenario: SlewToTarget slews to the previously set Target properties
    Given the simulated slew duration is "1s"
    And the bridge is running
    And a connected planetarium client
    When the client sets TargetRightAscension to 4.0 hours
    And the client sets TargetDeclination to 10.0 degrees
    And the client slews to target asynchronously
    Then Slewing should read false once the simulated slew converges
    And the reported position should be ra 4.0 hours dec 10.0 degrees

  Scenario: A new slew during convergence supersedes the old one
    Given the simulated slew duration is "2s"
    And the bridge is running
    And a connected planetarium client
    When the client starts an async slew to ra 2.0 hours dec 0.0 degrees
    And the client starts an async slew to ra 20.0 hours dec 50.0 degrees
    Then Slewing should read false once the simulated slew converges
    And the reported position should be ra 20.0 hours dec 50.0 degrees

  Scenario: AbortSlew ends the simulated motion
    Given the simulated slew duration is "10s"
    And the bridge is running
    And a connected planetarium client
    When the client starts an async slew to ra 12.0 hours dec 20.0 degrees
    And the client aborts the slew
    Then Slewing should read false

  Scenario Outline: Out-of-range slew coordinates are rejected with InvalidValue
    Given the bridge is running
    And a connected planetarium client
    When the client tries an async slew to ra <ra> hours dec <dec> degrees
    Then the call should be rejected with ASCOM INVALID_VALUE

    Examples:
      | ra   | dec   |
      | 24.0 | 10.0  |
      | -0.1 | 10.0  |
      | 5.0  | 90.5  |
      | 5.0  | -90.5 |

  Scenario: Site pushes from the client are adopted live
    Given the bridge is running
    And a connected planetarium client
    When the client sets SiteLatitude to 33.08731 degrees
    And the client sets SiteLongitude to -117.2535 degrees
    Then SiteLatitude should read 33.08731 degrees
    And SiteLongitude should read -117.2535 degrees

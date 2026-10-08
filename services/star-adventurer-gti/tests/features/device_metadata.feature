Feature: Device metadata
  The mount device reports its identity, capability flags, and static
  properties via the ASCOM Alpaca HTTP API. Capability values reflect the
  MVP scope: GermanPolar alignment, Topocentric coordinates, sidereal-only
  tracking, PulseGuide / SetGuideRates supported (since #206); no
  MoveAxis / FindHome / Alt-Az. CanSetPark is runtime-determined —
  `true` when the driver was started with `--config` (the BDD harness
  always provides one), `false` for `Config::default()` runs.

  Scenario: Device reports configured name
    Given a star-adventurer service configured with name "Test Mount"
    Then the device name should be "Test Mount"

  Scenario: Device reports configured unique ID
    Given a star-adventurer service configured with unique ID "test-mount-001"
    Then the device unique ID should be "test-mount-001"

  Scenario: Device reports configured description
    Given a star-adventurer service configured with description "Custom GTi description"
    When I connect the device
    Then the device description should be "Custom GTi description"

  Scenario: Driver info names the protocol family
    Given a running star-adventurer service
    When I connect the device
    Then the driver info should contain "Star Adventurer"

  Scenario: Driver version is non-empty
    Given a running star-adventurer service
    When I connect the device
    Then the driver version should not be empty

  Scenario: Capability flags match the MVP table
    Given a running star-adventurer service
    When I connect the device
    Then the device capabilities should match these values:
      | capability                  | value        |
      | AlignmentMode               | GermanPolar  |
      | EquatorialSystem            | Topocentric  |
      | CanSlew                     | true         |
      | CanSlewAsync                | true         |
      | CanSlewAltAz                | false        |
      | CanSlewAltAzAsync           | false        |
      | CanSync                     | true         |
      | CanSyncAltAz                | false        |
      | CanSetTracking              | true         |
      | CanSetRightAscensionRate    | false        |
      | CanSetDeclinationRate       | false        |
      | CanSetGuideRates            | true         |
      | CanPulseGuide               | true         |
      | CanFindHome                 | false        |
      | CanPark                     | true         |
      | CanUnpark                   | true         |
      | CanSetPark                  | true         |
      | CanSetPierSide              | true         |
      | DoesRefraction              | false        |

  Scenario: Tracking rates list contains only sidereal
    Given a running star-adventurer service
    When I connect the device
    Then TrackingRates should equal [Sidereal]

  Scenario: Site latitude reads from configuration
    Given a star-adventurer service configured with site latitude 37.7749
    When I connect the device
    Then SiteLatitude should be 37.7749 degrees

  Scenario: Site longitude reads from configuration
    Given a star-adventurer service configured with site longitude -122.4194
    When I connect the device
    Then SiteLongitude should be -122.4194 degrees

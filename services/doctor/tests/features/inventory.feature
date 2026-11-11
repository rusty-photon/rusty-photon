Feature: Install inventory diagnosis
  On a packaged host (one where the service manager reports at least one
  rusty-photon-* unit), doctor cross-checks the unit inventory against the
  config directory. A unit without a config file means the service has never
  started (services self-create config on first run) or writes its state
  somewhere unexpected. A config file for a catalog service with no unit is
  a leftover or a hand-copied stray. A .json file matching no catalog
  service is a typo a service will silently never read. Known non-service
  files (acme.json, the pki tree) are exempt, and so is a file a service
  declares as its state file (star-adventurer-gti's mount state record),
  which renaming or removing would destroy. On a dev checkout with no
  units, none of these mismatches are meaningful and the inventory checks do
  not run.

  Scenario: An installed unit with no config file warns
    Given an empty config directory
    And platform facts with an enabled unit "rusty-photon-qhy-focuser"
    When I run doctor with --json
    Then the report contains a "warn" check named "inventory.unit-without-config" for service "qhy-focuser"
    And that check's suggestion mentions "start"

  Scenario: A config file whose unit is not installed warns
    Given a config directory with a valid "dsd-fp2.json" on port 11119
    And platform facts with an enabled unit "rusty-photon-qhy-focuser"
    When I run doctor with --json
    Then the report contains a "warn" check named "inventory.config-without-unit" for service "dsd-fp2"

  Scenario: A config file matching no catalog service warns
    Given a config directory with a valid "qhy-focuser.json" on port 11113
    And a config file "qhy-focusser.json" containing:
      """
      { "server": { "port": 11199 } }
      """
    And platform facts with an enabled unit "rusty-photon-qhy-focuser"
    When I run doctor with --json
    Then the report contains a "warn" check named "inventory.unknown-config"
    And that check's detail mentions "qhy-focusser.json"

  Scenario: Known non-service files are not flagged
    Given a config directory with a valid "qhy-focuser.json" on port 11113
    And a config file "acme.json" containing:
      """
      { "provider": "letsencrypt" }
      """
    And platform facts with an enabled unit "rusty-photon-qhy-focuser"
    When I run doctor with --json
    Then the report has no checks named "inventory.unknown-config"

  Scenario: A service's state file beside the configs is not flagged
    Given a config directory with a valid "star-adventurer-gti.json" on port 11117
    And a config file "star-adventurer-gti.mount-state.json" containing:
      """
      { "state": "at_rest", "ra_ticks": -907200, "dec_ticks": 725760 }
      """
    And platform facts with an enabled unit "rusty-photon-star-adventurer-gti"
    When I run doctor with --json
    Then the report has no checks named "inventory.unknown-config"

  Scenario: A dev checkout skips inventory checks entirely
    Given a config directory with a valid "session-runner.json" on port 11171
    And platform facts with no rusty-photon units
    When I run doctor with --json
    Then the report field "mode" is "config-only"
    And the report has no checks named "inventory.unknown-config"
    And the report has no checks named "inventory.config-without-unit"

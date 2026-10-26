Feature: Repair — doctor --fix
  A default doctor run is read-only. --fix applies the machine-applicable
  fixes the checks planned — a port collision moved back to a free catalog
  default, a retired key deleted (sentinel's services map, ui-htmx's
  drivers map and sentinel block) — then re-diagnoses and reports the post-fix state, exit
  code included. Writes go through the same atomic save path the services' own
  setup pages save through, every field doctor does not touch is preserved (the
  formatting normalizes to the pretty-printed shape a setup page's save writes),
  and a second --fix run applies nothing. Judgment calls stay suggestions:
  discovery_port collisions, hand-set TLS material paths, and rp's
  equipment alpaca_url (inside the device-usage block doctor checks but
  does not own) are never written. The provisioning pass --fix also runs is
  provisioning.feature's subject.

  Background:
    Given platform facts with enabled units:
      | unit                     |
      | rusty-photon-sentinel    |
      | rusty-photon-qhy-focuser |
      | rusty-photon-ui-htmx     |
      | rusty-photon-dsd-fp2     |
      | rusty-photon-rp          |

  Scenario: A port collision is fixed by returning a service to its free default
    Given a config directory with "qhy-focuser.json" containing:
      """
      { "server": { "port": 11119 }, "device_overrides": { "keep": true } }
      """
    And a config file "dsd-fp2.json" containing:
      """
      { "server": { "port": 11119 } }
      """
    When I run doctor with --fix and --json
    Then doctor exits with code 0
    And the report records an applied fix for check "ports.collision" on service "qhy-focuser"
    And the config file "qhy-focuser.json" has the number 11113 at "/server/port"
    And the config file "qhy-focuser.json" has JSON true at "/device_overrides/keep"
    And the config file "dsd-fp2.json" has the number 11119 at "/server/port"
    And the report has no checks named "ports.collision" with status "fail"

  Scenario: The retired sentinel services map is deleted
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "services": { "rp": { "restart_command": "systemctl restart rusty-photon-rp" } },
        "operation_watchdog": { "rp_url": "http://localhost:11115" } }
      """
    When I run doctor with --fix and --json
    Then doctor exits with code 0
    And the report records an applied fix for check "config.retired-keys" on service "sentinel"
    And the config file "sentinel.json" has no value at "/services"
    And the config file "sentinel.json" has the string "http://localhost:11115" at "/operation_watchdog/rp_url"
    And the report has no checks named "config.retired-keys"

  Scenario: The retired ui-htmx drivers map and sentinel block are deleted
    Given a config directory with "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 },
        "drivers": {
          "qhy-focuser": { "base_url": "http://localhost:11113" } },
        "sentinel": { "base_url": "http://localhost:11114" } }
      """
    When I run doctor with --fix and --json
    Then doctor exits with code 0
    And the report records an applied fix for check "config.retired-keys" on service "ui-htmx"
    And the config file "ui-htmx.json" has no value at "/drivers"
    And the config file "ui-htmx.json" has no value at "/sentinel"
    And the config file "ui-htmx.json" has the number 11120 at "/server/port"

  Scenario: A second --fix run applies nothing
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "services": { "rp": { "restart_command": "x" } } }
      """
    When I run doctor with --fix and --json
    And I run doctor with --fix and --json
    Then doctor exits with code 0
    And the report records no applied fixes

  Scenario: A default run never writes
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "services": { "rp": { "restart_command": "x" } } }
      """
    When I run doctor with --json
    Then doctor exits with code 1
    And the config file "sentinel.json" is unchanged from what was staged
    And the report contains a "fail" check named "config.retired-keys" for service "sentinel"

  Scenario: Unfixable failures survive --fix and keep the exit code
    Given a config directory with "rp.json" containing:
      """
      { "server": { "port": 11115,
          "tls": { "cert": "{missing}/cert.pem", "key": "{missing}/key.pem" } } }
      """
    When I run doctor with --fix and --json
    Then doctor exits with code 1
    And the report contains a "fail" check named "tls.paths" for service "rp"
    And the config file "rp.json" has the string "{missing}/cert.pem" at "/server/tls/cert"

  Scenario: The JSON report carries the machine-applicable fix plan
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "services": { "rp": { "restart_command": "x" } } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "config.retired-keys" for service "sentinel"
    And that check's fix plan removes the key at "/services"

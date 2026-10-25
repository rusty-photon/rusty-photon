Feature: Cross-config name join diagnosis
  Since sentinel discovers its services from the platform service manager
  (D3s), the watchdog's operations.<family>.service names resolve against
  the installed rusty-photon-* units — matched by convention and validated
  by nothing at runtime until the 2am 404 — so doctor validates that join.
  It also flags the retired config keys: sentinel's services map (D3s),
  ui-htmx's whole drivers override map (#569 — rp's equipment roster is the
  only device source) and ui-htmx's sentinel block (the web UI restarts
  nothing; Sentinel's dashboard does); each keeps its service from starting.

  Background:
    Given platform facts with enabled units:
      | unit                     |
      | rusty-photon-sentinel    |
      | rusty-photon-qhy-focuser |
      | rusty-photon-ui-htmx     |

  Scenario: A retired sentinel services map keeps sentinel from starting
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "services": {
          "focuser": { "restart_command": "systemctl restart rusty-photon-qhy-focuser" } } }
      """
    When I run doctor with --json
    Then doctor exits with code 1
    And the report contains a "fail" check named "config.retired-keys" for service "sentinel"
    And that check's suggestion mentions "delete"

  Scenario: A retired ui-htmx drivers map keeps ui-htmx from starting
    Given a config directory with "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 },
        "drivers": {
          "qhy-focuser": { "base_url": "http://localhost:11113" } } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "config.retired-keys" for service "ui-htmx"
    And that check's detail mentions "drivers"

  Scenario: A retired ui-htmx sentinel block keeps ui-htmx from starting
    Given a config directory with "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 },
        "sentinel": { "base_url": "http://localhost:11114" } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "config.retired-keys" for service "ui-htmx"
    And that check's detail mentions "sentinel block"

  Scenario: A watchdog operation naming an uninstalled service dangles
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "operation_watchdog": {
          "rp_url": "http://localhost:11115",
          "operations": { "slew": { "service": "mount" } } } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "joins.watchdog-service" for service "sentinel"
    And that check's detail mentions "mount"
    And that check's suggestion mentions "qhy-focuser"

  Scenario: A watchdog operation naming an installed service resolves
    Given a config directory with "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "operation_watchdog": {
          "rp_url": "http://localhost:11115",
          "operations": { "move_focuser": { "service": "qhy-focuser" } } } }
      """
    When I run doctor with --json
    Then the report has no checks named "joins.watchdog-service"

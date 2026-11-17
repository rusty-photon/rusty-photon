Feature: Service discovery
  Sentinel has no configured service registry. It enumerates the installed
  rusty-photon-* service units from the platform service manager — excluding
  its own unit — at startup and again every discovery cycle, so an installed
  package appears and a removed one disappears without restarting sentinel.
  Each discovered service is classified by run state: running, failed, inert
  (a start condition such as the config-file gate is unmet), stopped (the
  operator stopped it), or disabled. GET /api/services reports every
  discovered service with its unit name, run state, and probed health; a
  service whose own config file cannot be read has no derivable probe URL
  and reports health "unknown". Only running and failed services are
  supervised — the rest are displayed and left alone. In these scenarios the
  platform service manager is the directory-backed test stub selected by
  SENTINEL_SERVICE_MANAGER_DIR.

  Scenario: Discovered services are listed with their run states
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And a discovered unit "rusty-photon-plate-solver" in state "inert"
    And a discovered unit "rusty-photon-qhy-focuser" in state "stopped"
    And a discovered unit "rusty-photon-zwo-camera" in state "disabled"
    And sentinel is running with no monitors
    When the services endpoint is requested
    Then the response status should be 200
    And the services response lists "dsd-fp2" with run state "running"
    And the services response lists "plate-solver" with run state "inert"
    And the services response lists "qhy-focuser" with run state "stopped"
    And the services response lists "zwo-camera" with run state "disabled"

  Scenario: Sentinel's own unit is not discovered
    Given a discovered unit "rusty-photon-sentinel" in state "running"
    And a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the services endpoint is requested
    Then the services response lists "dsd-fp2" with run state "running"
    And the services response does not list "sentinel"

  Scenario: The TLS renewal job unit is not discovered or supervised
    Given a discovered unit "rusty-photon-renew" in state "failed"
    And a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the services endpoint is requested
    Then the services response lists "dsd-fp2" with run state "running"
    And the services response does not list "renew"

  Scenario: A service installed while sentinel runs is discovered on the next cycle
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the unit "rusty-photon-qhy-camera" appears in state "running"
    Then the services response eventually lists "qhy-camera" with run state "running"

  Scenario: A removed service is dropped on the next cycle
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And a discovered unit "rusty-photon-qhy-camera" in state "running"
    And sentinel is running with no monitors
    When the unit "rusty-photon-qhy-camera" is removed
    Then the services response eventually does not list "qhy-camera"

  Scenario: A running service with no readable config reports unknown health
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the services endpoint is requested
    Then the services response lists "dsd-fp2" with health "unknown"
    And the service manager records no restarts after a settle period

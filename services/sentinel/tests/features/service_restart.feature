Feature: Service restart API
  Sentinel owns process restart for the observatory stack. For each
  discovered service the dashboard exposes
  POST /api/services/{name}/restart, where {name} is the service's
  discovered name (the unit name minus the rusty-photon- prefix): it runs
  the restart command derived from the unit name and polls the derived
  recovery check (systemctl is-active on Linux) until it passes or the
  restart budget elapses. The command's outcome is a domain result reported
  on HTTP 200 with a JSON body carrying "status" ("ok" or "failed") and
  "recovery" ("healthy", "timeout", or "skipped"); 4xx is reserved for
  addressing errors: 404 for a name no discovered service carries, 409 for
  a restart already in flight. The manual restart is the operator's
  recovery hammer, so it also works for services whose run state autonomous
  supervision leaves alone (stopped, inert, disabled).

  Only the dashboard itself may post a restart: browsers resend cached Basic
  credentials to any request, so a page on another site could otherwise
  restart a service. A request is accepted when its Sec-Fetch-Site is
  same-origin or none, or, carrying none, when its Origin's host and port
  are the request's own Host; any other request, one naming no origin at
  all included, is refused with 403 and runs nothing. A client that is not
  a browser names the dashboard's own origin, as the scenarios do unless
  they say otherwise. Each discovered service's row on the dashboard page
  carries a Restart button: a form posting to that endpoint.

  Scenario: A restart runs the derived command and confirms recovery
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2"
    Then the response status should be 200
    And the restart response reports status "ok" and recovery "healthy"
    And the service manager records a restart of "rusty-photon-dsd-fp2"

  Scenario: Recovery that never confirms is reported as a timeout
    Given a discovered unit "rusty-photon-dsd-fp2" in state "stopped"
    And the service manager leaves restarted units in their prior state
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2"
    Then the response status should be 200
    And the restart response reports status "ok" and recovery "timeout"

  Scenario: A failing restart command is a domain failure, not a transport error
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And the service manager fails restarts of "rusty-photon-dsd-fp2"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2"
    Then the response status should be 200
    And the restart response reports status "failed"

  Scenario: Restarting an unknown service is not found
    Given sentinel is running with no monitors
    When the restart endpoint is requested for "unknown-service"
    Then the response status should be 404

  Scenario: A stopped service can still be restarted manually
    Given a discovered unit "rusty-photon-dsd-fp2" in state "stopped"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2"
    Then the response status should be 200
    And the restart response reports status "ok" and recovery "healthy"
    And the service manager records a restart of "rusty-photon-dsd-fp2"

  Scenario: A restart posted from the dashboard page itself runs
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2" with Sec-Fetch-Site "same-origin"
    Then the response status should be 200
    And the restart response reports status "ok" and recovery "healthy"
    And the service manager records a restart of "rusty-photon-dsd-fp2"

  Scenario: A restart posted from another site is refused and runs nothing
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2" with Sec-Fetch-Site "cross-site"
    Then the response status should be 403
    And the restart response's error names "another site"
    And the service manager has recorded no restarts

  Scenario: A restart whose Origin is another server's is refused
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2" with Origin "http://elsewhere.example"
    Then the response status should be 403
    And the service manager has recorded no restarts

  Scenario: A restart that names no origin is refused
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the restart endpoint is requested for "dsd-fp2" naming no origin
    Then the response status should be 403
    And the service manager has recorded no restarts

  Scenario: The dashboard offers a Restart button for each discovered service
    Given a discovered unit "rusty-photon-dsd-fp2" in state "running"
    And sentinel is running with no monitors
    When the services response eventually lists "dsd-fp2" with run state "running"
    And the dashboard page is requested
    Then the dashboard's row for "dsd-fp2" has a Restart button posting to "/api/services/dsd-fp2/restart"

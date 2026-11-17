Feature: Service health supervision
  Sentinel supervises every discovered service in the running or failed run
  state — supervision is universal and unconfigured, with all policy values
  constants (poll interval 30s, failure threshold 3, restart backoff 60s
  doubling to 900s, restart budget 300s; these scenarios tighten them
  through the test stub's policy file). A running service is probed with
  GET at its derived health URL: a 200 counts as alive, and so do 401 and
  403 — an auth-enabled service that challenges an unauthenticated probe
  has proven it is up. A 503 counts as alive but degraded: the service's
  HTTP loop answered, deliberately reporting an external dependency (a
  missing ASTAP install) that no service restart can cure — so a 503
  never triggers a restart or a notification, resets any outage in
  progress, and shows the service as degraded on the dashboard.
  When the 503 body is JSON with a top-level "message" string, sentinel
  passes that string through to the dashboard verbatim as opaque display
  text (truncated to 200 characters, never interpreted, never acted on);
  any other body shows no message. Any other status, a timeout, or a
  connection error counts as a failed probe. After the failure threshold
  sentinel runs the
  derived restart command autonomously, then backs off (doubling) before
  any further attempt, probing at the same cadence throughout and never
  giving up. A successful probe resets the outage. A failed unit — one the
  OS supervisor gave up on — has no HTTP to probe and is restarted on the
  same threshold-and-backoff state machine. Stopped, inert, and disabled
  services are never probed or restarted. Autonomous restarts notify and
  are recorded in history; the first restart of an outage notifies at
  normal priority and every later one escalates with a message that says
  the service is still unhealthy. All restart paths share one in-flight
  slot per service, so an autonomous restart never races a manual one.
  Probe URLs dial the host derived from the supervised service's bind
  address — localhost for a wildcard bind — unless the optional
  probe_domain config key is set, in which case every probe dials
  <service>.<probe_domain> instead (the name an ACME wildcard
  certificate's DNS-only SANs can match; it must resolve to the local
  host).

  Scenario: A healthy running service is probed and never restarted
    Given a stub service whose health endpoint answers 200
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the dashboard reports service "plate-solver" health "up"
    Then the service manager records no restarts after a settle period

  Scenario: An auth-challenging service counts as alive
    Given a stub service whose health endpoint answers 401
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the dashboard reports service "plate-solver" health "up"
    Then the service manager records no restarts after a settle period

  Scenario: Consecutive failed probes trigger an autonomous restart
    Given a stub service whose health endpoint answers 500
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with notifiers and no monitors
    Then the service manager records at least 1 restart of "rusty-photon-plate-solver" within 10 seconds
    And the dashboard reports service "plate-solver" health "down"
    And the notification history records an autonomous restart of "plate-solver"

  Scenario: A restart that cures the service ends the outage
    Given a stub service whose health endpoint answers 500
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the service manager records at least 1 restart of "rusty-photon-plate-solver" within 10 seconds
    And the stub service becomes healthy
    Then the dashboard reports service "plate-solver" health "up"
    And the dashboard reports zero restarts in the current outage for "plate-solver"

  Scenario: Restarts that never cure the service back off and escalate
    Given a stub service whose health endpoint answers 500
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with notifiers and no monitors
    Then the service manager records at least 2 restarts of "rusty-photon-plate-solver" within 15 seconds
    And the notification history records an escalated still-unhealthy notification for "plate-solver"
    And the dashboard reports a scheduled next restart for "plate-solver"

  # The stub is healthy on localhost, so "down" proves the probes dialed
  # plate-solver.rig.invalid — .invalid is the reserved TLD that never
  # resolves — instead of the local host.
  Scenario: A configured probe domain replaces the local probe host
    Given a stub service whose health endpoint answers 200
    And the stub service is discovered as "plate-solver" in state "running"
    And a probe domain "rig.invalid" is configured
    And sentinel is running with no monitors
    Then the dashboard reports service "plate-solver" health "down"

  Scenario: A degraded service is alive and never restarted
    Given a stub service whose health endpoint answers 503 with body '{"status":"binary_unavailable","message":"ASTAP binary missing or not executable: /usr/bin/astap"}'
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the dashboard reports service "plate-solver" health "degraded"
    Then the service manager records no restarts after a settle period

  Scenario: A degraded service's own message reaches the dashboard verbatim
    Given a stub service whose health endpoint answers 503 with body '{"status":"binary_unavailable","message":"ASTAP binary missing or not executable: /usr/bin/astap"}'
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the dashboard reports service "plate-solver" health "degraded"
    And the services endpoint is requested
    Then the services response lists "plate-solver" with health message "ASTAP binary missing or not executable: /usr/bin/astap"

  # The stub's default 503 body is JSON without a "message" field.
  Scenario: A degraded answer without a message shows no message
    Given a stub service whose health endpoint answers 503
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the dashboard reports service "plate-solver" health "degraded"
    And the services endpoint is requested
    Then the services response lists "plate-solver" with no health message

  Scenario: A degraded answer ends an outage like a recovery
    Given a stub service whose health endpoint answers 500
    And the stub service is discovered as "plate-solver" in state "running"
    And sentinel is running with no monitors
    When the service manager records at least 1 restart of "rusty-photon-plate-solver" within 10 seconds
    And the stub service starts answering 503
    Then the dashboard reports service "plate-solver" health "degraded"
    And the dashboard reports zero restarts in the current outage for "plate-solver"

  Scenario: A failed unit is restarted without an HTTP probe
    Given a discovered unit "rusty-photon-qhy-focuser" in state "failed"
    And sentinel is running with no monitors
    Then the service manager records at least 1 restart of "rusty-photon-qhy-focuser" within 10 seconds

  Scenario: A stopped service is never probed or restarted
    Given a stub service whose health endpoint answers 503
    And the stub service is discovered as "plate-solver" in state "stopped"
    And sentinel is running with no monitors
    When the services endpoint is requested
    Then the services response lists "plate-solver" with health "unknown"
    And the service manager records no restarts after a settle period

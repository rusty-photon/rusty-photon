Feature: Restart via Sentinel
  Sentinel owns process restart; the BFF is its authorised client. When the
  BFF config carries a sentinel block, a config card renders a "Restart via
  Sentinel" button posting to /config/{service}/restart; the BFF forwards to
  Sentinel's POST /api/services/{name}/restart and renders the outcome. For a
  roster-derived device page the name is found by matching the device's
  alpaca_url port against Sentinel's discovered services (GET /api/services
  probe_port); rp's own page always uses the name "rp". Sentinel restarts the
  discovered unit through the platform service manager. An accepted restart
  swaps in the reconnecting fragment, which polls /config/{service}/status
  until the driver serves its configuration again; a restart the service
  manager fails surfaces Sentinel's failure detail; a device whose driver
  Sentinel has not discovered simply renders no restart button (no port
  match), while rp's own restart against a Sentinel that has not discovered
  rp surfaces Sentinel's not-found reason. Without a sentinel block, no
  restart affordance is rendered.

  Scenario: The config card offers a restart via Sentinel
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And a sentinel that has discovered the running "qhy-focuser" unit
    When I open the qhy-focuser config page
    Then the page offers to restart the driver via Sentinel

  Scenario: An accepted restart restarts the discovered unit and the page reconnects
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And a sentinel that has discovered the running "qhy-focuser" unit
    When I request a restart of the qhy-focuser driver
    Then the service manager records a restart of "rusty-photon-qhy-focuser"
    And the page reports the driver is restarting
    And the page polls the device's status route every 1s for reconnection
    When I poll the reconnect status until max_step 60000 is served
    Then the page shows the value "60000"

  Scenario: A restart the service manager fails surfaces Sentinel's failure detail
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And a sentinel that has discovered the running "qhy-focuser" unit
    And the service manager fails restarts of "rusty-photon-qhy-focuser"
    When I request a restart of the qhy-focuser driver
    Then the page reports the restart failed

  Scenario: A device whose driver Sentinel has not discovered renders no restart button
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And a sentinel that has discovered no services
    When I open the qhy-focuser config page
    Then the page offers no restart affordance

  Scenario: Restarting rp when Sentinel has not discovered it surfaces the not-found reason
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And a sentinel that has discovered no services
    When I request a restart of rp
    Then the page reports Sentinel does not supervise the driver

  Scenario: No sentinel configured renders no restart affordance
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I open the qhy-focuser config page
    Then the page offers no restart affordance

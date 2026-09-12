Feature: Aggregation over the per-service doctors

  On a packaged host, central doctor extends its diagnosis through each
  installed service (docs/services/doctor.md, Aggregation section). An
  active Alpaca-class service is asked over HTTP for its configured
  devices; an installed-but-stopped unit's own binary is run as
  "doctor --json" and its checks merge into the report. Staged units
  without a run state have no aggregation story and are skipped.

  The HTTP probe dials a service by name, not by address: "localhost" on
  a self-signed install, the service's public name on an ACME one. A name
  can resolve into either address family, so an endpoint a scenario
  stands up has to answer on both — otherwise whatever holds the other
  half of that port answers in its place.

  Scenario: an active Alpaca service reports its device inventory
    Given a stub management endpoint serving two configured devices
    And a config file "ppba-driver.json" pointing at the stub endpoint
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains an "ok" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "2 configured device(s)"
    And that check's detail mentions "Stub Camera"

  Scenario: the probe reaches the stub itself, in whichever address family localhost resolves to
    Given a stub management endpoint serving two configured devices
    And a config file "ppba-driver.json" pointing at the stub endpoint
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains an "ok" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "Stub Camera"
    And the stub itself answers on the IPv6 loopback

  Scenario: an active service that does not answer its own port is a failure
    Given a config file "ppba-driver.json" declaring a port nothing listens on
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "fail" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "does not answer"
    And doctor exits with code 1

  Scenario: an authenticated endpoint doctor holds no credential for proves liveness only
    Given a stub management endpoint that requires authentication
    And a config file "ppba-driver.json" pointing at the stub endpoint with auth enabled
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "warn" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "liveness is proven"

  Scenario: the staged observatory credential unlocks an authenticated endpoint
    Given a staged observatory credential
    And a stub management endpoint that requires authentication
    And a config file "ppba-driver.json" pointing at the stub endpoint with auth enabled
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains an "ok" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "2 configured device(s)"

  Scenario: an active TLS service is probed over HTTPS with doctor's own trust root
    Given a config file "ppba-driver.json" containing:
      """
      {}
      """
    And doctor tls issue has already run
    And an HTTPS stub management endpoint for "ppba-driver" serving two configured devices
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains an "ok" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "2 configured device(s)"

  Scenario: an ACME install is probed at the service's public name, not localhost
    Given an acme.json declaring domain "pier1.invalid"
    And a config file "ppba-driver.json" declaring a port nothing listens on
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "fail" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "http://ppba-driver.pier1.invalid"

  Scenario: an ACME install is verified by the public store, so a missing pki tree does not block the probe
    Given an acme.json declaring domain "pier1.invalid"
    And a config file "ppba-driver.json" with a tls block but no pki tree
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "fail" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "https://ppba-driver.pier1.invalid"

  Scenario: a stopped unit's own doctor report merges into the aggregate
    Given a stub per-service doctor for "ppba-driver" whose report has a failing "config.full-shape" check
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts where unit "rusty-photon-ppba-driver" is installed but stopped, with the stub binary
    When I run doctor with --json
    Then the report contains a "fail" check named "config.full-shape" for service "ppba-driver"
    And that check's detail mentions "typo_key"
    And doctor exits with code 1

  Scenario: a binary that predates the doctor subcommand is version skew, not a broken rig
    Given a stub per-service binary for "ppba-driver" that does not know the doctor subcommand
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts where unit "rusty-photon-ppba-driver" is installed but stopped, with the stub binary
    When I run doctor with --json
    Then the report contains a "warn" check named "service.doctor-probe" for service "ppba-driver"
    And that check's detail mentions "did not produce a doctor report"
    And the report has no checks named "service.devices"

  Scenario: a stopped unit whose binary path is unknown is reported, not skipped
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts where unit "rusty-photon-ppba-driver" is installed but stopped, with no known binary path
    When I run doctor with --json
    Then the report contains a "warn" check named "service.doctor-probe" for service "ppba-driver"
    And that check's detail mentions "records no binary path"

  Scenario: staged units without a run state have no aggregation story
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts with an enabled unit "rusty-photon-ppba-driver"
    When I run doctor with --json
    Then the report has no checks named "service.devices"
    And the report has no checks named "service.doctor-probe"

  Scenario: an active core-class service exposes no management API and is not probed
    Given a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 } }
      """
    And platform facts where unit "rusty-photon-sentinel" is installed and active
    When I run doctor with --json
    Then the report has no checks named "service.devices"
    And the report has no checks named "service.doctor-probe"

  Scenario: the TLS renewal job unit is not a service and is never probed
    Given platform facts where unit "rusty-photon-renew" is installed and active
    When I run doctor with --json
    Then the report has no checks named "service.devices"
    And the report has no checks named "service.doctor-probe"

  Scenario: a TLS service without a pki tree warns instead of probing unverified
    Given a config file "ppba-driver.json" with a tls block but no pki tree
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "warn" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "trust root"

  Scenario: a rejected observatory credential proves liveness only
    Given a staged observatory credential the endpoint does not accept
    And a stub management endpoint that requires authentication
    And a config file "ppba-driver.json" pointing at the stub endpoint with auth enabled
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "warn" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "credential was rejected"

  Scenario: a management API answering a server error is a failure
    Given a stub management endpoint answering HTTP 500
    And a config file "ppba-driver.json" pointing at the stub endpoint
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "fail" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "500"

  Scenario: a management API answering garbage is a failure
    Given a stub management endpoint whose payload is not management JSON
    And a config file "ppba-driver.json" pointing at the stub endpoint
    And platform facts where unit "rusty-photon-ppba-driver" is installed and active
    When I run doctor with --json
    Then the report contains a "fail" check named "service.devices" for service "ppba-driver"
    And that check's detail mentions "did not parse"

  Scenario: a recorded binary that no longer exists is a probe warning
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts where unit "rusty-photon-ppba-driver" is installed but stopped, with a binary that does not exist
    When I run doctor with --json
    Then the report contains a "warn" check named "service.doctor-probe" for service "ppba-driver"
    And that check's detail mentions "could not run"

  Scenario: a per-service report with no checks is a probe warning, not silent coverage
    Given a stub per-service doctor for "ppba-driver" whose report has no checks
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And platform facts where unit "rusty-photon-ppba-driver" is installed but stopped, with the stub binary
    When I run doctor with --json
    Then the report contains a "warn" check named "service.doctor-probe" for service "ppba-driver"
    And that check's detail mentions "no checks"

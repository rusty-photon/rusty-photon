Feature: The flip orchestrator — doctor tls flip-to-acme
  Flipping an already-provisioned self-signed install to publicly-trusted
  ACME certificates is one command: flip-to-acme issues the wildcard pair
  (or accepts an existing one), stages the whole convergence op plan in
  memory — repointed server.tls blocks, removed CA pins, rewritten client
  URLs, probe domain and advertised URL — writes the changed configs only
  once planning succeeded, and verifies the derived public names resolve,
  reporting the exact hosts line when they do not. Preconditions each
  refuse with a named reason before anything is written: every installed
  service needs a config file to flip, a staging install never converges
  without the explicit --allow-staging override, a --domain contradicting
  acme.json refuses rather than guessing, and a missing wildcard pair
  under an existing acme.json is renewal's recovery territory. --dry-run
  prints the full cross-round op plan, carries it in the report's plan
  field, and writes nothing.

  Scenario: A dry run prints the cross-round op plan and writes nothing
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://127.0.0.1:11115" } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme with --dry-run
    Then doctor exits with code 1
    And the text output contains "rp.json: set /server/tls/cert"
    And the text output contains "ui-htmx.json: set /rp/base_url"
    And the text output contains "https://rp.pier1.example.com:11115"
    And the config file "rp.json" has "/server/tls/cert" pointing at the pki file "rp.pem"
    And the config file "ui-htmx.json" has the string "https://127.0.0.1:11115" at "/rp/base_url"

  Scenario: A dry run's report carries the plan without applied fixes
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme with --dry-run and --json
    Then the report records a planned op for check "tls.stale-selfsigned-pointer" on service "rp"
    And the report records no applied fixes

  Scenario: The flip converges a provisioned install onto the ACME end state
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://127.0.0.1:11115" } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme
    Then doctor exits with code 0
    And the config file "rp.json" has "/server/tls/cert" pointing at the pki file "acme-cert.pem"
    And the config file "rp.json" has "/server/tls/key" pointing at the pki file "acme-key.pem"
    And the config file "rp.json" has no value at "/ca_cert"
    And the config file "rp.json" has the string "https://rp.pier1.example.com:11115" at "/server/advertised_url"
    And the config file "ui-htmx.json" has the string "https://rp.pier1.example.com:11115" at "/rp/base_url"
    And the config file "ui-htmx.json" has no value at "/rp/ca_cert_path"

  Scenario: A second flip applies nothing
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme
    And I run doctor tls flip-to-acme with --json
    Then doctor exits with code 0
    And the report records no applied fixes

  Scenario: A staging acme.json refuses to converge without the override
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And the acme.json is amended to use the staging endpoint
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme
    Then doctor exits with code 2
    And stderr contains "staging"
    And stderr contains "--allow-staging"
    And the config file "rp.json" has "/server/tls/cert" pointing at the pki file "rp.pem"

  Scenario: The override converges a staging rehearsal tree
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And the acme.json is amended to use the staging endpoint
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme with --allow-staging
    Then doctor exits with code 0
    And the config file "rp.json" has "/server/tls/cert" pointing at the pki file "acme-cert.pem"

  Scenario: No acme.json and no issuance flags refuses naming the flags
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    When I run doctor tls flip-to-acme
    Then doctor exits with code 2
    And stderr contains "--domain"

  Scenario: An acme.json without the wildcard pair points at renewal
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    When I run doctor tls flip-to-acme
    Then doctor exits with code 2
    And stderr contains "doctor tls renew"

  Scenario: A --domain contradicting acme.json refuses rather than guessing
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme with --domain "pier2.example.com"
    Then doctor exits with code 2
    And stderr contains "pier1.example.com"
    And stderr contains "pier2.example.com"

  Scenario: An installed service without a config file refuses by name
    Given platform facts with an enabled unit "rusty-photon-qhy-focuser"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor tls flip-to-acme
    Then doctor exits with code 2
    And stderr contains "qhy-focuser"

  Scenario: The verification reports the exact hosts line for unresolved names
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And the host resolves none of the public names
    When I run doctor tls flip-to-acme with --json
    Then doctor exits with code 1
    And the report contains a "fail" check named "dns.unresolvable"
    And that check's suggestion mentions "127.0.0.1 rp.pier1.example.com"

  Scenario: Resolving public names complete a clean flip
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And the host resolves the public name "rp.pier1.example.com"
    When I run doctor tls flip-to-acme with --json
    Then doctor exits with code 0
    And the report contains an "ok" check named "dns.unresolvable"

  Scenario: A dry run before issuance announces the order and writes nothing
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    When I run doctor tls flip-to-acme with --dry-run and all required issuance flags
    Then doctor exits with code 0
    And the text output contains "would run"
    And the config root does not contain "acme.json"

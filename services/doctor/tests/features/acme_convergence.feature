Feature: ACME convergence
  Once acme.json exists the install's declared state is ACME: every
  service serves the shared wildcard certificate pair, clients trust
  the platform roots instead of a pinned CA, sentinel probes and rp
  advertises public <service>.<domain> names, and those names resolve
  on the box. Doctor grades what still diverges from that state, and
  --fix converges everything that is provably doctor's own material,
  so a self-signed install flips with tls issue --acme, one --fix run,
  and the reported hosts entries. A staging acme.json downgrades the
  whole family to suggestion-only warnings: doctor never converges a
  fleet onto a publicly-untrusted certificate. Hand-placed material is
  reported with the derivable value and left for the operator.

  Scenario: A doctor-issued server pointer is repointed at the wildcard pair
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor with --fix and --json
    Then the report records an applied fix for check "tls.stale-selfsigned-pointer" on service "ppba-driver"
    And the config file "ppba-driver.json" has "/server/tls/cert" pointing at the pki file "acme-cert.pem"
    And the config file "ppba-driver.json" has "/server/tls/key" pointing at the pki file "acme-key.pem"

  Scenario: A read-only run names the stale pointer without writing
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor with --json
    Then the report contains a "fail" check named "tls.stale-selfsigned-pointer" for service "ppba-driver"
    And that check's detail mentions "self-signed"
    And that check's suggestion mentions "doctor --fix"
    And the config file "ppba-driver.json" has "/server/tls/cert" pointing at the pki file "ppba-driver.pem"

  Scenario: A staging acme.json reports the stale pointer but withholds the rewrite
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And the acme.json is amended to use the staging endpoint
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor with --fix and --json
    Then the report contains a "warn" check named "tls.stale-selfsigned-pointer" for service "ppba-driver"
    And that check's detail mentions "staging"
    And the config file "ppba-driver.json" has "/server/tls/cert" pointing at the pki file "ppba-driver.pem"

  Scenario: Hand-placed server material is reported but never rewritten
    Given an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112, "tls": { "cert": "/operator/custom.pem", "key": "/operator/custom-key.pem" } } }
      """
    When I run doctor with --fix and --json
    Then the report contains a "warn" check named "tls.stale-selfsigned-pointer" for service "ppba-driver"
    And that check's detail mentions "operator intent"
    And the config file "ppba-driver.json" has the string "/operator/custom.pem" at "/server/tls/cert"

  Scenario: A stale CA pin is removed once the fleet flips
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    When I run doctor with --fix and --json
    Then the report records an applied fix for check "tls.stale-ca-pin" on service "rp"
    And the config file "rp.json" has no value at "/ca_cert"

  Scenario: A foreign CA pin is reported but left for the operator
    Given an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 }, "ca_cert": "/etc/ssl/corp-root.pem" }
      """
    When I run doctor with --fix and --json
    Then the report contains a "fail" check named "tls.stale-ca-pin" for service "sentinel"
    And that check's suggestion mentions "private CA"
    And the config file "sentinel.json" has the string "/etc/ssl/corp-root.pem" at "/ca_cert"

  Scenario: The probe domain is written from acme.json
    Given an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 } }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has the string "pier1.example.com" at "/probe_domain"

  Scenario: A hand-set probe domain is operator intent and stays
    Given an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 }, "probe_domain": "rig.example.net" }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has the string "rig.example.net" at "/probe_domain"
    And the report has no checks named "sentinel.probe-domain"

  Scenario: rp advertises its public name once the install flips
    Given an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    When I run doctor with --fix and --json
    Then the config file "rp.json" has the string "https://rp.pier1.example.com:11115" at "/server/advertised_url"

  Scenario: The staging endpoint downgrades the derivable writes to suggestions
    Given an acme.json for the domain "pier1.example.com"
    And the acme.json is amended to use the staging endpoint
    And an ACME wildcard certificate pair expiring in 60 days
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has no value at "/probe_domain"
    And the config file "rp.json" has no value at "/server/advertised_url"
    And the report contains a "warn" check named "sentinel.probe-domain" for service "sentinel"
    And that check's detail mentions "staging"

  Scenario: An unresolvable public name fails with the exact hosts line
    Given an acme.json for the domain "pier1.example.com"
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And the host resolves the public name "rp.pier1.example.com"
    When I run doctor with --json
    Then the report contains a "fail" check named "dns.unresolvable"
    And that check's detail mentions "ppba-driver.pier1.example.com"
    And that check's suggestion mentions "127.0.0.1 ppba-driver.pier1.example.com"

  Scenario: Every unresolvable name lands on one hosts line
    Given an acme.json for the domain "pier1.example.com"
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And the host resolves none of the public names
    When I run doctor with --json
    Then the report contains a "fail" check named "dns.unresolvable"
    And that check's suggestion mentions "127.0.0.1 ppba-driver.pier1.example.com rp.pier1.example.com"

  Scenario: All public names resolving is reported healthy
    Given an acme.json for the domain "pier1.example.com"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And the host resolves the public name "rp.pier1.example.com"
    When I run doctor with --json
    Then the report contains an "ok" check named "dns.unresolvable"

  Scenario: One --fix run converges a provisioned install onto the flip end state
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
    When I run doctor with --fix and --json
    Then the config file "rp.json" has "/server/tls/cert" pointing at the pki file "acme-cert.pem"
    And the config file "rp.json" has no value at "/ca_cert"
    And the config file "rp.json" has the string "https://rp.pier1.example.com:11115" at "/server/advertised_url"
    And the config file "ui-htmx.json" has the string "https://rp.pier1.example.com:11115" at "/rp/base_url"
    And the config file "ui-htmx.json" has no value at "/rp/ca_cert_path"

  Scenario: A second --fix after the flip applies nothing
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And doctor has already run with --fix
    And an acme.json for the domain "pier1.example.com"
    And an ACME wildcard certificate pair expiring in 60 days
    And doctor has already run with --fix
    When I run doctor with --fix and --json
    Then the report records no applied fixes

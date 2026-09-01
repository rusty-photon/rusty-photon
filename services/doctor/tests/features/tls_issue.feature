Feature: Certificate issuance via doctor tls issue
  doctor tls issue creates the self-signed CA (if absent) and a
  certificate pair for each installed service that lacks one, under the
  config root's pki directory. The service set is derived from the
  catalog and what is installed — not from a hand-typed default list —
  so services the retired rp_tls DEFAULT_SERVICES list missed (dsd-fp2
  among them) are covered. Configs are never touched; that is the --fix
  provisioning pass. A missing explicit --config-dir is created rather
  than rejected — issuance materializes a tree from nothing, and the
  recommended ACME staging rehearsal targets a scratch directory —
  while every other doctor entry point keeps rejecting it, so a typo'd
  path never silently diagnoses an empty directory.

  Scenario: tls issue creates a missing --config-dir
    When I run doctor tls issue pointed at a config directory that does not exist
    Then doctor exits with code 0
    And that config directory was created with a pki tree

  Scenario: tls issue refuses a --config-dir that is not a directory
    When I run doctor tls issue pointed at a config path that is a file
    Then doctor exits with code 2
    And stderr contains "is not a directory"

  Scenario: tls issue generates the CA and certificates for every installed service
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "dsd-fp2.json" containing:
      """
      { "server": { "port": 11119 } }
      """
    When I run doctor tls issue
    Then the pki file "ca.pem" exists
    And the pki file "ca-key.pem" exists
    And the pki file "ppba-driver.pem" exists
    And the pki file "dsd-fp2.pem" exists
    And the config file "ppba-driver.json" is unchanged from what was staged
    And the config file "dsd-fp2.json" is unchanged from what was staged

  Scenario: tls issue preserves the existing CA on re-run
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor tls issue has already run
    When I run doctor tls issue
    Then the pki file "ca.pem" is unchanged
    And the pki file "ppba-driver.pem" is unchanged

  Scenario: The --services flag limits certificate generation
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "dsd-fp2.json" containing:
      """
      { "server": { "port": 11119 } }
      """
    When I run doctor tls issue limited to the service "ppba-driver"
    Then the pki file "ppba-driver.pem" exists
    And the pki file "dsd-fp2.pem" does not exist

  Scenario: tls issue reports the issued material as JSON
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    When I run doctor tls issue with --json
    Then the report records an applied "generate-ca" provisioning action
    And the report records an applied "generate-cert" provisioning action for service "ppba-driver"

  Scenario: The --force flag re-issues service certificates but never the CA
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor tls issue has already run
    When I run doctor tls issue with --force
    Then the pki file "ca.pem" is unchanged
    And the pki file "ppba-driver.pem" has changed

  Scenario: Generated certificates are valid for TLS
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor tls issue has already run
    When a test HTTPS server is started with the "ppba-driver" certificate
    And a client connects using the generated CA certificate
    Then the HTTPS connection succeeds

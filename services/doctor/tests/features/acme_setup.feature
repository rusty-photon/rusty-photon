Feature: ACME certificate setup via doctor tls issue --acme
  doctor tls issue --acme requests certificates from Let's Encrypt via
  DNS-01 challenge validation. ACME mode requires --domain,
  --dns-provider, --email, and exactly one of --dns-token or
  --dns-token-var; account state persists to acme.json beside the
  service configs (the config root, not the pki directory), so a later
  renewal run can pick it up without re-passing every flag. The token
  flag chosen decides what acme.json stores: --dns-token-var NAME
  stores the indirection "$NAME" so the secret itself never reaches
  disk, while --dns-token stores its value verbatim — and a verbatim
  value that does not start with "$" draws a stderr warning, because it
  is usually a shell-expanded secret and it disconnects renew.env
  token rotation. With the Cloudflare provider the domain must sit at
  least one label below its zone apex (rig.example.com in the
  example.com zone, giving <service>.rig.example.com names): the
  enclosing zone is found by walking parent labels, and the apex itself
  is rejected so the wildcard never covers sibling hostnames in the
  zone.

  Scenario: tls issue --acme fails without --domain
    When I run doctor tls issue with --acme but no --domain
    Then the command exits with a non-zero status
    And stderr contains "domain"

  Scenario: ACME-only flags are rejected without --acme
    When I run doctor tls issue with --domain but no --acme
    Then the command exits with a non-zero status
    And stderr contains "acme"

  Scenario: tls issue --acme fails without --dns-provider
    When I run doctor tls issue with --acme and --domain but no --dns-provider
    Then the command exits with a non-zero status
    And stderr contains "dns-provider"

  Scenario: tls issue --acme fails without --email
    When I run doctor tls issue with --acme and --domain and --dns-provider but no --email
    Then the command exits with a non-zero status
    And stderr contains "email"

  Scenario: tls issue --acme without a token flag names both options
    When I run doctor tls issue with --acme and --domain and --dns-provider and --email but no token flag
    Then the command exits with a non-zero status
    And stderr contains "--dns-token"
    And stderr contains "--dns-token-var"

  Scenario: A literal --dns-token warns that the secret persists into acme.json
    When I run doctor tls issue with --acme and all required flags pointing to staging
    Then stderr contains "persisted verbatim into acme.json"
    And stderr contains "--dns-token-var"

  Scenario: The token variable name is persisted, never a token
    When I run doctor tls issue with --acme and --dns-token-var "DOCTOR_BDD_UNSET_TOKEN"
    Then the command exits with a non-zero status
    And stderr contains "DOCTOR_BDD_UNSET_TOKEN"
    And "acme.json" stores the api_token "$DOCTOR_BDD_UNSET_TOKEN"
    And stderr does not contain "persisted verbatim"

  Scenario: A dollar-form --dns-token is stored as indirection without a warning
    When I run doctor tls issue with --acme and --dns-token "$DOCTOR_BDD_UNSET_TOKEN"
    Then the command exits with a non-zero status
    And "acme.json" stores the api_token "$DOCTOR_BDD_UNSET_TOKEN"
    And stderr does not contain "persisted verbatim"

  Scenario: The two token flags conflict
    When I run doctor tls issue with --acme and both --dns-token and --dns-token-var
    Then the command exits with a non-zero status
    And stderr contains "cannot be used with"

  Scenario: tls issue --acme saves the ACME configuration beside the configs
    When I run doctor tls issue with --acme and all required flags pointing to staging
    Then the config root contains "acme.json"
    And "acme.json" contains the provided domain
    And "acme.json" contains the provided email
    And "acme.json" contains the DNS provider name
    And "acme.json" has staging set to true

  Scenario: tls issue without --acme still generates a self-signed CA
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    When I run doctor tls issue
    Then the pki file "ca.pem" exists
    And the pki file "ppba-driver.pem" exists

@serial
Feature: session-runner as an authenticated MCP client of rp
  session-runner reaches a TLS- and auth-enabled rp by presenting the
  observatory credential over verified HTTPS — the service_auth and
  ca_cert config fields, built through the shared rp-mcp-client crate
  (ADR-017). A credential is never sent over a connection the client
  cannot verify: service_auth without ca_cert connects unauthenticated
  with a loud warning.

  Scenario: standalone validation reaches a TLS and auth enabled rp
    Given generated TLS certificates
    And rp is started with TLS and auth enabled
    And session-runner is configured with service_auth and ca_cert for rp
    When the shipped "sky_flat" document is validated standalone
    Then the validation response reports catalog_validation "checked"

  Scenario: a credential without a CA is not sent to rp
    Given generated TLS certificates
    And rp is started without TLS but with auth enabled
    And session-runner is configured with service_auth for rp but no ca_cert
    When the shipped "sky_flat" document is validated standalone
    Then the validation response reports the catalog check skipped as unreachable

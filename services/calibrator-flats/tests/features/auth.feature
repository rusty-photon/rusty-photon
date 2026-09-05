@serial
Feature: TLS and HTTP Basic Auth smoke
  With `server.tls` and `server.auth` configured the service serves HTTPS and
  requires HTTP Basic Auth on `/health` and on its MCP endpoint `/mcp` alike —
  the same server block guards both, so rp's registration `auth` is the one
  observatory credential. Absent both blocks it serves plain unauthenticated
  HTTP. The deep TLS/auth behavior suites for the shared server stack live in
  ppba-driver (Alpaca drivers) and ui-htmx (BFF); these smoke scenarios prove
  the service threads the shared server config into its own serve path.

  Scenario: TLS with auth rejects missing credentials with 401 and accepts valid ones
    Given generated TLS certificates for the service
    And the service is configured with TLS and auth enabled
    When the service is started with TLS and auth
    Then the service rejects requests without credentials with 401
    And the service responds 200 to requests with valid credentials

  # No rp is running here: the provider answers tools/list on its own,
  # which is what lets rp dial it at startup without a cycle.
  Scenario: The MCP endpoint requires the service's Basic Auth and lists the flats tools
    Given generated TLS certificates for the service
    And the service is configured with TLS and auth enabled
    When the service is started with TLS and auth
    Then the MCP endpoint rejects an unauthenticated client
    And the MCP endpoint lists "train_flats", "take_flats" and "get_flat_training" for the authenticated client

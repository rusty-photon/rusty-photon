@serial
Feature: TLS and HTTP Basic Auth smoke
  With `server.tls` and `server.auth` configured the service serves HTTPS and
  requires HTTP Basic Auth. Absent both blocks it serves plain unauthenticated
  HTTP. The deep TLS/auth behavior suites for the shared server stack live in
  ppba-driver (Alpaca drivers) and ui-htmx (BFF); this smoke scenario proves
  the service threads the shared server config into its own serve path.

  Scenario: TLS with auth rejects missing credentials with 401 and accepts valid ones
    Given generated TLS certificates for the service
    And the service is configured with TLS and auth enabled
    When the service is started with TLS and auth
    Then the service rejects requests without credentials with 401
    And the service responds 200 to requests with valid credentials

  Scenario: Without credentials the restart routes answer 401, and no restart starts
    Given generated TLS certificates for the service
    And the service is configured with TLS and auth enabled
    When the service is started with TLS and auth
    Then the service refuses a restart request without credentials with 401
    And the service refuses an instance request without credentials with 401
    And the service's log shows no restart request once it is stopped

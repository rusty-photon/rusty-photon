@serial
Feature: pa-scops-oag HTTP Basic Auth
  pa-scops-oag can require authentication when auth is configured.

  Scenario: auth enabled with correct credentials returns 200
    Given generated TLS certificates for pa-scops-oag
    And pa-scops-oag is configured with TLS and auth enabled and mock serial
    When pa-scops-oag is started with TLS and auth
    Then the Alpaca management endpoint should respond with valid credentials

  Scenario: auth enabled with wrong credentials returns 401
    Given generated TLS certificates for pa-scops-oag
    And pa-scops-oag is configured with TLS and auth enabled and mock serial
    When pa-scops-oag is started with TLS and auth
    Then the Alpaca management endpoint should reject wrong credentials with 401

  Scenario: auth enabled with missing credentials returns 401
    Given generated TLS certificates for pa-scops-oag
    And pa-scops-oag is configured with TLS and auth enabled and mock serial
    When pa-scops-oag is started with TLS and auth
    Then the Alpaca management endpoint should reject missing credentials with 401

  Scenario: 401 response includes WWW-Authenticate header
    Given generated TLS certificates for pa-scops-oag
    And pa-scops-oag is configured with TLS and auth enabled and mock serial
    When pa-scops-oag is started with TLS and auth
    Then the 401 response should include a WWW-Authenticate header

  Scenario: auth disabled requires no credentials
    Given pa-scops-oag is configured without auth and with mock serial
    When pa-scops-oag is started without auth
    Then the Alpaca management endpoint should respond without credentials

  Scenario: A restart request without credentials is refused with 401
    Given generated TLS certificates for pa-scops-oag
    And pa-scops-oag is configured with TLS and auth enabled and mock serial
    When pa-scops-oag is started with TLS and auth
    Then the service refuses a restart request without credentials with 401

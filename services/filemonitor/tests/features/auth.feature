@serial
Feature: filemonitor HTTP Basic Auth
  filemonitor can require authentication when auth is configured.

  Scenario: auth enabled with correct credentials returns 200
    Given generated TLS certificates for filemonitor
    And a monitored file containing "SAFE"
    And filemonitor is configured with TLS and auth enabled and a contains rule "SAFE" as safe
    When filemonitor is started with TLS and auth
    Then the Alpaca management endpoint should respond with valid credentials

  Scenario: auth enabled with wrong credentials returns 401
    Given generated TLS certificates for filemonitor
    And a monitored file containing "SAFE"
    And filemonitor is configured with TLS and auth enabled and a contains rule "SAFE" as safe
    When filemonitor is started with TLS and auth
    Then the Alpaca management endpoint should reject wrong credentials with 401

  Scenario: auth enabled with missing credentials returns 401
    Given generated TLS certificates for filemonitor
    And a monitored file containing "SAFE"
    And filemonitor is configured with TLS and auth enabled and a contains rule "SAFE" as safe
    When filemonitor is started with TLS and auth
    Then the Alpaca management endpoint should reject missing credentials with 401

  Scenario: 401 response includes WWW-Authenticate header
    Given generated TLS certificates for filemonitor
    And a monitored file containing "SAFE"
    And filemonitor is configured with TLS and auth enabled and a contains rule "SAFE" as safe
    When filemonitor is started with TLS and auth
    Then the 401 response should include a WWW-Authenticate header

  Scenario: auth disabled requires no credentials
    Given a monitored file containing "SAFE"
    And filemonitor is configured without auth and a contains rule "SAFE" as safe
    When filemonitor is started without auth
    Then the Alpaca management endpoint should respond without credentials

  Scenario: Without valid credentials the restart routes answer 401, and no restart starts
    Given generated TLS certificates for filemonitor
    And a monitored file containing "SAFE"
    And filemonitor is configured with TLS and auth enabled and a contains rule "SAFE" as safe
    When filemonitor is started with TLS and auth
    Then the service refuses a restart request without valid credentials with 401
    And the service refuses an instance request without credentials with 401
    And the service's log shows no restart request once it is stopped

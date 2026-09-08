Feature: upbv2-driver TLS support
  upbv2-driver can serve over HTTPS when TLS is configured.

  Scenario: upbv2-driver starts with TLS and accepts HTTPS requests
    Given generated TLS certificates for upbv2-driver
    And upbv2-driver is configured with TLS enabled and mock serial
    When upbv2-driver is started with TLS
    Then the Alpaca management endpoint should respond over HTTPS

@serial
Feature: rp MCP endpoint TLS and authentication
  The /mcp endpoint sits behind the same server-wide TLS and HTTP Basic
  authentication as every REST route — there is no unauthenticated MCP
  carve-out. First-party clients present the observatory credential over
  verified HTTPS.

  Scenario: the MCP tool catalog is served over TLS to a credentialed client
    Given generated TLS certificates
    And rp is configured with TLS and auth enabled
    When rp is started with auth
    And an MCP client connects over TLS with valid credentials
    Then the MCP tool catalog should include "capture"

  Scenario: an MCP tool call succeeds over TLS with valid credentials
    Given generated TLS certificates
    And rp is configured with TLS and auth enabled
    And a camera on the simulator is configured for the next rp start
    When rp is started with auth
    And an MCP client connects over TLS with valid credentials
    Then calling "get_camera_info" for camera "main-cam" over MCP should succeed

  Scenario: an MCP session cannot be established without credentials
    Given generated TLS certificates
    And rp is configured with TLS and auth enabled
    When rp is started with auth
    Then an MCP client without credentials cannot list tools

  Scenario: an MCP session cannot be established with the wrong password
    Given generated TLS certificates
    And rp is configured with TLS and auth enabled
    When rp is started with auth
    Then an MCP client with the wrong password cannot list tools

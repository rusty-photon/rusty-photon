Feature: rp MCP endpoint Host allowlist
  The MCP transport answers 403 Forbidden to a request whose Host header
  names an authority rp is not reachable as — the standard defence
  against DNS rebinding a browser onto a locally bound server. Every
  authority rp itself advertises as its MCP endpoint is on the
  allowlist, so an orchestrator dialing the URL rp handed it is served,
  not turned away. The probe is a 2026-07-28 tools/list — the transport
  is session-less (mcp_transport.feature), so one self-contained
  request is the whole conversation.

  Scenario: an MCP request addressed to the system hostname is served
    Given a temp rp config bound to all interfaces
    And rp is started with that config file
    When a 2026-07-28 MCP request is sent with the system hostname as the Host header
    Then the MCP response status should be 200

  Scenario: an MCP request addressed to loopback is served
    Given a temp rp config bound to all interfaces
    And rp is started with that config file
    When a 2026-07-28 MCP request is sent with the Host header "127.0.0.1"
    Then the MCP response status should be 200

  Scenario: an MCP request addressed to the configured advertised host is served
    Given a temp rp config bound to all interfaces advertising "https://observatory.example:11115"
    And rp is started with that config file
    When a 2026-07-28 MCP request is sent with the Host header "observatory.example:11115"
    Then the MCP response status should be 200

  Scenario: an MCP request addressed to an unknown hostname is rejected
    Given a temp rp config bound to all interfaces
    And rp is started with that config file
    When a 2026-07-28 MCP request is sent with the Host header "attacker.example"
    Then the MCP response status should be 403

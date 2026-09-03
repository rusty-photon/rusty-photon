Feature: rp MCP transport is session-less
  rp speaks MCP 2026-07-28 and serves every request statelessly (rp.md
  § MCP Server, ADR-021). No response carries an Mcp-Session-Id header
  and no request needs one: a client bootstraps with server/discover
  and then sends self-contained requests — the MCP-Protocol-Version
  header, Mcp-Method (and Mcp-Name on a tools/call), and the
  2026-07-28 _meta fields. A client on an older revision has its
  initialize answered without a session and is served statelessly from
  then on. There is no idle keep-alive between calls: a client may stay
  quiet for any length of time and its next call is served like its
  first. First-party clients pin 2026-07-28 and negotiate it through
  server/discover.

  Scenario: A 2026-07-28 tool call is answered without a session
    Given a temp rp config with no equipment
    And rp is started with that config file
    When a 2026-07-28 "tools/call" request for "get_safety_status" is sent
    Then the MCP response status should be 200
    And the MCP response should carry no Mcp-Session-Id header
    And the MCP response should be a JSON-RPC result

  Scenario: A pre-2026-07-28 client is served without a session
    Given a temp rp config with no equipment
    And rp is started with that config file
    When a "2025-03-26" initialize request is sent
    Then the MCP response status should be 200
    And the MCP response should carry no Mcp-Session-Id header
    And the MCP response should be a JSON-RPC result
    When a "2025-03-26" "tools/list" request is sent with no session header
    Then the MCP response status should be 200
    And the MCP response should carry no Mcp-Session-Id header
    And the MCP response should be a JSON-RPC result

  Scenario: The standard client negotiates 2026-07-28 through server/discover
    Given a temp rp config with no equipment
    And rp is started with that config file
    And an MCP client connected to rp
    Then the MCP client should have negotiated protocol version "2026-07-28"
    And the MCP tool catalog should include "get_safety_status"

  # rmcp's legacy sessions closed after 300 s idle; the 30 s here is a
  # pin that nothing of that kind is left, not a wait for a timer.
  Scenario: A client idle for longer than the old keep-alive still completes its next call
    Given a temp rp config with no equipment
    And rp is started with that config file
    And an MCP client connected to rp
    Then the MCP tool catalog should include "get_safety_status"
    When the MCP client stays idle for 30 seconds
    Then the MCP tool catalog should include "get_safety_status"

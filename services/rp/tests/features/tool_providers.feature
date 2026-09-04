@serial
Feature: Tool-provider aggregation
  A tool provider is a plugin running its own MCP server. rp dials every
  `type: "tool_provider"` registration at startup through the standard
  client (the observatory credential over verified TLS, ADR-017),
  discovers its tools with tools/list, and proxies them through its own
  catalog: a client of rp sees them beside the built-ins with no way to
  tell the difference. A proxied call forwards the caller's arguments and
  returns the provider's result verbatim, and takes part in the safety
  contract exactly like a built-in: a provider tool is gated by default —
  refused with the SafetyUnsafe JSON-RPC error (code -32010) while
  conditions are unsafe, and cancelled by the unsafe transition with
  "cancelled: safety" while rp sends notifications/cancelled for the
  provider's request — and a registration opts a tool out with
  "gate": {"<tool>": "none"}. The catalog is built once at startup: a tool
  name a provider shares with a built-in or with another provider fails
  startup naming both sources, and a provider that goes away keeps its
  tools in the catalog answering a tool error naming the provider until
  the reconnect supervisor re-dials it on the equipment cadence.

  Scenario: Provider tools appear in the catalog
    Given a stub tool provider offering "echo" and "slow_echo"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "echo"
    And the tool list should include "slow_echo"
    And the tool list should include "capture"

  Scenario: A provider tool call is proxied with its result
    Given a stub tool provider offering "echo" and "slow_echo"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the MCP client calls the provider tool "echo" with {"message": "hello"}
    Then the provider tool result field "message" should be "hello"
    And the tool provider should have received a call to "echo"

  # There is no precedence to guess at: a provider cannot shadow a
  # built-in, so `capture` from a provider is a startup error, not a
  # substitution.
  Scenario: A colliding tool name fails startup
    Given a stub tool provider offering "echo" and "capture"
    And an rp config registering the tool provider
    When rp attempts to start
    Then rp should fail to start

  # The provider's own log is the oracle: rp's cancellation reached the
  # provider's in-flight request, not just rp's caller.
  Scenario: A safety stop cancels an in-flight provider tool
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a stub tool provider offering "echo" and "slow_echo"
    And rp is running with the tool provider registered
    When a second MCP client starts the provider tool "slow_echo" in the background
    And the tool provider has received a call to "slow_echo"
    And the safety monitor reports unsafe
    Then the background "slow_echo" call should fail with "cancelled: safety" within 2 seconds
    And the tool provider should have seen its "slow_echo" request cancelled within 2 seconds

  Scenario: A provider outage answers its tools with an error and the catalog is unchanged
    Given a stub tool provider offering "echo" and "slow_echo"
    And an equipment reconnect interval of 500 milliseconds
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the tool provider stops
    And the MCP client calls the provider tool "echo" with {"message": "hello"}
    Then the tool call should return an error
    And the error message should contain "tool provider `stub-provider` is unreachable"
    When the MCP client lists available tools
    Then the tool list should include "echo"
    When the tool provider comes back
    Then the provider tool "echo" should answer again within 10 seconds

  Scenario: A gated provider tool answers SafetyUnsafe while unsafe
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a stub tool provider offering "echo" and "slow_echo"
    And the tool provider registration ungates "echo"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    Then the safety status should list "slow_echo" as gated
    And the safety status should not list "echo" as gated
    And each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor "weather-watcher":
      | tool      | arguments         |
      | slow_echo | {"delay_ms": 100} |
    And each of these ungated tools should answer:
      | tool | arguments            |
      | echo | {"message": "hello"} |

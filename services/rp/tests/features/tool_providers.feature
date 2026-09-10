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

  A registration may also declare focus_tools: which of the provider's
  tools are focus operations, with the argument that carries the train.
  rp brackets such a call with the focus_started / focus_complete /
  focus_failed triple its own sweeps emit — the train's terminal camera
  plus focuser, the focuser's position with its temperature read before
  the call, then the result's top-level position, hfr, best_position,
  best_hfr, confirmed, fit_r_squared, samples_used (a missing field is
  null) plus steps only when the result carries it, or the tool error.
  A call whose argument resolves to no known train is forwarded without
  the bracket; a focus_tools key naming a tool the provider does not
  offer fails startup.

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

  # --- Focus tools -----------------------------------------------------
  # The stub echoes its arguments, so the result fields the bracket
  # reads are exactly what the call sent.

  Scenario: A declared focus tool is bracketed with the focus events
    Given a running Alpaca simulator
    And a stub tool provider offering "focus_train" and "echo"
    And the tool provider registration declares "focus_train" as a focus tool taking its train from "train_id"
    And a camera and a focuser on the simulator in train "main"
    And a test webhook receiver subscribed to "focus_started" and "focus_complete"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the MCP client calls the provider tool "focus_train" with {"train_id": "main", "position": 5120, "hfr": 2.4, "best_position": 5100, "best_hfr": 2.3, "confirmed": true, "fit_r_squared": 0.98, "samples_used": 9}
    Then the provider tool result field "train_id" should be "main"
    And the test webhook receiver should receive a "focus_started" event
    And the "focus_started" event payload field "camera_id" should be "main-cam"
    And the "focus_started" event payload field "focuser_id" should be "main-focuser"
    And the "focus_started" event payload should contain a "position"
    And the "focus_started" event payload should contain a "temperature"
    And the test webhook receiver should receive a "focus_complete" event
    And the "focus_complete" event payload field "camera_id" should be "main-cam"
    And the "focus_complete" event payload field "focuser_id" should be "main-focuser"
    And the "focus_complete" event payload field "position" should be the JSON 5120
    And the "focus_complete" event payload field "hfr" should be the JSON 2.4
    And the "focus_complete" event payload field "best_position" should be the JSON 5100
    And the "focus_complete" event payload field "best_hfr" should be the JSON 2.3
    And the "focus_complete" event payload field "confirmed" should be the JSON true
    And the "focus_complete" event payload field "fit_r_squared" should be the JSON 0.98
    And the "focus_complete" event payload field "samples_used" should be the JSON 9
    And the "focus_complete" event payload should not contain a "steps"
    And the "focus_started" and "focus_complete" events share one operation_id

  Scenario: A focus result that carries steps hands them on and a field it lacks is null
    Given a running Alpaca simulator
    And a stub tool provider offering "focus_train" and "echo"
    And the tool provider registration declares "focus_train" as a focus tool taking its train from "train_id"
    And a camera and a focuser on the simulator in train "main"
    And a test webhook receiver subscribed to "focus_complete"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the MCP client calls the provider tool "focus_train" with {"train_id": "main", "position": 5120, "steps": [{"focuser_id": "main-focuser", "run_train_id": "main", "camera_id": "main-cam", "metric": "capture", "position": 5120}]}
    Then the test webhook receiver should receive a "focus_complete" event
    And the "focus_complete" event payload field "position" should be the JSON 5120
    And the "focus_complete" event payload field "hfr" should be the JSON null
    And the "focus_complete" event payload field "confirmed" should be the JSON null
    And the "focus_complete" event payload field "steps" should be the JSON [{"focuser_id": "main-focuser", "run_train_id": "main", "camera_id": "main-cam", "metric": "capture", "position": 5120}]

  Scenario: A focus tool call whose train cannot be resolved is forwarded without the bracket
    Given a running Alpaca simulator
    And a stub tool provider offering "focus_train" and "echo"
    And the tool provider registration declares "focus_train" as a focus tool taking its train from "train_id"
    And a camera and a focuser on the simulator in train "main"
    And a test webhook receiver subscribed to "focus_started" and "focus_failed"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the MCP client calls the provider tool "focus_train" with {"train_id": "nope"}
    Then the provider tool result field "train_id" should be "nope"
    And the tool provider should have received a call to "focus_train"
    And the test webhook receiver should not have received a "focus_started" event

  Scenario: A focus tool whose provider is unreachable fails with focus_failed
    Given a running Alpaca simulator
    And a stub tool provider offering "focus_train" and "echo"
    And the tool provider registration declares "focus_train" as a focus tool taking its train from "train_id"
    And a camera and a focuser on the simulator in train "main"
    And a test webhook receiver subscribed to "focus_started" and "focus_failed"
    And rp is running with the tool provider registered
    And an MCP client connected to rp
    When the tool provider stops
    And the MCP client calls the provider tool "focus_train" with {"train_id": "main"}
    Then the tool call should return an error
    And the error message should contain "tool provider `stub-provider` is unreachable"
    And the test webhook receiver should receive a "focus_started" event
    And the test webhook receiver should receive a "focus_failed" event
    And the "focus_failed" event payload field "error" should contain "tool provider `stub-provider` is unreachable"

  Scenario: A focus_tools key naming a tool the provider does not offer fails startup
    Given a stub tool provider offering "echo" and "slow_echo"
    And the tool provider registration declares "focus_train" as a focus tool taking its train from "train_id"
    And an rp config registering the tool provider
    When rp attempts to start
    Then rp should fail to start

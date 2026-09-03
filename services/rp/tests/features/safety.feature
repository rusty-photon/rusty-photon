@serial
Feature: Safety enforcement
  rp owns safety. Configured ASCOM SafetyMonitor devices are polled at
  safety.poll_interval, and conditions are safe only while every monitor
  reports safe; a monitor that cannot be read counts as unsafe. An
  unsafe transition interrupts the active session: every in-flight
  gated tool call — one that moves the mount towards the sky or
  exposes the optics — is cancelled and answers the tool error
  "cancelled: safety", an in-flight capture is cancelled the same way
  (the transition aborts its exposure), every other ungated call
  already in flight (a park, a filter move) runs to completion, and the
  session waits in "interrupted". While conditions stay unsafe the
  safety gate sits in the tool dispatch: a gated tool is refused with
  the SafetyUnsafe JSON-RPC error — code -32010, data.reason "safety",
  data.monitor naming the unsafe monitor — and never dispatched, while
  every ungated tool (a read, park, close_cover, a capture, a filter or
  focuser move, a panel lamp) keeps answering. Nothing at
  the HTTP layer is gated: the MCP transport keeps serving every
  request (it is session-less — mcp_transport.feature) and the REST
  surface answers. The built-in classes are a default: safety.gate in
  the config moves any tool across the line, and a name that is not in
  the catalog fails startup. get_safety_status reports the state the
  gate acts on (overall, per-monitor readings, the effective gated
  list). The unsafe transition also stops the
  hardware, best-effort: in-progress exposures are aborted, guiding is
  stopped through the configured guider service (emitting
  "guide_stopped" with reason "safety"), and the mount is parked. The
  safe transition opens the gate and re-invokes the orchestrator with
  recovery context — the same workflow and session ids, recovery
  reason "safety_interruption" — returning the session to "active".
  Each monitor transition emits a "safety_changed" event. A client
  that disconnects mid-call cancels its own call whatever its class,
  and a cancelled capture aborts its exposure.

  Scenario: An unsafe transition interrupts the session and the safe transition re-invokes the orchestrator
    Given a running Alpaca simulator
    And a test orchestrator that waits for a stop signal
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to "safety_changed"
    And rp is running with equipment and both plugins configured
    When a session is started via the REST API
    And the safety monitor reports unsafe
    Then the test webhook receiver should receive a "safety_changed" event
    And the "safety_changed" event payload field "new_state" should be "unsafe"
    And the session status should become "interrupted"
    When the safety monitor reports safe again
    Then the session status should become "active"
    And the test orchestrator should have been re-invoked with recovery reason "safety_interruption"
    And the recovery invocation should carry the original workflow and session ids

  Scenario: An unsafe transition stops guiding and parks the mount
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a stub guider returning canned guiding stats
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the operator unparks the mount
    And the safety monitor reports unsafe
    Then the stub guider should have received a stop request within 5 seconds
    And the mount should report parked on the simulator within 10 seconds
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "safety"

  # The refusal is a JSON-RPC error, not a tool error: the standard
  # client surfaces it as McpCallError::SafetyStopped, which the harness
  # renders naming the code and the monitor asserted here. The mount and
  # the cover are the gated set's two shapes: mount motion and exposed
  # optics.
  Scenario: Gated tools answer SafetyUnsafe while conditions are unsafe
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And rp is running with a mount and a cover calibrator on the simulator
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    Then each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor "weather-watcher":
      | tool         | arguments                       |
      | slew         | {"ra": 10.0, "dec": 40.0}       |
      | unpark       | {}                              |
      | set_tracking | {"enabled": true}               |
      | open_cover   | {"calibrator_id": "flat-panel"} |
    When the safety monitor reports safe again
    And the safety status reports overall "safe" within 5 seconds
    Then each of these ungated tools should answer:
      | tool   | arguments |
      | unpark | {}        |

  # One row per ungated shape in the design table (rp.md § In-Flight
  # Tool Calls): a read, the stop/secure set, and the indoor actuators.
  # The mount is parked by the transition itself before park is called.
  Scenario: Ungated tools answer while conditions are unsafe
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And rp is running with a full indoor rig on the simulator
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    Then the safety status should list monitor "weather-watcher" as "unsafe"
    And the safety status should list "slew" as gated
    And the mount should report parked on the simulator within 10 seconds
    And each of these ungated tools should answer:
      | tool               | arguments                                                              |
      | get_mount_position | {}                                                                     |
      | get_park_state     | {}                                                                     |
      | park               | {}                                                                     |
      | close_cover        | {"calibrator_id": "flat-panel"}                                        |
      | calibrator_on      | {"calibrator_id": "flat-panel", "brightness": 1}                       |
      | set_filter         | {"filter_wheel_id": "main-fw", "filter_name": "Red"}                   |
      | move_focuser       | {"focuser_id": "main-focuser", "position": 5000}                       |
      | capture            | {"camera_id": "main-cam", "duration": "100ms"}                         |

  Scenario: A config override moves a tool across the gate
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a safety gate override gating "auto_focus" and ungating "open_cover"
    And rp is running with a full indoor rig on the simulator
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    Then the safety status should list "auto_focus" as gated
    And the safety status should not list "open_cover" as gated
    And each of these ungated tools should answer:
      | tool       | arguments                       |
      | open_cover | {"calibrator_id": "flat-panel"} |
    And each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor "weather-watcher":
      | tool       | arguments                                                                                             |
      | auto_focus | {"camera_id": "main-cam", "focuser_id": "main-focuser", "duration": "100ms", "step_size": 100, "half_width": 2} |

  Scenario: An override naming an unknown tool fails startup
    Given an rp config with a safety gate override gating "no_such_tool"
    When rp attempts to start
    Then rp should fail to start

  # OmniSim slews at real-mount speed, so a sync a few degrees off the
  # target turns the slew into a multi-second motion the transition can
  # land in the middle of. The 2 s budget is the contract: the poll
  # interval (250 ms here) plus one 100 ms tick plus the abort.
  Scenario: An unsafe transition cancels an in-flight slew
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to the events "slew_started, slew_failed"
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the MCP client calls "sync_mount" with ra "10.6847" dec "31.2689"
    And a second MCP client starts a slew to ra "10.6847" dec "41.2689" in the background
    And the test webhook receiver has received a "slew_started" event
    And the safety monitor reports unsafe
    Then the background "slew" call should fail with "cancelled: safety" within 2 seconds
    And the "slew_failed" event payload field "error" should be "cancelled: safety"

  Scenario: An in-flight park completes through an unsafe transition
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to the events "park_started, park_complete"
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the MCP client calls "sync_mount" with ra "10.6847" dec "38.2689"
    And the MCP client calls "slew" with ra "10.6847" dec "41.2689"
    And a second MCP client starts a park in the background
    And the test webhook receiver has received a "park_started" event
    And the safety monitor reports unsafe
    Then the background "park" call should succeed
    And the test webhook receiver should receive a "park_complete" event
    And the mount should report parked on the simulator within 10 seconds

  Scenario: A client that disconnects mid-capture has its exposure aborted
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to the events "exposure_started, exposure_failed"
    And rp is running with a camera on the simulator
    When a second MCP client starts a "10s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the second MCP client disconnects
    Then the test webhook receiver should receive an "exposure_failed" event
    And the "exposure_failed" event payload field "error" should be "cancelled: client disconnected"
    And the simulator camera should report idle within 2 seconds

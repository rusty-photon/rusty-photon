@serial
Feature: Safety enforcement
  rp owns safety. Configured ASCOM SafetyMonitor devices are polled at
  safety.poll_interval, and conditions are safe only while every monitor
  reports safe; a monitor that cannot be read counts as unsafe. On an
  unsafe transition every in-flight gated tool call — one that moves
  the mount towards the sky or exposes the optics — is cancelled and
  answers the tool error "cancelled: safety", and every ungated call
  already in flight (a park, a filter move) runs to completion. While
  conditions stay unsafe the safety gate sits in the tool dispatch: a
  gated tool is refused with the SafetyUnsafe JSON-RPC error — code
  -32010, data.reason "safety", data.monitor naming the unsafe monitor
  — and never dispatched, while every ungated tool (a read, park,
  close_cover, a filter or focuser move, a panel lamp) keeps answering.
  capture is classed call by call from its own arguments: a Dark, a
  Bias or a capture whose light_source is "panel" is ungated, taken
  while unsafe and run on through the transition, its exposure left
  alone; every other — a Light, a sky flat, a capture that names no
  light source — is gated, refused with the same SafetyUnsafe refusal
  so an orchestrator pauses at its next sky frame wherever the
  transition lands, and cancelled in flight. Nothing at
  the HTTP layer is gated: the MCP transport keeps serving every
  request (it is session-less — mcp_transport.feature) and the REST
  surface answers. The built-in classes are a default: safety.gate in
  the config moves any tool across the line, and a name that is not in
  the catalog fails startup. get_safety_status reports the state the
  gate acts on (overall, per-monitor readings, the effective gated
  list). The unsafe transition also stops the
  hardware, best-effort: in-progress exposures are aborted except a
  dark, a bias or a panel flat that rp is taking, guiding is
  stopped through the configured guider service (emitting
  "guide_stopped" with reason "safety"), and the mount is parked. The
  safe transition opens the gate and does nothing else: rp keeps no
  session and re-invokes nobody — the "safety_changed" event each
  monitor transition emits is the signal an orchestrator resumes on
  (session-runner's recovery.feature covers that end to end). A client
  that disconnects mid-call cancels its own call whatever its class,
  and a cancelled capture aborts its exposure.

  # The whole contract on one rig: a slew in flight is cancelled with
  # the safety reason, the gate refuses the next gated call, and the
  # safe transition lifts it — after which the same gated tool answers
  # again with nobody re-invoked in between. (OmniSim's park on the
  # unsafe side is what makes unpark the natural gated probe.)
  # "slew_started" is the barrier that puts the slew in flight before
  # the monitor flips: a slew that has not reached rp yet is refused at
  # the gate instead, which is correct but a different contract.
  Scenario: An unsafe transition cancels in-flight gated work and the safe transition lifts the gate
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to the events "safety_changed, slew_started"
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the MCP client calls "sync_mount" with ra "10.6847" dec "31.2689"
    And a second MCP client starts a slew to ra "10.6847" dec "41.2689" in the background
    And the test webhook receiver has received a "slew_started" event
    And the safety monitor reports unsafe
    Then the background "slew" call should fail with "cancelled: safety" within 2 seconds
    And the test webhook receiver should receive a "safety_changed" event
    And the "safety_changed" event payload field "new_state" should be "unsafe"
    And each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor "weather-watcher":
      | tool   | arguments |
      | unpark | {}        |
    When the safety monitor reports safe again
    And the safety status reports overall "safe" within 5 seconds
    Then the gated tool "unpark" should answer again

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
      | tool               | arguments                                                               |
      | get_mount_position | {}                                                                      |
      | get_park_state     | {}                                                                      |
      | park               | {}                                                                      |
      | close_cover        | {"calibrator_id": "flat-panel"}                                         |
      | calibrator_on      | {"calibrator_id": "flat-panel", "brightness": 1}                        |
      | set_filter         | {"filter_wheel_id": "main-fw", "filter_name": "Red"}                    |
      | move_focuser       | {"focuser_id": "main-focuser", "position": 5000}                        |
      | capture            | {"camera_id": "main-cam", "duration": "100ms", "light_source": "panel"} |

  # rp.md § Capture Tool Details, *Light source*. The Light row names a
  # target the store does not have: the refusal comes before rp looks
  # the target up or touches the camera. The rig keeps the flat
  # <uuid8>.fits name, so a typed frame needs no naming pattern.
  Scenario: While unsafe, rp takes darks, bias and panel-lit frames and refuses every capture that needs the sky
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And rp is running with a capture rig and no naming templates configured
    And an MCP client connected to rp
    When the safety monitor reports unsafe
    And the safety status reports overall "unsafe" within 5 seconds
    Then each of these captures should be taken:
      | arguments                                                                                    |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Dark"}                         |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Bias"}                         |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Flat", "light_source": "panel"} |
      | {"camera_id": "main-cam", "duration": "100ms", "light_source": "panel"}                      |
    And each of these captures should be refused with SafetyUnsafe code -32010 naming monitor "weather-watcher":
      | arguments                                                                                    |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Light", "target": "m31"}       |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Flat", "light_source": "sky"}  |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Flat"}                         |
      | {"camera_id": "main-cam", "duration": "100ms", "light_source": "sky"}                        |
      | {"camera_id": "main-cam", "duration": "100ms"}                                               |
    When the safety monitor reports safe again
    And the safety status reports overall "safe" within 5 seconds
    Then each of these captures should be taken:
      | arguments                                                                                    |
      | {"camera_id": "main-cam", "duration": "100ms"}                                               |
      | {"camera_id": "main-cam", "duration": "100ms", "frame_type": "Flat", "light_source": "sky"}  |

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

  # rp.md § SafetyMonitor Polling, step 2: a capture that names no
  # light source needs the sky, so it is gated for its call. The 2 s
  # budget is the slew's: the poll, one 100 ms tick and the abort.
  Scenario: An unsafe transition cancels an in-flight capture that needs the sky
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to the events "exposure_started"
    And rp is running with a camera on the simulator
    When a second MCP client starts a "10s" capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the safety monitor reports unsafe
    Then the background "capture" call should fail with "cancelled: safety" within 2 seconds
    And the simulator camera should report idle within 2 seconds

  # rp.md § SafetyMonitor Polling, step 3. The park is the barrier: the
  # transition parks only after its exposure abort, so a camera still
  # exposing once the mount reports parked is one the abort left
  # alone. The 5 s frame leaves the transition (a 250 ms poll, then a
  # park from the reset pose) a wide margin.
  Scenario Outline: An unsafe transition lets an in-flight <frame> finish and save its frame
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a test webhook receiver subscribed to the events "exposure_started"
    And rp is running with a mount and a capture rig and no naming templates configured
    And the mount is unparked
    When a second MCP client starts a "5s" <frame> capture of camera "main-cam" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the safety monitor reports unsafe
    Then the mount should report parked on the simulator within 10 seconds
    And the simulator camera should still be exposing
    And the background "capture" call should succeed
    And the background capture should have saved a frame

    Examples:
      | frame          |
      | Dark           |
      | panel-lit Flat |

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

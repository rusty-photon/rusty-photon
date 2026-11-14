@wip @serial
Feature: The guide run watch reports a run's end and a lost star
  The guider pushes no event: a run it stops by itself, or one that
  has lost its stars, shows only in its stats. So rp watches a run it
  has seen settle. From the guide_settled on it reads the guider's
  GET /api/v1/guiding/stats every 5 s and turns what it sees into
  events. guide_stopped with the reason guider is emitted once, when
  the run has ended without rp asking, and its stop_reason says how:
  the reason of the guider's last_stop when a read shows the state
  idle or looping, which is requested when another client stopped the
  run; restarted when a read shows idle or looping with no last_stop;
  unreachable when no read has answered for 60 s. guide_star_lost is
  emitted when every read over the guiding config's star_lost_after
  has shown the state star_lost, with lost_for_ms saying how long the
  loss has lasted, and it is emitted again after each further
  star_lost_after for as long as the loss lasts. A read that shows
  another state ends the loss. The watch sends the guider its reads
  and nothing else: what follows an event is the orchestrator's
  decision. It ends when it has reported the run's end. It ends too
  when the guider has answered a stop that rp asked for, which reports
  its own guide_stopped, and it goes on when that stop failed. A start
  that fails before it settles is that call's error and no event of
  the watch's.

  The reads come 5 s apart, so an event follows the stub guider's
  change by up to 5 s, and a lost star's by up to 5 s more. The
  scenario of the guider that stops answering runs for over a minute.
  A stub guider that restarts is in the state idle and has no
  last_stop.

  Scenario: A run that goes on guiding is read again and again and reported neither stopped nor star-lost
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to the events "guide_settled, guide_stopped, guide_star_lost"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the test webhook receiver should receive a "guide_settled" event
    And no "guide_stopped" event arrives within 12000 ms
    And the test webhook receiver should not have received a "guide_star_lost" event
    And the stub guider should have received at least 2 "/guiding/stats" requests

  Scenario: A run the guider stops by itself is reported once, with the guider's reason
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider stops the run on its own with reason "mount_slewing"
    Then the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "mount_slewing"
    And the stub guider should receive no "/guiding/stats" request within 12000 ms
    And exactly 1 "guide_stopped" event should have been received
    And the stub guider should have received exactly 1 "/guiding/start" request
    And the stub guider should not have received a "/guiding/stop" request

  Scenario Outline: A run another client stops and leaves <state> is reported with the stop reason requested
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And another client stops the stub guider's run and leaves it in state "<state>"
    Then the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "requested"

    Examples:
      | state   |
      | idle    |
      | looping |

  Scenario: A guider that restarts under the run is reported as restarted
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider restarts
    Then the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "restarted"

  Scenario: A guider that answers no read for 60 s is reported as unreachable, and not before
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider stops answering
    Then no "guide_stopped" event arrives within 50000 ms
    And the test webhook receiver should receive a "guide_stopped" event within 30 seconds
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "unreachable"

  Scenario: A stop rp asks for reports the run's end itself, and the watch ends with it
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the MCP client calls "stop_guiding"
    Then the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "requested"
    And the stub guider should receive no "/guiding/stats" request within 12000 ms
    And exactly 1 "guide_stopped" event should have been received

  Scenario: The watch goes on after a stop of rp's that the guider failed
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider answers "/guiding/stop" requests with error code "stop_timeout" and message "the exposure in flight did not end in time"
    And a test webhook receiver subscribed to "guide_settled" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the MCP client calls "stop_guiding"
    Then the tool call should return an error
    When the stub guider stops the run on its own with reason "camera_error"
    Then the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "guider"
    And the "guide_stopped" event payload field "stop_reason" should be "camera_error"

  Scenario: A lost star is reported once star_lost_after has passed, with how long it has lasted, and again while it lasts
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to the events "guide_settled, guide_star_lost, guide_stopped"
    And rp's guiding config sets "star_lost_after" to the JSON "2s"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider loses its guide star
    Then the test webhook receiver should receive a "guide_star_lost" event within 20 seconds
    And the "guide_star_lost" event payload field "lost_for_ms" should be at least 2000
    And the test webhook receiver should receive 2 "guide_star_lost" events within 20 seconds
    And the test webhook receiver should not have received a "guide_stopped" event
    And the stub guider should have received exactly 1 "/guiding/start" request
    And the stub guider should not have received a "/star/reselect" request

  Scenario: A loss that has lasted less than star_lost_after is not reported
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to the events "guide_settled, guide_star_lost, guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider loses its guide star
    Then no "guide_star_lost" event arrives within 12000 ms
    And the test webhook receiver should not have received a "guide_stopped" event

  Scenario: A star found again ends the loss, and nothing more is reported
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to the events "guide_settled, guide_star_lost, guide_stopped"
    And rp's guiding config sets "star_lost_after" to the JSON "2s"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    And the test webhook receiver has received a "guide_settled" event
    And the stub guider loses its guide star
    Then the test webhook receiver should receive a "guide_star_lost" event within 20 seconds
    When the stub guider finds its guide star again
    Then no "guide_stopped" event arrives within 12000 ms
    And exactly 1 "guide_star_lost" event should have been received

  Scenario: A start that fails before it settles is watched by no one
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider answers "/guiding/start" requests with error code "no_stars" and message "no star in the frame passed the selection"
    And a test webhook receiver subscribed to "guide_failed" and "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the test webhook receiver should receive a "guide_failed" event
    And the stub guider should receive no "/guiding/stats" request within 12000 ms
    And the test webhook receiver should not have received a "guide_stopped" event

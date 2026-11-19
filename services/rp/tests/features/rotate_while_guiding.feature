@serial
Feature: A rotator move of the guiding train keeps the guider's calibration right
  The guider is a client of no rotator and cannot see the field turn,
  and a calibration is right only at the angle it was made at. So when
  move_rotator turns a rotator of the guiding train, rp reads the
  guider's state and runs the rotate-while-guiding ladder around the
  move. A guider found guiding has its corrections paused first, its
  frames going on, and a pause that fails aborts the tool before the
  rotator moves. After the move the change of the sky angle decides.
  At or below the guiding config's recalibrate_above_deg, 5 degrees by
  default, the calibrations are kept, and a run the ladder paused has
  its guide stars re-selected and its corrections resumed. Above it rp
  empties the guider's calibration store, whether the guider guides or
  not: it stops a run first, since the guider refuses the clear in
  one, and a run the ladder paused is then started again with the
  guiding config's settle values. A run found paused, star-lost or
  calibrating is not paused and not brought back: above the threshold
  it is stopped for the clear and left stopped. A state read that
  fails counts as a guider in no run. A stop, a clear, a start, a
  re-selection or a resume that fails is a tool error, and so is a
  guider that does not answer after a turn above the threshold. The
  result's guiding_ladder carries delta_deg, calibration_cleared and
  guiding_restarted, and it is null when the guiding train did not
  turn. move_rotator_started says in guiding_paused whether the ladder
  pauses the corrections for this move. The ladder's stop and its
  start emit the events of stop_guiding and start_guiding.

  The simulator's rotator starts at 0 degrees, so a move to 2 degrees
  turns the field by less than the default threshold and a move to 10
  degrees by more. The guiding train holds the rotator and the
  simulator's camera, which is the stub guider's camera, so the
  ladder's start passes the check of guider_devices_check.feature. The
  stub guider refuses a clear of its calibration store in a run state
  with not_idle, as the guider does.

  Scenario: A turn below the threshold pauses the corrections, re-selects the guide stars and resumes, and keeps the calibrations
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 2.0
    Then the rotator result field "angle" should be 2.0 within 0.1
    And the stub guider should have received a pause request with full false
    And the stub guider should have received the requests "/guiding/pause, /star/reselect, /guiding/resume" in that order
    And the stub guider should not have received a "/calibration/clear" request
    And the stub guider should not have received a "/guiding/stop" request
    And the stub guider should not have received a "/guiding/start" request
    And the rotator result ladder field "delta_deg" should be 2.0 within 0.1
    And the rotator result ladder field "calibration_cleared" should be false
    And the rotator result ladder field "guiding_restarted" should be false

  Scenario: A turn above the threshold stops the run, empties the calibration store and starts guiding again with the config's settle values
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to the events "guide_stopped, guide_started, guide_settled"
    And rp's guiding config sets "settle_pixels" to the JSON 0.8
    And rp's guiding config sets "settle_time" to the JSON "8s"
    And rp's guiding config sets "settle_timeout" to the JSON "40s"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the rotator result field "angle" should be 10.0 within 0.1
    And the stub guider should have received a pause request with full false
    And the stub guider should have received the requests "/guiding/pause, /guiding/stop, /calibration/clear, /guiding/start" in that order
    And the stub guider should have received a start request with settle pixels 0.8 time "8s" timeout "40s"
    And the stub guider should not have received a "/star/reselect" request
    And the stub guider should not have received a "/guiding/resume" request
    And the rotator result ladder field "delta_deg" should be 10.0 within 0.1
    And the rotator result ladder field "calibration_cleared" should be true
    And the rotator result ladder field "guiding_restarted" should be true
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "requested"
    And the test webhook receiver should receive a "guide_started" event
    And the test webhook receiver should receive a "guide_settled" event

  Scenario: A turn above the threshold with the guider idle empties the calibration store and starts no run
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the rotator result field "angle" should be 10.0 within 0.1
    And the stub guider should have received a "/calibration/clear" request
    And the stub guider should not have received a pause request
    And the stub guider should not have received a "/guiding/stop" request
    And the stub guider should not have received a "/guiding/start" request
    And the rotator result ladder field "calibration_cleared" should be true
    And the rotator result ladder field "guiding_restarted" should be false

  Scenario: A turn below the threshold with the guider idle changes nothing at the guider
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider's camera is the simulator's camera
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 2.0
    Then the rotator result field "angle" should be 2.0 within 0.1
    And the stub guider should not have received a pause request
    And the stub guider should not have received a "/calibration/clear" request
    And the stub guider should not have received a "/star/reselect" request
    And the stub guider should not have received a "/guiding/resume" request
    And the stub guider should not have received a "/guiding/start" request
    And the rotator result ladder field "calibration_cleared" should be false
    And the rotator result ladder field "guiding_restarted" should be false

  Scenario Outline: A run found <state> is not paused, and a turn below the threshold leaves it as it is
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 2.0
    Then the rotator result field "angle" should be 2.0 within 0.1
    And the stub guider should not have received a pause request
    And the stub guider should not have received a "/star/reselect" request
    And the stub guider should not have received a "/guiding/resume" request
    And the stub guider should not have received a "/guiding/stop" request
    And the stub guider should not have received a "/calibration/clear" request
    And the rotator result ladder field "calibration_cleared" should be false
    And the rotator result ladder field "guiding_restarted" should be false

    Examples:
      | state       |
      | paused      |
      | star_lost   |
      | calibrating |

  Scenario Outline: A run found <state> is stopped for the clear above the threshold and not started again
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the rotator result field "angle" should be 10.0 within 0.1
    And the stub guider should not have received a pause request
    And the stub guider should have received the requests "/guiding/stop, /calibration/clear" in that order
    And the stub guider should not have received a "/guiding/start" request
    And the stub guider should not have received a "/guiding/resume" request
    And the rotator result ladder field "calibration_cleared" should be true
    And the rotator result ladder field "guiding_restarted" should be false
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "requested"

    Examples:
      | state       |
      | paused      |
      | star_lost   |
      | calibrating |

  Scenario: A pause the guider fails aborts the move before the rotator turns
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And the stub guider answers "/guiding/pause" requests with error code "internal" and message "the guider failed the request"
    And a test webhook receiver subscribed to "move_rotator_started" and "move_rotator_failed"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the tool call should return an error
    And the test webhook receiver should receive a "move_rotator_started" event
    And the test webhook receiver should receive a "move_rotator_failed" event
    And the stub guider should not have received a "/calibration/clear" request
    When the MCP client calls "get_rotator_position" with rotator "main-rotator"
    Then the rotator result field "angle" should be 0.0 within 0.1

  Scenario Outline: A <path> the guider fails after the turn is a tool error
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And the stub guider answers "<path>" requests with error code "internal" and message "the guider failed the request"
    And a test webhook receiver subscribed to "move_rotator_failed"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle <angle>
    Then the tool call should return an error
    And the test webhook receiver should receive a "move_rotator_failed" event

    Examples:
      | path               | angle |
      | /star/reselect     | 2.0   |
      | /guiding/resume    | 2.0   |
      | /guiding/stop      | 10.0  |
      | /calibration/clear | 10.0  |
      | /guiding/start     | 10.0  |

  Scenario: A turn of a rotator outside the guiding train reports no ladder and leaves the guider's run alone
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a rotator on the simulator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 45.0
    Then the rotator result field "angle" should be 45.0 within 0.1
    And the stub guider should not have received a pause request
    And the stub guider should not have received a "/calibration/clear" request
    And the rotator result should have no guiding ladder

  Scenario Outline: move_rotator_started says guiding_paused <paused> for a guider found <state>
    Given a running Alpaca simulator
    And a stub guider in state "<state>"
    And the stub guider's camera is the simulator's camera
    And a test webhook receiver subscribed to "move_rotator_started" and "move_rotator_complete"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 2.0
    Then the test webhook receiver should receive a "move_rotator_started" event
    And the "move_rotator_started" event payload field "guiding_paused" should be the JSON <paused>
    And the test webhook receiver should receive a "move_rotator_complete" event
    And the "move_rotator_complete" event payload should contain a "guiding_ladder"

    Examples:
      | state   | paused |
      | guiding | true   |
      | idle    | false  |
      | paused  | false  |

  Scenario: A higher recalibrate_above_deg keeps the calibrations through a wider turn
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp's guiding config sets "recalibrate_above_deg" to the JSON 20.0
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the rotator result field "angle" should be 10.0 within 0.1
    And the stub guider should have received the requests "/guiding/pause, /star/reselect, /guiding/resume" in that order
    And the stub guider should not have received a "/calibration/clear" request
    And the rotator result ladder field "calibration_cleared" should be false
    And the rotator result ladder field "guiding_restarted" should be false

  Scenario: A guider that does not answer does not stop a turn below the threshold
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp's guiding config sets "timeout" to the JSON "2s"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the stub guider stops answering
    And the MCP client calls "move_rotator" with rotator "main-rotator" to angle 2.0
    Then the rotator result field "angle" should be 2.0 within 0.1
    And the rotator result ladder field "calibration_cleared" should be false
    And the rotator result ladder field "guiding_restarted" should be false

  Scenario: A guider that does not answer after a turn above the threshold fails the tool as a failed clear
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider's camera is the simulator's camera
    And rp's guiding config sets "timeout" to the JSON "2s"
    And rp is running with a rotator and a camera on the simulator inside guiding train "guide"
    And an MCP client connected to rp
    When the stub guider stops answering
    And the MCP client calls "move_rotator" with rotator "main-rotator" to angle 10.0
    Then the tool call should return an error
    When the MCP client calls "get_rotator_position" with rotator "main-rotator"
    Then the rotator result field "angle" should be 10.0 within 0.1

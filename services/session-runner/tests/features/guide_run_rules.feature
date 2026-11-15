@wip @serial
Feature: The deep-sky document answers a lost guide star and a run the guider stopped
  rp's guide run watch reports a guided run that has lost its stars,
  guide_star_lost, and one that ended without rp asking, guide_stopped
  with the reason guider. The shipped deep_sky.json answers each with a
  rule, and both rules are off unless the guide parameter is true.

  On guide_star_lost the document reads get_guiding_stats and, while
  the state is still star_lost, calls start_guiding, which there
  selects guide stars on a fresh frame and takes a new lock where they
  are. The mount is not slewed and the target is not centred again. A
  frame with no star fails that call, the failure is logged, and the
  event comes again for as long as the loss lasts, so the document
  asks again at each one. The capture loop goes on between the
  attempts, and a lost star never fails the session.

  On a guide_stopped of the reason guider the next pass of the
  dispatch loop acquires the target again from the slew on: stop,
  slew, centre, rotate, focus, start guiding. The exposure in flight
  is not aborted. A guide_stopped of any other reason, a stop that rp
  was asked for among them, starts no re-acquisition.

  The stub guider's device report names the simulator's mount and a
  camera that is none of rp's, so rp's check of the guider's devices
  passes. rp's watch reads the guider every 5 s, and the scenarios
  shorten star_lost_after to 2 s, so guide_star_lost follows a lost
  star by up to 10 s and comes again at every read after that. Each
  centring captures one solving frame, which counts among the
  exposure_complete events.

  Scenario: A lost star gets a new lock where the mount points, and the session goes on to its last frame
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub guider in state "idle"
    And a stub plate solver echoing the first target
    And rp's guiding config sets "star_lost_after" to the JSON "2s"
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration | 2s    |
      | max_frames        | 15    |
      | focus             | false |
      | centering         | true  |
      | park_on_finish    | false |
      | guide             | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    And the deep-sky session has captured at least 1 frames
    And the stub guider loses its guide star
    Then the run ends within 180 seconds
    And the run should report "complete"
    And the SSE stream should show at least 1 "guide_star_lost" events
    # The acquisition's start and the one the rule asks for. An event
    # drained after the new lock reads the state guiding and starts
    # nothing.
    And the stub guider should have received exactly 2 "/guiding/start" requests
    And the SSE stream should show exactly 2 "guide_settled" events
    And the SSE stream should show exactly 1 "slew_complete" event
    And the SSE stream should show exactly 1 "centering_complete" event
    # 15 light frames and the one solving frame of the one centring.
    And the SSE stream should show exactly 16 "exposure_complete" events

  Scenario: A star the guider cannot find again is asked for at every event while the frames go on
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub guider in state "idle"
    And rp's guiding config sets "star_lost_after" to the JSON "2s"
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration | 2s    |
      | max_frames        | 20    |
      | focus             | false |
      | centering         | false |
      | park_on_finish    | false |
      | guide             | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    And the deep-sky session has captured at least 1 frames
    And the stub guider answers "/guiding/start" requests with error code "no_stars" and message "no star in the frame passed the selection"
    And the stub guider loses its guide star
    Then the run ends within 240 seconds
    And the run should report "complete"
    And the SSE stream should show at least 2 "guide_star_lost" events
    # The acquisition's start, then one refused start for each event.
    And the stub guider should have received at least 3 "/guiding/start" requests
    And the SSE stream should show exactly 1 "guide_settled" event
    And the SSE stream should show exactly 1 "slew_complete" event
    And the SSE stream should show exactly 20 "exposure_complete" events

  Scenario: A run the guider stops by itself is acquired again from the slew on
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub guider in state "idle"
    And a stub plate solver echoing the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration | 2s    |
      | max_frames        | 10    |
      | focus             | false |
      | centering         | true  |
      | park_on_finish    | false |
      | guide             | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    And the deep-sky session has captured at least 1 frames
    And the stub guider stops the run on its own with reason "mount_slewing"
    Then the run ends within 180 seconds
    And the run should report "complete"
    # The first acquisition and the one that follows the guider's stop.
    And the SSE stream should show exactly 2 "slew_complete" events
    And the SSE stream should show exactly 2 "centering_complete" events
    And the stub guider should have received exactly 2 "/guiding/start" requests
    And the SSE stream should show exactly 2 "guide_settled" events

  Scenario: A stop that rp was asked for starts no re-acquisition
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub guider in state "idle"
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration | 2s    |
      | max_frames        | 8     |
      | focus             | false |
      | centering         | false |
      | park_on_finish    | false |
      | guide             | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    And the deep-sky session has captured at least 1 frames
    And an operator's MCP client calls "stop_guiding" on rp
    Then the run ends within 120 seconds
    And the run should report "complete"
    And the stub guider should have received exactly 1 "/guiding/start" request
    And the SSE stream should show exactly 1 "guide_settled" event
    And the SSE stream should show exactly 1 "slew_complete" event
    And the SSE stream should show exactly 8 "exposure_complete" events

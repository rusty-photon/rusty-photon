@serial
Feature: Resume (the re-entrancy contract)
  A run interrupted mid-way continues from the persisted blackboard
  when it resumes: re-execution from the root skips once-marked setup
  and picks the capture loop up at the recorded frame count instead of
  starting over. Nobody re-invokes the engine — rp keeps no session
  registry — so every resume is the engine's own: a killed engine
  resumes its run manifest on restart, an rp outage pauses the run
  until rp is back, and a safety interruption pauses it until rp's
  safety monitors read safe again.

  The safety scenario exercises rp's own machinery end-to-end: an
  unsafe SafetyMonitor reading makes rp cancel the in-flight call and
  refuse the run's gated calls, and the run waits on rp's safety
  status. The rp-outage scenario restarts rp on the port the run was
  configured for, as a real restart would. The fixture document
  (recovery_capture_loop, tests/fixtures/workflows/) plans 4 frames of
  2s each; its progress counter lives in session.frames.

  Scenario: A killed engine resumes on restart without repeating recorded frames
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "recovery_capture_loop" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    And the blackboard records at least 2 frames
    And the session-runner is killed
    And the session-runner is restarted
    Then the run reports "running" within 30 seconds
    And the run ends within 60 seconds
    And the run should report "complete"
    And the SSE stream should show between 4 and 5 "exposure_complete" events
    And the SSE stream should show exactly 1 "filter_switch" event
    And the blackboard is deleted within 10 seconds

  Scenario: An rp outage pauses the run and it completes against the restarted rp
    Given a running Alpaca simulator
    And rp is running with a camera and session-runner running the "recovery_capture_loop" workflow
    When a run is started
    And the blackboard records at least 2 frames
    And rp is killed
    Then the run reports "paused" within 10 seconds
    And the run is paused for "rp_outage"
    And the session-runner is still healthy and the blackboard is kept
    When rp is restarted
    And an SSE client is watching rp's event stream
    Then the run reports "running" within 30 seconds
    And the blackboard is deleted within 60 seconds
    And the run should report "complete"
    And the SSE stream should show only the remaining "exposure_complete" events

  Scenario: A safety interruption pauses the run and it resumes by itself once conditions are safe
    Given a running Alpaca simulator
    And a safety monitor guards the session
    And rp is running with a camera and session-runner running the "recovery_capture_loop" workflow
    And an SSE client is watching rp's event stream
    When a run is started
    And the blackboard records at least 2 frames
    And the safety monitor reports unsafe
    Then the run reports "paused" within 5 seconds
    And the run is paused for "safety"
    And the blackboard is kept
    When the safety monitor reports safe again
    Then the run reports "running" within 5 seconds
    And the blackboard is deleted within 60 seconds
    And the run should report "complete"
    And the SSE stream should show between 4 and 5 "exposure_complete" events
    And the SSE stream should show exactly 1 "filter_switch" event
    And the SSE stream should show exactly 2 "safety_changed" events

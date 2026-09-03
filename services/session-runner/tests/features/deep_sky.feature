@serial
Feature: Deep-sky workflow document
  The shipped deep_sky.json drives a full night cycle against rp's real
  planner: unpark, start tracking and start cooling the camera
  (start_cooldown — idempotent, so a resume adopts the rung the cooler
  already holds), then a dispatch loop that asks
  get_next_target after every frame — on a target change it slews,
  optionally plate-solve-centers and auto-focuses, optionally starts
  guiding, then captures one frame per pass, records it via
  record_exposure, and dithers on the dither_every cadence — ending at
  the max_frames budget, at dawn, or when every target's integration
  goal is met, stopping guiding, optionally parking, and warming the
  camera back up in a finally (start_warmup). The document
  is train-addressed: it takes a single train_id (the imaging train)
  and rp resolves the camera, filter wheel, and focuser through the
  train, with sweep geometry coming from the train's auto_focus config
  block rather than parameters. rp's guide-focus-watch events name the
  guiding train, and the document's triggers answer them: a guide-only
  metric auto_focus on guide_focus_degraded, the full refocus_train on
  guide_focus_escalation. Because the planner evaluates real ephemeris at
  wall-clock now, every scenario computes its observing site to fit the
  clock: an equatorial site at the anti-solar longitude is always in
  deep astronomical night, and celestial-equator targets placed by hour
  angle sink at a constant 0.25 degrees per minute — which makes "this
  target drops below its floor in N seconds" exact. The dawn scenario
  flips the trick: a site 45 degrees west of the sub-solar longitude
  has a risen, still-climbing morning sun at any moment, so the planner
  answers end_of_session. The simulated mount
  is taught the same site (rp refuses a mount whose reported site
  disagrees with config) and synced onto the first target so document
  slews stay sub-degree.

  Exposure counts include every capture rp makes on the document's
  behalf: each center_on_target iteration captures one solving frame,
  and each auto_focus sweep captures one frame per grid position, on
  top of the light frames the capture loop takes.

  Scenario: A full night cycle runs start to finish and parks the mount
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub plate solver echoing the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 500ms |
      | max_frames     | 3     |
      | focus          | false |
      | centering      | true  |
      | park_on_finish | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 120 seconds
    And the blackboard is deleted within 10 seconds
    And the SSE stream should show exactly 1 "unpark_complete" event
    And the SSE stream should show exactly 1 "slew_complete" event
    And the SSE stream should show exactly 1 "centering_complete" event
    # 3 light frames + 1 centering solve frame (converged on iteration 1).
    And the SSE stream should show exactly 4 "exposure_complete" events
    And the SSE stream should show exactly 1 "park_complete" event

  Scenario: The planner's exposure plan drives the capture duration
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target whose exposure plan is a single unfiltered 2-second frame
    And the simulated mount matches the site and points at the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | max_frames     | 2     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | false |
    And an SSE client is watching rp's event stream
    When a run is started
    # `exposure_duration` is deliberately not supplied: its default is 300s, so
    # the session can only finish this fast if the planner-returned 2s
    # plan is what reaches the camera.
    Then the run ends within 90 seconds
    And the SSE stream should show exactly 2 "exposure_complete" events

  Scenario: A session ends when the target's integration goal is met
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target whose integration goal is 2 unfiltered 2-second frames
    And the simulated mount matches the site and points at the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | max_frames     | 0     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | false |
    And an SSE client is watching rp's event stream
    When a run is started
    # max_frames is 0 (no budget), so the session can only end through
    # rp's derived progress: each capture carries the pass's target and
    # frame_type: Light, so the frame lands in the target's directory,
    # and once the scan finds the 2-frame goal met the planner
    # eliminates the exhausted target and answers end_of_session.
    Then the run ends within 90 seconds
    And the SSE stream should show exactly 2 "exposure_complete" events
    And the SSE stream should show exactly 1 "slew_complete" event

  Scenario: A session started after dawn ends immediately with no frames
    Given a running Alpaca simulator
    And an observing site where the morning sun has risen and one planner target sits below its floor
    And the simulated mount matches the site and points at the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 2s    |
      | max_frames     | 2     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | false |
    And an SSE client is watching rp's event stream
    When a run is started
    # The planner sees a bright rising Sun and no viable target, so its
    # first answer is end_of_session and the document ends the session
    # before slewing or capturing anything. Before rp #465 the reason
    # was wait_for_twilight and the document's zero-frames heuristic
    # read it as dusk — it would have waited forever.
    Then the run ends within 30 seconds
    And the SSE stream should show exactly 0 "slew_complete" events
    And the SSE stream should show exactly 0 "exposure_complete" events

  Scenario: A target sinking below its altitude floor switches the dispatch loop to the next target
    Given a running Alpaca simulator
    And an observing site where it is astronomical night and the first of two planner targets sinks below its floor after 120 seconds
    And the simulated mount matches the site and points at the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 2s    |
      | max_frames     | 0     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | false |
    And an SSE client is watching rp's event stream
    When a run is started
    # The first slew acquires the sinking target; the second acquires
    # the backup once the planner drops the first below its floor.
    Then a second "slew_complete" event should be observed within 300 seconds
    And at least 1 "exposure_complete" event should precede the second "slew_complete" on the stream
    And at least 1 "exposure_complete" event should follow the second "slew_complete" on the stream

  Scenario: The refocus trigger re-runs auto_focus after the configured number of frames
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And rp is running with a camera, a mount, a focuser, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration         | 500ms |
      | max_frames        | 3     |
      | focus             | true  |
      | refocus_every     | 2     |
      | refocus_hfr_factor | 0    |
      | centering         | false |
      | park_on_finish    | false |
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 180 seconds
    # One sweep at acquisition, one from the refocus-after-frames
    # trigger after the second light frame — both train-addressed, the
    # sweep geometry coming from the imaging train's auto_focus block.
    # focus_started is emitted at sweep begin, so the count holds
    # whether or not the V-curve fit succeeds against the simulator's
    # images (the document try-wraps auto_focus and continues on a
    # failed fit).
    And the SSE stream should show exactly 2 "focus_started" events

  Scenario: A due meridian flip re-slews to the target between exposures, never during one
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    # meridian_margin is far above any real time_to_flip, so the 30s
    # meridian poll's first cycle fires the trigger deterministically
    # mid-loop.
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration       | 2s     |
      | max_frames      | 20     |
      | focus           | false  |
      | centering       | false  |
      | park_on_finish  | false  |
      | meridian_margin | 100000 |
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 240 seconds
    # The acquisition slew plus at least one flip re-slew.
    And the SSE stream should show at least 2 "slew_complete" events
    And no "slew_complete" event should fall between an "exposure_started" and its "exposure_complete"

  Scenario: A safety interruption pauses the session and the resumed run re-acquires the target
    Given a running Alpaca simulator
    And a safety monitor guards the session
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub plate solver echoing the first target
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 2s    |
      | max_frames     | 4     |
      | focus          | false |
      | centering      | true  |
      | park_on_finish | false |
    And an SSE client is watching rp's event stream
    When a run is started
    And the deep-sky session has captured at least 2 frames
    And the safety monitor reports unsafe
    Then the run reports "paused" within 5 seconds
    And the run is paused for "safety"
    And the blackboard is kept
    When the safety monitor reports safe again
    Then the run reports "running" within 5 seconds
    And the blackboard is deleted within 120 seconds
    # Two acquisitions (initial + the re-acquisition the document
    # performs on params._recovery when the engine resumes by itself
    # after the safety pause) — that is the resume contract this
    # scenario pins.
    And the SSE stream should show exactly 2 "centering_complete" events
    # 4 light frames + 2 centering solve frames, plus at most one frame
    # whose exposure completed just before the safety abort landed.
    And the SSE stream should show between 6 and 7 "exposure_complete" events
    And the SSE stream should show exactly 2 "safety_changed" events

  Scenario: A guided session starts guiding after acquisition, dithers on cadence, and stops before parking
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    And a stub guider accepting guide commands
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 500ms |
      | max_frames     | 3     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | true  |
      | guide          | true  |
      | dither_every   | 2     |
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 120 seconds
    # Guiding starts once (one target, one acquisition), the one dither
    # lands after the second recorded frame, and guiding stops at
    # shutdown before the park.
    And the SSE stream should show exactly 1 "guide_settled" event
    And the SSE stream should show exactly 1 "dither_settled" event
    And the SSE stream should show exactly 1 "guide_stopped" event
    And the SSE stream should show exactly 1 "park_complete" event
    And no "dither_settled" event should fall between an "exposure_started" and its "exposure_complete"
    And the stub guider should have received exactly 1 "/guiding/start" request
    And the stub guider should have received exactly 1 "/dither" request
    And the stub guider should have received exactly 1 "/guiding/stop" request

  Scenario: The guide focus watch events drive the document's refocus triggers end to end
    Given a running Alpaca simulator
    And an observing site where it is astronomical night with one planner target
    And the simulated mount matches the site and points at the first target
    # Lifecycle mode: the stub serves no guide frames until the
    # document starts guiding, so rp's watch degrades (2.0 -> 3.0)
    # only during the run — the engine's intake sees the events.
    And a lifecycle stub guider with the HFD script "2.0,3.0"
    And the stub guider has a focus watch of window 3, poll interval "250ms", and escalation deadline "3s"
    And a guiding train "guide" on the simulator's focuser with a metric auto_focus block
    And rp is running with a camera, a mount, and session-runner running the "deep_sky" workflow with parameters:
      | exposure_duration      | 2s    |
      | max_frames     | 8     |
      | focus          | false |
      | centering      | false |
      | park_on_finish | false |
      | guide          | true  |
    And an SSE client is watching rp's event stream
    When a run is started
    Then the run ends within 240 seconds
    # guide_focus_degraded fires the document's guide-only metric
    # auto_focus (focus_started proves the wiring); the episode stays
    # degraded past the deadline, and guide_focus_escalation fires the
    # full refocus_train (refocus_started proves it). Sweep success is
    # deliberately not asserted — the stub's flat post-degradation HFD
    # makes every fit fail, and the document try-wraps both calls.
    And the SSE stream should show at least 1 "guide_focus_degraded" events
    And the SSE stream should show at least 1 "focus_started" events
    And the SSE stream should show at least 1 "guide_focus_escalation" events
    And the SSE stream should show at least 1 "refocus_started" events

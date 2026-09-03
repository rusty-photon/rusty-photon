@serial
Feature: Planner convenience tools
  rp exposes the planner MCP tools — `get_target_status`,
  `get_next_target`, `get_meridian_status`, `record_exposure`,
  `get_session_progress` — that compose the primitives from
  `ephemeris_primitives.feature`, the embedded catalog, and the
  per-goal progress derived from the frames on disk
  (`target_store_progress.feature`). v1 implements §"Dynamic
  Planner" decision-logic bullets 1 (altitude half), 2, 3, 4, and 6
  (eliminate below-floor and goal-met targets, prefer transiting,
  break near-transit ties by least progress then filter batching,
  twilight / end-of-session fallback).

  Progress is never a counter the planner keeps: `capture` writes a
  frame, and every later read derives the counts from the files. These
  scenarios therefore drive the planner by placing frames on disk, not
  by calling `record_exposure` — which increments nothing.

  Planner candidates come from the active rows of the rp-targets store
  (seeded via the `add_target` MCP tool); the legacy `targets[]` config
  array was retired. Altitude-gating parity against the store is pinned
  separately in `target_store_planner.feature`.

  The filter-batching tie-break (bullet 4) reads the filter wheel: among
  equally transiting, equally progressed candidates, the target whose
  next goal names the filter currently in the wheel wins. The wheel is
  the imaging train's sole filter wheel when `get_next_target` is passed
  `train_id`, or the rig's only configured wheel when it is not; a rig
  without a wheel, a disconnected wheel or a failed read leave the
  tie-break neutral and store order decides. Nothing is remembered from
  `record_exposure`.

  The reason discriminant for `get_next_target` is a structured
  string (`best_transiting_candidate`, `no_targets_configured`,
  `all_below_min_altitude`, `wait_for_twilight`, `end_of_session`)
  so a planner plugin can branch without parsing free-form text.

  A recommendation carries the target as a `target` object whose
  coordinate nests as `coord: {ra_hours, dec_degrees}`, plus the plan
  entry to shoot next as a nested `exposure` object:
  `exposure.filter` and `exposure.duration_secs` are the first goal (in
  store order) whose `desired_count` the frames on disk have not yet
  met; `exposure` is null when the target defines no goals — the
  orchestrator then falls back to its own exposure parameters.

  Scenario: Tool catalog includes the convenience tools
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "get_target_status"
    And the tool list should include "get_next_target"
    And the tool list should include "get_meridian_status"
    And the tool list should include "record_exposure"
    And the tool list should include "get_session_progress"

  Scenario: get_target_status accepts a catalog name
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_target_status" for target "M 31"
    Then the tool call should succeed
    And the result target_name should be "M 31"
    And the result altitude_degrees should be a finite number

  Scenario: get_next_target with no targets configured returns the structured branch
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "no_targets_configured"
    And the result target should be null

  Scenario: get_next_target returns the first goal for the recommended target
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 120s              | 2             |
      | Blue   | 1x1     | 60s               | 2             |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "best_transiting_candidate"
    And the result filter should be "Red"
    And the result duration_secs should be 120

  Scenario: get_next_target leaves the plan null for a target without goals
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Bare Field"
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "best_transiting_candidate"
    And the result filter should be null
    And the result duration_secs should be null

  # The dusk/dawn pair pins bullet 6's sky gating end-to-end against
  # the real ephemeris: a never-visible target (floor 90 degrees, which
  # no computed altitude reaches) forces the no-survivors branch, and
  # the explicit evaluation time puts the Sun in evening vs morning
  # twilight at the configured site. 2026-03-20 is the March equinox:
  # at this longitude the Sun sits near -10 degrees and descending at
  # 19:20 UTC, near -10 degrees and climbing at 05:00 UTC.
  Scenario: get_next_target in evening twilight says wait_for_twilight
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the never-visible target "Below Floor"
    When the MCP client calls "get_next_target" at time "2026-03-20T19:20:00Z"
    Then the tool call should succeed
    And the result reason should be "wait_for_twilight"
    And the result target should be null

  Scenario: get_next_target in morning twilight says end_of_session
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the never-visible target "Below Floor"
    When the MCP client calls "get_next_target" at time "2026-03-20T05:00:00Z"
    Then the tool call should succeed
    And the result reason should be "end_of_session"
    And the result target should be null

  # The progress scenarios place frames on disk and let the planner
  # derive the counts, which is how a real night works: `capture`
  # writes the file, nothing increments a counter. An always-visible
  # target (floor -90) keeps the recommendation deterministic at any
  # wall-clock, and counted goals give the planner finite integration
  # targets. Targets are addressed by slug — a frame belongs to a
  # target through the `{target}` token in its path ("Test Field" ->
  # test-field).

  Scenario: record_exposure reports the target's derived progress
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 2             |
    And the data directory contains these frames:
      | path                                                                             | sidecar |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "record_exposure" for target "test-field" filter "Red"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      | Red    | 1x1     | 2m                | 1    | 1     | 2             |

  Scenario: A met integration goal rotates the recommendation to the next goal
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 1             |
      | Blue   | 1x1     | 1m                | 1             |
    And the data directory contains these frames:
      | path                                                                             | sidecar |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "best_transiting_candidate"
    And the result filter should be "Blue"
    And the result duration_secs should be 60

  Scenario: Exhausting every integration goal ends the session
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 1             |
    And the data directory contains these frames:
      | path                                                                             | sidecar |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "end_of_session"
    And the result target should be null

  # A rejected frame is captured but not good, and the planner counts
  # goals met on `good` — so a night of bad seeing keeps the target in
  # the rotation instead of retiring it on frames that will never stack.
  Scenario: A frame rejected by grading does not exhaust its goal
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is configured with target-store default grading max_hfr_pixels 3.0
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 1             |
    And the data directory contains these frames:
      | path                                                                             | sidecar      |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | {"hfr": 9.9} |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "best_transiting_candidate"
    And the result filter should be "Red"

  # get_target_status resolves target_name through the embedded catalog
  # for its sky maths, then slugifies it to match a store row for
  # progress — so this target is stored under a catalog name, the way
  # the tool is actually queried.
  Scenario: get_target_status reports the matched target's derived progress
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "M 31" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 2             |
    # A name becomes a slug exactly one way — whitespace runs collapse to
    # a hyphen — so "M 31" is stored, imaged, and looked up as m-31.
    And the data directory contains these frames:
      | path                                                                 | sidecar |
      | m-31/2026-07-30/Light/m-31_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "get_target_status" for target "M 31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      | Red    | 1x1     | 2m                | 1    | 1     | 2             |

  Scenario: get_session_progress reports every configured target's derived progress
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 2             |
      | Blue   | 1x1     | 1m                | 1             |
    And the data directory contains these frames:
      | path                                                                             | sidecar |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "get_session_progress"
    Then the tool call should succeed
    And the progress for target "test-field" should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      | Red    | 1x1     | 2m                | 1    | 1     | 2             |
      | Blue   | 1x1     | 1m                | 0    | 0     | 1             |

  Scenario: The planner balances equally transiting targets by progress
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible targets "First Field" and "Second Field", each wanting 2 unfiltered 2-second frames
    # Identical coordinates mean an exact transit tie; without the
    # captured frame, store order would recommend "First Field". The
    # unfiltered goal renders "NA" for the filter token, exactly as
    # `capture` writes it on a rig with no filter wheel.
    And the data directory contains these frames:
      | path                                                                                | sidecar |
      | first-field/2026-07-30/Light/first-field_NA_1x1_0001_2s_fpos_0_-10C_aaaaaaa1.fits   | absent  |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the recommended target should be "second-field"

  Scenario: The planner prefers the target whose next goal matches the filter in the wheel
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera and filter wheel on the simulator in an imaging train
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Blue First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Blue   | 1x1     | 2s                | 2             |
    And the MCP client has added the always-visible target "Red First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2s                | 2             |
    # Identical coordinates and untouched goals: an exact tie all the way
    # down to bullet 4, where store order would pick "Blue First".
    When the MCP client calls "set_filter" with train "main" and filter "Red"
    And the MCP client calls "get_next_target" with train "main"
    Then the tool call should succeed
    And the recommended target should be "red-first"
    When the MCP client calls "set_filter" with train "main" and filter "Blue"
    And the MCP client calls "get_next_target" with train "main"
    Then the tool call should succeed
    And the recommended target should be "blue-first"

  Scenario: The rig's only filter wheel is read when no train is named
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera and filter wheel on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Blue First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Blue   | 1x1     | 2s                | 2             |
    And the MCP client has added the always-visible target "Red First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2s                | 2             |
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    And the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the recommended target should be "red-first"

  Scenario: A wheel read failure leaves the tie-break neutral
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    # Nothing listens on port 1: the wheel is configured but never
    # connects, so the read fails and store order decides.
    And rp is running with a filter wheel at "http://127.0.0.1:1" device 0
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Blue First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Blue   | 1x1     | 2s                | 2             |
    And the MCP client has added the always-visible target "Red First" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2s                | 2             |
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the recommended target should be "blue-first"

  Scenario: record_exposure rejects a target that is not configured
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field"
    When the MCP client calls "record_exposure" for target "not-configured" filter "Red"
    Then the tool call should fail
    And the tool error message should mention "unknown target"

  Scenario: get_target_status fails when site is missing
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_target_status" for target "M 31"
    Then the tool call should fail
    And the tool error message should mention "site not configured"

  Scenario: get_target_status fails for an unknown name
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_target_status" for target "M 999"
    Then the tool call should fail
    And the tool error payload should have field "error" equal to "target_not_found"

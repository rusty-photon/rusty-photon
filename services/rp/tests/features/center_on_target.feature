@serial
Feature: Center on target compound tool
  The center_on_target MCP tool drives an iterative
  capture → plate_solve → sync_mount → slew loop until the great-circle
  residual between the solved field-center and the requested (ra, dec)
  sits at or below tolerance_arcsec. ra is decimal hours [0, 24); dec is
  decimal degrees [-90, 90]; the input is converted to degrees once for
  the residual check (the solved values are degrees on the wire). On the
  first iteration the tool issues sync_mount with the solved center
  unconditionally — the first solve is the absolute pointing reference
  and subsequent iterations rely on the mount honouring relative slews
  rather than re-syncing. After sync, if the residual is already inside
  tolerance the loop returns without slewing; otherwise it slews to
  (ra, dec) and continues. Subsequent iterations skip the sync, slew on
  miss, and return on hit. The loop errors with tolerance_not_reached
  after max_attempts and propagates any per-iteration capture /
  plate_solve / sync_mount / slew failure verbatim. center_on_target
  does not write a section on any single exposure document — each
  per-iteration capture's wcs section is written by the embedded
  plate_solve, and the compound result is returned via MCP plus
  centering_started / centering_iteration / centering_complete events.
  Inputs (camera_id, ra, dec, duration, tolerance_arcsec, max_attempts)
  are required; max_attempts is capped at 50 (MAX_ATTEMPTS) before any
  motion. The mount is resolved via the singular mount config — there is
  no telescope_id parameter.

  Scenario: Tool catalog includes center_on_target
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "center_on_target"

  Scenario: Single-iteration happy path returns converged immediately
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should succeed
    And the center_on_target result should report attempts 1
    And the center_on_target iterations[0] action should be "converged"
    And the center_on_target result should contain "final_ra"
    And the center_on_target result should contain "final_dec"
    And the center_on_target result should contain "final_error_arcsec"

  # Canned WCS values are kept within ~2 arcmin of the input target
  # so the iter-1 sync teleports the mount only a tiny distance and
  # the subsequent slew completes well within do_slew_blocking's
  # 300 s deadline (and rmcp's 300 s keep-alive). Earlier values
  # ~9° off target reliably hung windows / bdd / rp under CI load
  # — see issue tracker for the OmniSim slew-time investigation.
  Scenario: Multi-iteration happy path converges on iteration 2
    Given a running Alpaca simulator
    And a stub plate solver returning these per-call WCS responses:
      | ra_center | dec_center |
      | 10.7095   | 41.289     |
      | 10.6845   | 41.269     |
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should succeed
    And the center_on_target result should report attempts 2
    And the center_on_target iterations[0] action should be "sync"
    And the center_on_target iterations[1] action should be "converged"
    And the stub plate solver should have received 2 solve calls

  Scenario: tolerance_not_reached after max_attempts
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    # Default canned WCS is (10.6848°, 41.269°). Target is ~3.6"
    # off in dec — bigger than the 1" tolerance, so iter 1 + iter 2
    # both miss → tolerance_not_reached. Slew distance per iter is
    # ~3.6" (tiny) so this can't hang under any plausible OmniSim
    # timing.
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.270"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.270 duration "100ms" tolerance_arcsec 1 max_attempts 2
    Then the tool call should return an error
    And the error message should contain "tolerance_not_reached"

  Scenario: Mid-loop plate_solve failure aborts and propagates
    Given a running Alpaca simulator
    And a stub plate solver returning error code "solve_failed" with message "ASTAP exited with code 1"
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "solve_failed"

  Scenario: Mid-loop equipment failure (tracking off) aborts and propagates
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to false
    And an MCP client connected to rp
    # ASCOM requires Tracking == true before equatorial sync_mount
    # and slew operations — the iter-1 sync_mount propagates the
    # natural Alpaca error if tracking is off, aborting the loop.
    # We assert the propagated error fragment from do_sync_mount
    # ("failed to sync mount") so the scenario fails loud if the
    # abort point ever moves to a different inner call (capture,
    # plate_solve, slew). Target is kept close to the canned WCS
    # for defense-in-depth — even if the impl ever stopped
    # requiring tracking for sync, the subsequent slew would still
    # be tiny.
    When the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.270 duration "100ms" tolerance_arcsec 1 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "failed to sync mount"

  Scenario: Three-iteration multi-iter run records sync, slew, then converged
    Given a running Alpaca simulator
    And a stub plate solver returning these per-call WCS responses:
      | ra_center | dec_center |
      | 10.7095   | 41.289     |
      | 10.7095   | 41.289     |
      | 10.6845   | 41.269     |
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    # Sync-on-iter-1-only invariant: iter 1 records "sync" (sync +
    # slew), iter 2 records "slew" (no sync), iter 3 records
    # "converged". The strong invariant — that sync_to is invoked
    # exactly once across the whole loop — is verified by the
    # synthetic-mount unit test in `imaging::tools::center_on_target`
    # which counts adapter calls directly. This BDD scenario
    # validates the user-visible action sequence and exercises the
    # live OmniSim mount through a multi-iter sync/slew/converged
    # cycle.
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should succeed
    And the center_on_target result should report attempts 3
    And the center_on_target iterations[0] action should be "sync"
    And the center_on_target iterations[1] action should be "slew"
    And the center_on_target iterations[2] action should be "converged"

  Scenario: Per-iteration wcs sections persist on every captured document
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub plate solver returning these per-call WCS responses:
      | ra_center | dec_center |
      | 10.7095   | 41.289     |
      | 10.6845   | 41.269     |
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then 2 FITS files should exist in the pinned data directory
    And every sidecar JSON in the pinned data directory should contain an "wcs" section

  Scenario: Centering captures every iteration at the binning it was given
    Given rp's data_directory is pinned to a fresh tempdir
    And a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.7123" dec "41.269"
    And the MCP client calls center_on_target at binning "2x2"
    Then the tool call should succeed
    And every sidecar JSON in the pinned data directory should report binning "2x2"

  Scenario: Nonexistent camera returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls center_on_target with camera "nonexistent" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "camera not found"

  Scenario: Disconnected camera returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with an unreachable camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "camera not connected"

  Scenario: No mount configured returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "no mount configured"

  Scenario: Mount not connected returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and an unreachable mount
    And an MCP client connected to rp
    When the MCP client calls center_on_target with camera "main-cam" ra 0.7123 dec 41.269 duration "100ms" tolerance_arcsec 60 max_attempts 5
    Then the tool call should return an error
    And the error message should contain "mount not connected"

  Scenario Outline: Rejects calls missing required parameters
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls center_on_target omitting "<missing_param>"
    Then the tool call should return an error
    And the error message should contain "<missing_param>"

    Examples:
      | missing_param    |
      | camera_id        |
      | ra               |
      | dec              |
      | duration         |
      | tolerance_arcsec |
      | max_attempts     |

  Scenario Outline: Rejects out-of-range numeric parameters
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls center_on_target with override "<field>" set to <value>
    Then the tool call should return an error
    And the error message should contain "<field>"

    Examples:
      | field            | value |
      | tolerance_arcsec | 0     |
      | max_attempts     | 0     |
      | max_attempts     | 51    |
      | ra               | 24    |
      | dec              | 91    |

  # Closed-loop scenarios (Phase 4 of
  # docs/plans/archive/sky-survey-camera-mount-following.md): the camera is a
  # real `sky-survey-camera` process following OmniSim's Telescope.
  # Before invoking `center_on_target`, the scenario `POST`s to the
  # camera's `/sky-survey/position` endpoint, which in follow mode
  # arms a *one-shot* pointing override (F7) — the next light
  # exposure renders at that overridden position; subsequent
  # exposures resume reading the mount.
  #
  # That one-shot semantic is what makes the loop converge: iter 0
  # sees the camera "off-target" (the overridden position), syncs
  # the mount to the solved coordinates, slews back to the requested
  # target, and discards the override. Iter 1 reads the mount fresh
  # (now back at the target via the slew), plate-solves to that
  # mount-reported position, and converges. A persistent camera-side
  # offset (the `offset_*_arcsec` config fields) cannot be driven to
  # zero by sync+slew — see the plan's "Note on the offset model" —
  # which is why we use a one-shot override here instead.
  #
  # The plate-solver stub walks a Sequence: first call returns the
  # "off-target" coordinates, second call returns the requested
  # target. rp's persistence layer writes its own FITS from the
  # camera's ImageArray and does not propagate the camera-side WCS,
  # so the stub fakes the solve outcome (production rp ships
  # pointing hints via the request body, not via FITS keywords).

  @e2e-centering
  Scenario: Closed-loop centering converges in 2 iterations after a one-shot off-target override
    Given a running Alpaca simulator
    And the SkyView stub is ready
    And sky-survey-camera follows the simulated mount with offset_ra_arcsec 0.0 offset_dec_arcsec 0.0
    And a stub plate solver returning these per-call WCS responses:
      | ra_center | dec_center |
      | 180.0167  | -30.0125   |
      | 180.0000  | -30.0000   |
    And rp is running with the sky-survey-camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "12.0000" dec "-30.0000"
    And sky-survey-camera is told its next exposure is at ra_deg 180.0167 dec_deg -30.0125
    And the MCP client calls center_on_target with camera "main-cam" ra 12.0000 dec -30.0000 duration "100ms" tolerance_arcsec 5 max_attempts 3
    Then the tool call should succeed
    And the center_on_target result should report attempts 2
    And the center_on_target iterations[0] action should be "sync"
    And the center_on_target iterations[1] action should be "converged"
    And the stub plate solver should have received 2 solve calls

  @e2e-centering
  Scenario Outline: Closed-loop centering converges for varied one-shot off-target overrides
    Given a running Alpaca simulator
    And the SkyView stub is ready
    And sky-survey-camera follows the simulated mount with offset_ra_arcsec 0.0 offset_dec_arcsec 0.0
    And a stub plate solver returning these per-call WCS responses:
      | ra_center             | dec_center             |
      | <off_target_ra_deg>   | <off_target_dec_deg>   |
      | 180.0000              | -30.0000               |
    And rp is running with the sky-survey-camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "12.0000" dec "-30.0000"
    And sky-survey-camera is told its next exposure is at ra_deg <off_target_ra_deg> dec_deg <off_target_dec_deg>
    And the MCP client calls center_on_target with camera "main-cam" ra 12.0000 dec -30.0000 duration "100ms" tolerance_arcsec 5 max_attempts 3
    Then the tool call should succeed
    And the center_on_target result should report attempts 2
    And the center_on_target iterations[0] action should be "sync"
    And the center_on_target iterations[1] action should be "converged"

    # Off-target ra/dec span ~30"–~60" miss across the table.
    Examples:
      | off_target_ra_deg | off_target_dec_deg |
      | 180.00833         | -30.00556          |
      | 180.01667         | -30.01250          |
      | 179.99167         | -29.99444          |

@serial
Feature: Sky-flat workflow document
  The shipped sky_flat.json points the mount at the zenith — right
  ascension from get_local_sidereal_time, declination from the
  site_latitude_degrees parameter — turns tracking off, and captures
  per-filter twilight flats, re-scaling the exposure after every frame
  against the measured median (the sky, unlike a flat panel, keeps
  changing). In-band frames count toward the filter's plan; a sky pinned
  against the operator's exposure bounds closes the twilight window and
  ends the run with a partial report.

  OmniSim's image content does not track exposure duration, so this
  feature pins the end-to-end plumbing: a 0.5 target ADU fraction with
  1.0 tolerance makes every simulated frame land in-band (a median can
  never stray more than 100% from half of max_adu), so the frame counts
  are exact. The adaptation math — rescale-always, discards, both window
  closures, the attempt budget — is pinned by the engine's exec tests
  running the shipped document against scripted medians.

  Scenario: Sky flats are captured at the zenith across the filter plan and the mount parks
    Given a running Alpaca simulator
    And a flat plan of 2 "Luminance" flats and 2 "Red" flats
    And an observing site where it is astronomical night
    And the simulated mount matches the site and points at the zenith
    And rp is running with a camera, a mount, a filter wheel, and session-runner running the "sky_flat" workflow with parameters:
      | site_latitude_degrees | 0     |
      | tolerance             | 1.0   |
      | initial_duration      | 100ms |
      | min_exposure_duration | 10ms  |
      | max_exposure_duration | 500ms |
    And an SSE client is watching rp's event stream
    When a run is started
    And the workflow document runs to completion
    Then the run should report "complete"
    And the SSE stream should show exactly 1 "unpark_complete" event
    And the SSE stream should show exactly 1 "slew_complete" event
    And the SSE stream should show exactly 4 "exposure_complete" events
    And the SSE stream should show exactly 1 "park_complete" event

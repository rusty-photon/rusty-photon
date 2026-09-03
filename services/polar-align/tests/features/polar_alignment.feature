@serial
Feature: Plate-solving polar alignment (end-to-end)
  The polar-align orchestrator measures the mount's RA-axis direction
  from three plate solves taken with only the RA axis moving, reports
  the axis-to-pole error as signed azimuth and altitude arcminutes on
  its /status endpoint, then holds an adjustment phase until the
  operator finishes it. A run is started at the service's own POST
  /runs and its outcome lives on /status — nothing is posted to rp —
  so both the numeric contract and the run's end are asserted there.

  These tests start OmniSim (telescope + camera), rp, and polar-align,
  with an in-process plate-solver stub whose canned solves are
  choreographed from a known injected axis error.

  Scenario: Measurement recovers a choreographed axis error
    Given a running Alpaca simulator
    And a stub plate solver choreographed for an axis error of 30.0 arcminutes east and -20.0 arcminutes in altitude
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "adjusting" phase
    And the adjustment is finished via the REST API
    Then the polar-align status should report an azimuth error within 2.0 arcminutes of 30.0
    And the polar-align status should report an altitude error within 2.0 arcminutes of -20.0
    And the polar-align status should report an axis cross-check below 1.0 arcseconds
    And the stub plate solver should have received at least 3 solve requests
    And the preview endpoint should serve a PNG at most 1024 pixels wide
    And the polar-align run has ended

  Scenario: A failing solver aborts the measurement and ends the run
    Given a running Alpaca simulator
    And a stub plate solver that always fails
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "error" phase
    Then the polar-align run has ended

  Scenario: Matrix-less adjustment solves abort the workflow after repeated failures
    Given a running Alpaca simulator
    And a stub plate solver whose solves stop carrying a WCS matrix after the second point
    And the workflow tolerates at most 3 consecutive failed solves
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "error" phase
    Then the polar-align status error should mention "consecutive adjustment solves failed"
    And the polar-align run has ended

  Scenario: Adjustment completes on its own when the window expires
    Given a running Alpaca simulator
    And a stub plate solver choreographed for an axis error of 30.0 arcminutes east and -20.0 arcminutes in altitude
    And the adjustment window is limited to 1 second
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "complete" phase
    Then the polar-align run has ended

  Scenario: Finishing with no adjustment in progress is rejected
    Given the polar-align service is running standalone
    When the adjustment is finished via the REST API
    Then the finish request should be rejected with status 409

  Scenario: A run cannot start without an rp to reach
    Given the polar-align service is running standalone
    When a run is started without a configured rp
    Then the run request should be rejected with status 400
    And the polar-align workflow phase should be "idle"

  Scenario: Measurement from the current pointing recovers a choreographed axis error
    Given a running Alpaca simulator
    And the simulator mount is synced to a mid-sky pointing west of the meridian
    And a stub plate solver choreographed for an axis error of 30.0 arcminutes east and -20.0 arcminutes in altitude from a mid-sky pointing
    And the measurement starts from the current mount position
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "adjusting" phase
    And the adjustment is finished via the REST API
    Then the polar-align status should report an azimuth error within 2.0 arcminutes of 30.0
    And the polar-align status should report an altitude error within 2.0 arcminutes of -20.0
    And the polar-align status should report an axis cross-check below 1.0 arcseconds
    And the first solve request should be hinted within 1.0 degrees of the synced pointing
    And the polar-align run has ended

  Scenario: A matrix-less solve aborts a measurement from the current pointing
    Given a running Alpaca simulator
    And the simulator mount is synced to a mid-sky pointing west of the meridian
    And a stub plate solver whose solves stop carrying a WCS matrix after the second point
    And the measurement starts from the current mount position
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "error" phase
    Then the polar-align status error should mention "full camera attitudes"
    And the polar-align run has ended

  Scenario: The observer site resolves from rp when the plugin config has none
    Given a running Alpaca simulator
    And rp is configured with the simulator's default observer site
    And a stub plate solver choreographed for an axis error of 30.0 arcminutes east and -20.0 arcminutes in altitude at the simulator's default site
    And the polar-align config carries no site block
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "adjusting" phase
    And the adjustment is finished via the REST API
    Then the polar-align status should report an azimuth error within 2.0 arcminutes of 30.0
    And the polar-align status should report an altitude error within 2.0 arcminutes of -20.0
    And the polar-align run has ended

  Scenario: A site missing on both sides aborts before any motion
    Given a running Alpaca simulator
    And a stub plate solver that always fails
    And the polar-align config carries no site block
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow reaches the "error" phase
    Then the polar-align status error should mention "site"
    And the polar-align run has ended

  Scenario: Manual rotation measures a hand-rotated tracker
    Given a running Alpaca simulator
    And a stub plate solver choreographed for an axis error of 30.0 arcminutes east and -20.0 arcminutes in altitude from a mid-sky pointing
    And the measurement uses manual rotation
    And rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator
    When a run is started
    And the polar-align workflow awaits measurement point 2
    And the operator confirms the manual rotation
    And the polar-align workflow awaits measurement point 3
    And the operator confirms the manual rotation
    And the polar-align workflow reaches the "adjusting" phase
    And the adjustment is finished via the REST API
    Then the polar-align status should report an azimuth error within 2.0 arcminutes of 30.0
    And the polar-align status should report an altitude error within 2.0 arcminutes of -20.0
    And the polar-align status should report an axis cross-check below 1.0 arcseconds
    And the first solve request should carry no pointing hint
    And the polar-align run has ended

  Scenario: Confirming a rotation while nothing waits is rejected
    Given the polar-align service is running standalone
    When the operator confirms the manual rotation
    Then the continue request should be rejected with status 409

  Scenario: The preview is absent before any capture
    Given the polar-align service is running standalone
    When the preview endpoint is fetched
    Then the preview request should be rejected with status 404

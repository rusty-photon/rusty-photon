@serial
Feature: Guiding tools on the guider's contract
  rp's six guiding tools proxy to the guider service over the HTTP
  contract of docs/services/guider.md: start_guiding, stop_guiding,
  dither, pause_guiding, resume_guiding and get_guiding_stats. rp
  serves no restart_guider tool. A start blocks until the run has
  settled and returns the settled figures in guide-camera pixels and
  in arcseconds. The settle values of a start resolve field by field:
  the call's argument, then the guiding config's key, then the
  guider's own default, which rp leaves to the guider by sending no
  value. guide_started carries the start's deadline as
  max_duration_ms: calibration_timeout plus the resolved settle
  timeout plus 15 s, and no deadline when no settle timeout is
  resolved. A stop answers the state idle and emits guide_stopped with
  the reason requested and a null stop_reason; the safety stop emits
  it with the reason safety. A resume answers guiding, or star_lost
  when the run is in it. get_guiding_stats returns the guider's stats
  without the running totals. A refusal of the guider's reaches the
  caller with the guider's error code and message.

  The stub guider answers every endpoint of the contract, follows its
  starts, stops, pauses and resumes from the state each scenario
  gives it, and records every request. A start it accepts settles with
  an RMS of 0.21 px in RA, 0.18 px in Dec and 0.28 px in total, which
  is 0.63, 0.54 and 0.83 arcseconds, over 12 samples. Its stats
  report the same figures while it guides, at a pixel scale of 3.0
  arcseconds per pixel, with an SNR of 25.0, a star mass of 5432.0 and
  5 stars. Its device report names the simulator's mount and a camera
  that is none of rp's, so the check of guider_devices_check.feature
  passes. A stub guider named before rp starts is the guider rp's
  equipment.mount.guiding block points at, and the block names no
  settle value and no calibration_timeout unless a scenario sets one,
  so the calibration_timeout is its default of 10 minutes.

  Scenario: The tool list has the six guiding tools and no restart_guider
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "start_guiding"
    And the tool list should include "stop_guiding"
    And the tool list should include "dither"
    And the tool list should include "pause_guiding"
    And the tool list should include "resume_guiding"
    And the tool list should include "get_guiding_stats"
    And the tool list should not include "restart_guider"

  Scenario: start_guiding returns the settled run's figures in pixels and in arcseconds
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the guider result should contain "state" with value "guiding"
    And the guider result should contain "rms_ra_px" with number 0.21
    And the guider result should contain "rms_dec_px" with number 0.18
    And the guider result should contain "total_rms_px" with number 0.28
    And the guider result should contain "rms_ra_arcsec" with number 0.63
    And the guider result should contain "rms_dec_arcsec" with number 0.54
    And the guider result should contain "total_rms_arcsec" with number 0.83
    And the guider result should contain "sample_count" with number 12
    And the stub guider should have received exactly 1 "/guiding/start" request

  Scenario: A start with no settle value in the call or the config sends the guider none
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the stub guider should have received a start request without a settle override

  Scenario: The guiding config's settle values reach the guider on a start
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator and guider settle pixels 0.8 time "8s" timeout "40s"
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the stub guider should have received a start request with settle pixels 0.8 time "8s" timeout "40s"

  Scenario: A start's own settle arguments override the config's field by field
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator and guider settle pixels 0.8 time "8s" timeout "40s"
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with settle_pixels 1.5 and settle_timeout "20s"
    Then the stub guider should have received a start request with settle pixels 1.5 time "8s" timeout "20s"

  Scenario: recalibrate reaches the guider on a start
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with recalibrate true
    Then the stub guider should have received a start request with recalibrate true

  Scenario Outline: guide_started carries calibration_timeout plus the settle timeout plus 15 s as its deadline
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_started" and "guide_settled"
    And rp's guiding config sets "calibration_timeout" to the JSON "<calibration_timeout>"
    And rp is running with a camera on the simulator and guider settle pixels 0.8 time "8s" timeout "40s"
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the test webhook receiver should receive a "guide_started" event
    And the test webhook receiver should receive a "guide_settled" event
    And the "guide_started" event should carry a max_duration_ms of <max_duration_ms>
    And the "guide_started" event payload field "settle_timeout" should be "40s"
    And the "guide_settled" event payload field "rms_ra_px" should be the number 0.21
    And the "guide_settled" event payload field "rms_dec_px" should be the number 0.18
    And the "guide_settled" event payload field "total_rms_px" should be the number 0.28
    And the "guide_settled" event payload field "sample_count" should be the number 12

    Examples:
      | calibration_timeout | max_duration_ms |
      | 10m                 | 655000          |
      | 2m                  | 175000          |

  Scenario: A start with no settle timeout resolved carries no deadline
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And a test webhook receiver subscribed to "guide_started"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the test webhook receiver should receive a "guide_started" event
    And the "guide_started" event reserves the deadline fields as absent

  Scenario: stop_guiding answers idle and emits guide_stopped as requested, with no stop reason
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "stop_guiding"
    Then the guider result should contain "state" with value "idle"
    And the stub guider should have received a stop request
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "requested"
    And the "guide_stopped" event payload field "stop_reason" should be the JSON null

  Scenario: stop_guiding on an idle guider answers idle
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "stop_guiding"
    Then the guider result should contain "state" with value "idle"

  Scenario: The safety stop emits guide_stopped with the reason safety and no stop reason
    Given a running Alpaca simulator
    And a safety monitor on the simulator
    And a stub guider in state "guiding"
    And a test webhook receiver subscribed to "guide_stopped"
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the operator unparks the mount
    And the safety monitor reports unsafe
    Then the stub guider should have received a stop request within 5 seconds
    And the test webhook receiver should receive a "guide_stopped" event
    And the "guide_stopped" event payload field "reason" should be "safety"
    And the "guide_stopped" event payload field "stop_reason" should be the JSON null

  Scenario: pause_guiding passes full on and answers paused, and resume_guiding answers guiding
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "pause_guiding" with full true
    Then the guider result should contain "state" with value "paused"
    And the stub guider should have received a pause request with full true
    When the MCP client calls "resume_guiding"
    Then the guider result should contain "state" with value "guiding"
    And the stub guider should have received a resume request

  Scenario: resume_guiding answers star_lost for a run that has lost its guide star
    Given a running Alpaca simulator
    And a stub guider in state "star_lost"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "resume_guiding"
    Then the guider result should contain "state" with value "star_lost"

  Scenario: get_guiding_stats returns the guider's stats without the running totals
    Given a running Alpaca simulator
    And a stub guider in state "guiding"
    And the stub guider reports a last stop of reason "mount_slewing"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_guiding_stats"
    Then the guider result should contain "state" with value "guiding"
    And the guider result should contain "guiding" with the JSON true
    And the guider result should contain "settling" with the JSON false
    And the guider result should contain "pixel_scale_arcsec" with number 3.0
    And the guider result should contain "rms_ra_px" with number 0.21
    And the guider result should contain "rms_dec_px" with number 0.18
    And the guider result should contain "total_rms_px" with number 0.28
    And the guider result should contain "rms_ra_arcsec" with number 0.63
    And the guider result should contain "rms_dec_arcsec" with number 0.54
    And the guider result should contain "total_rms_arcsec" with number 0.83
    And the guider result should contain "snr" with number 25.0
    And the guider result should contain "star_mass" with number 5432.0
    And the guider result should contain "star_count" with number 5
    And the guider result should contain "sample_count" with number 12
    And the guider result's "last_stop" should carry the reason "mount_slewing"
    And the guider result should not contain "totals"

  Scenario: get_guiding_stats reports a null last_stop for a guider that has stopped no run
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_guiding_stats"
    Then the guider result should contain "state" with value "idle"
    And the guider result should contain "guiding" with the JSON false
    And the guider result should contain "last_stop" with the JSON null

  Scenario Outline: A start the guider refuses reaches the caller with the guider's code and message
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider answers "/guiding/start" requests with error code "<code>" and message "<message>"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "<code>"
    And the error message should contain "<message>"

    Examples:
      | code                 | message                                        |
      | no_stars             | no star in the frame passed the selection      |
      | calibration_failed   | the star moved too little during calibration   |
      | guide_failed         | the run stopped: the mount is slewing          |
      | settle_timeout       | the settle did not complete in time            |
      | mount_not_ready      | the mount is parked                            |
      | device_not_connected | the camera is not connected                    |
      | device_unreachable   | the mount did not answer                       |
      | camera_leased        | the camera is leased                           |
      | internal             | broken pipe                                    |

  Scenario: A start the guider refuses emits guide_failed and no guide_settled
    Given a running Alpaca simulator
    And a stub guider in state "idle"
    And the stub guider answers "/guiding/start" requests with error code "no_stars" and message "no star in the frame passed the selection"
    And a test webhook receiver subscribed to "guide_failed" and "guide_settled"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the test webhook receiver should receive a "guide_failed" event
    And the "guide_failed" event payload field "error" should contain "no_stars"
    And the test webhook receiver should not have received a "guide_settled" event

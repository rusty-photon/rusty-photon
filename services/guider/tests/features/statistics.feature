Feature: The statistics of guiding
  The RMS window holds the last 50 guide steps: frames that gave an offset
  in `guiding`, whether or not a pulse followed. Its RMS fields are the
  root mean square of the RA and the Dec error, and their total, in pixels
  and in arcseconds at the pixel scale (docs/services/guider.md
  § Statistics). Every accepted `guiding/start` clears it and the metrics
  window, and it survives a stop. The running totals only grow while the
  service runs: the guide steps, the samples and the sums of squares of
  each axis's error, the star-lost frames of runs, and the interruptions.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate. The
  guider polls them every 100 ms, exposes for 200 ms, and settles within
  1.5 px for 1 s, in at most 30 s. Each scenario starts with a stored
  calibration of the simulated rig.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration settles within 1.5 px for 1 s in at most 30 s
    And the state directory holds a calibration of the simulated rig

  Scenario: Guiding fills the RMS window and the totals
    Given the guider is started
    And the guider is guiding
    When the guider's totals count at least 5 guide steps
    Then the guider's stats report RMS fields in pixels and in arcseconds at 2″ a pixel
    And the guider's totals count as many RA and Dec samples as guide steps

  Scenario: A start clears the RMS window and the metrics window, and the totals go on
    Given the guider is started
    And the guider is guiding
    And the guider's totals count at least 5 guide steps
    When a client asks the guider to start guiding
    Then the guider's metrics hold only frames numbered above those they held before the request
    And the guider's totals count at least the guide steps they counted before the request

  Scenario: With Dec off the Dec and total RMS are null and the Dec totals do not grow
    Given the guider configuration's "/control/dec/mode" is the JSON "off"
    And the guider is started
    And the guider is guiding
    When the guider's totals count at least 5 guide steps
    Then the guider's stats report no Dec or total RMS
    And the guider's totals count no Dec sample

  Scenario: The RMS window survives a stop, and the stop counts as an interruption
    Given the guider is started
    And the guider is guiding
    And the guider's totals count at least 5 guide steps
    When a client asks the guider to stop
    Then the guider's stats report the state "idle"
    And the guider's stats report RMS fields in pixels and in arcseconds at 2″ a pixel
    And the guider's totals count at least 1 interruption

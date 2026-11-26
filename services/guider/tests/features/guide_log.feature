Feature: The guide log
  The guider writes a guide log in the format PHD2 writes, in `guide-logs/`
  of the state directory (docs/services/guider.md § Guide log). The first
  calibration or guiding run after each local noon, and the first after the
  service starts, opens a file, named by the local time it was opened. A
  calibration is a section with a row for each frame of its legs; a guiding
  run is a section with a row for each guide step, a `"DROP"` row for each
  star-lost frame, and notes for its dithers, settles, pauses, resumes and
  stop. Opening a file removes the files older than `guide_log.retention`.
  A log that cannot be written never stops guiding. Each scenario's log is
  read once the service has stopped and closed it, and is parsed with the
  parser built from the logs PHD2 wrote on the rigs.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels under a sky of 6″ seeing and three stars, and a mount on the East
  side of the pier at declination 0° that guides at the sidereal rate. The
  guider polls them every 100 ms, exposes for 200 ms, calibrates over 6 px
  in 4 steps, and settles within 1.5 px for 1 s, in at most 30 s.

  Background:
    Given the simulated camera and mount, both connected
    And the simulated sky holds 3 stars
    And the simulated mount guides at the sidereal rate
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration calibrates over 6 px in 4 steps
    And the guider configuration settles within 1.5 px for 1 s in at most 30 s

  Scenario: A calibration is a section of the night's guide log
    Given the guider is started
    When a client asks the guider to calibrate
    And the guider is told to shut down
    Then the state directory holds 1 guide log, named for the time it was opened
    And the guide log parses as PHD2's format
    And the guide log holds a calibration that passed, with rows of the legs West, East, Backlash, North and South
    And the guide log's East rows count down to 0, back to where the West leg began
    And the guide log's South rows count down to 0, back to where the North leg began

  Scenario: A guiding run is a section with a row for each guide step, and its stop ends it
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    And the guider's totals count at least 5 guide steps
    When a client asks the guider to stop
    And the guider is told to shut down
    Then the guide log parses as PHD2's format
    And the guide log holds a guiding run of at least 5 rows that ends
    And the guide log's guiding run has the note "SETTLING STATE CHANGE, Settling started"
    And the guide log's guiding run has the note "SETTLING STATE CHANGE, Settling complete"
    And the guide log's guiding run has the note "Server received guiding/stop"

  Scenario: The guiding header gives the pointing the mount reported when the run began
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When a client asks the guider to stop
    And the guider is told to shut down
    Then the guide log's guiding header gives the pier side East, the hour angle and the altitude

  Scenario: A dither, a pause and a resume are notes of the run
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When a client asks the guider to dither with the body {"amount_px": 4.0}
    And a client asks the guider to pause
    And a client asks the guider to resume
    And a client asks the guider to stop
    And the guider is told to shut down
    Then the guide log parses as PHD2's format
    And the guide log's guiding run has a note beginning "DITHER by "
    And the guide log's guiding run has the note "Server received guiding/pause, full = false"
    And the guide log's guiding run has the note "Server received guiding/resume"

  Scenario: A star-lost frame is a DROP row
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the simulated field jumps 100″ east
    And the guider's stats report the state "star_lost"
    And a client asks the guider to stop
    And the guider is told to shut down
    Then the guide log parses as PHD2's format
    And the guide log's guiding run has a dropped frame

  Scenario: A stop of the guider's own is a note that names its reason
    Given the state directory holds a calibration of the simulated rig
    And the guider is started
    And the guider is guiding
    When the simulated mount starts a slew of an hour
    And the guider's last stop has the reason "mount_slewing"
    And the guider is told to shut down
    Then the guide log parses as PHD2's format
    And the guide log's guiding run has a note beginning "GUIDING STOPPED, mount_slewing: "

  Scenario: Each start of the service opens a new file
    Given the guider is started
    And a client asks the guider to calibrate
    When the guider is started again
    And a client asks the guider to calibrate
    And the guider is told to shut down
    Then the state directory holds 2 guide logs

  Scenario: Opening a file removes the guide logs older than guide_log.retention
    Given the state directory holds a guide log last written 100 days ago
    And the guider is started
    When a client asks the guider to calibrate
    And the guider is told to shut down
    Then the state directory holds 1 guide log, named for the time it was opened

  Scenario: A guide log that cannot be written never stops guiding
    Given the state directory holds a file where the guide logs would go
    And the state directory holds a calibration of the simulated rig
    And the guider is started
    When a client asks the guider to start guiding
    Then the request answers 200 with the state "guiding" and a settled answer

  Scenario: A guider configured to keep no guide log writes none
    Given the guider configuration's "/guide_log/enabled" is the JSON false
    And the guider is started
    When a client asks the guider to calibrate
    And the guider is told to shut down
    Then the state directory holds 0 guide logs

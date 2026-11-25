Feature: Configuration validation at startup
  The guider refuses to start, and exits non-zero before it binds, on a
  configuration it cannot run with, and its error names the field
  (docs/services/guider.md § Configuration validation at startup). The
  devices are not checked at startup: a camera or a mount that does not
  answer is reported, and does not stop the service.

  Each scenario starts from a configuration the guider accepts, its
  listener on port 0 and its state directory in the scenario's scratch
  directory, and changes one field.

  Scenario Outline: A field the guider cannot run with refuses the start and is named
    Given a guider configuration with "<pointer>" set to the JSON <value>
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "<named>"

    Examples:
      | pointer                           | value                              | named                                                      |
      | /stars/maximum                    | 8                                  | unknown field `maximum`                                    |
      | /devices/camera                   | {"alpaca_url": "ftp://camera:1"}   | alpaca_url must be an http:// or https:// URL              |
      | /devices/mount                    | {"alpaca_url": "localhost:11117"}  | alpaca_url must be an http:// or https:// URL              |
      | /optics/focal_length_mm           | 0                                  | optics.focal_length_mm must be a finite number greater than 0 |
      | /optics/focal_length_mm           | -200                               | optics.focal_length_mm must be a finite number greater than 0 |
      | /camera/exposure                  | "2 parsecs"                        | camera.exposure is not a duration                          |
      | /camera/exposure                  | "0s"                               | camera.exposure must be longer than zero                   |
      | /devices/poll_interval            | "0s"                               | devices.poll_interval must be longer than zero             |
      | /stop_timeout                     | "0ms"                              | stop_timeout must be longer than zero                      |
      | /guide_log/retention              | "0d"                               | guide_log.retention must be longer than zero               |
      | /camera/binning                   | 0                                  | camera.binning must be at least 1                          |
      | /camera/max_frame_failures        | 0                                  | camera.max_frame_failures must be at least 1               |
      | /mount/max_failures               | 0                                  | mount.max_failures must be at least 1                      |
      | /mount/min_pulse                  | "2500ms"                           | mount.min_pulse must be shorter than mount.max_pulse       |
      | /stars/min                        | 0                                  | stars.min must be at least 1                               |
      | /stars/min                        | 9                                  | stars.min must be at most stars.max                        |
      | /stars/min_snr                    | 0                                  | stars.min_snr must be a finite number greater than 0       |
      | /stars/search_region_px           | 0                                  | stars.search_region_px must be a finite number greater than 0 |
      | /stars/mass_change                | 0                                  | stars.mass_change must be a number in (0, 1]               |
      | /stars/mass_change                | 1.5                                | stars.mass_change must be a number in (0, 1]               |
      | /calibration/steps                | 3                                  | calibration.steps must be at least 4                       |
      | /calibration/steps                | 61                                 | calibration.steps must be at most calibration.max_steps    |
      | /calibration/distance_px          | 0                                  | calibration.distance_px must be a finite number greater than 0 |
      | /calibration/backlash_clear_px    | 0                                  | calibration.backlash_clear_px must be a finite number greater than 0 |
      | /control/ra/aggression            | 0                                  | control.ra.aggression must be a number in (0, 2]           |
      | /control/dec/aggression           | 2.5                                | control.dec.aggression must be a number in (0, 2]          |
      | /control/ra/hysteresis            | 1                                  | control.ra.hysteresis must be a number in [0, 1)           |
      | /control/ra/hysteresis            | -0.1                               | control.ra.hysteresis must be a number in [0, 1)           |
      | /control/dec/mode                 | "sideways"                         | control.dec.mode must be "auto", "north", "south" or "off" |
      | /control/dec/reversal_frames      | 0                                  | control.dec.reversal_frames must be at least 1             |
      | /control/runaway/frames           | 0                                  | control.runaway.frames must be at least 1                  |
      | /control/min_move_px              | -0.1                               | control.min_move_px must be a finite number of at least 0  |
      | /control/runaway/min_error_px     | -1                                 | control.runaway.min_error_px must be a finite number of at least 0 |
      | /settling/pixels                  | 0                                  | settling.pixels must be a finite number greater than 0     |

  Scenario: A file that is not valid JSON refuses the start
    Given a guider configuration file that ends inside its first object
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "is not valid JSON"

  Scenario: A configuration file named on the command line that does not exist refuses the start
    Given a guider configuration file named on the command line that does not exist
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "does not exist"

  Scenario: A ca_cert that cannot be read refuses the start
    Given a guider configuration whose ca_cert names a file that does not exist
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "cannot be read"

  Scenario: A ca_cert that holds no certificate refuses the start
    Given a guider configuration whose ca_cert names a file holding "not a certificate"
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "holds no certificate"

  Scenario: A state directory that cannot be created refuses the start
    Given a guider configuration whose state directory lies under a regular file
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "cannot be created"

  @unix
  Scenario: A state directory that cannot be written refuses the start
    Given a guider configuration whose state directory cannot be written
    When the guider is started
    Then the guider exits non-zero before it binds
    And its error output names "cannot be written"

  Scenario: A state directory that does not exist is created at startup
    Given a guider configuration whose state directory does not exist yet
    When the guider is started
    Then the guider is serving
    And the state directory exists

  Scenario: A settle time of zero is accepted
    Given a guider configuration with "/settling/time" set to the JSON "0s"
    When the guider is started
    Then the guider is serving

  Scenario: Devices that do not answer do not stop the start
    Given a guider configuration naming a focal length, and a camera and a mount whose servers do not answer
    When the guider is started
    Then the guider is serving
    And /health answers 503 with the message "the camera does not answer; the mount does not answer"

Feature: The camera facade's configuration

  The facade is configured by the camera block of phd2-guider's config
  and is off unless camera.enabled is true. With it on, pixel_size_x_um,
  pixel_size_y_um and image_dir are required, the listener must bind to
  a loopback address, and PHD2 must be reached on a loopback address.
  These rules are checked when the file is parsed, so the doctor
  subcommand reports a broken block the way a start would refuse it.

  The device's ASCOM UniqueID is minted into the config file on the
  first start with the facade enabled, and never changed after.

  The facade device answers the shared config.get, config.apply and
  config.schema actions over the whole of phd2-guider's config. The
  service has no reload path, so a change applies at its next start: an
  applied change is reported under restart_required with status ok.
  The listener fields are read-only, since changing them would cut off
  the client editing through them, and the identity is locked.

  Scenario: The facade's UniqueID is minted into the config file on the first enabled start
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled and no unique_id
    Then the config file should hold a camera unique_id
    And the facade camera's UniqueID should be the config file's camera unique_id

  Scenario: config.schema marks the listener fields read-only and the identity locked
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When the client reads the facade's config schema
    Then the read-only fields should include "server.port"
    And the read-only fields should include "camera.enabled"
    And the read-only fields should include "camera.server.port"
    And the read-only fields should include "camera.server.bind_address"
    And the locked fields should include "camera.unique_id"

  Scenario: config.apply persists a change for the next start
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When the client applies the current config with camera.capture_grace set to "20s"
    Then the apply status should be "ok"
    And the apply should list "camera.capture_grace" as needing a restart
    And the config file should hold camera.capture_grace "20s"

  Scenario: A disabled facade needs none of its fields
    Given this service's config with the camera block {"enabled": false} staged for doctor
    When the doctor subcommand runs
    Then the doctor report is clean

  Scenario: Enabling the facade without pixel sizes fails the config check, naming the field
    Given this service's config with the camera block {"enabled": true, "image_dir": "/var/lib/rusty-photon/phd2-images"} staged for doctor
    When the doctor subcommand runs
    Then the doctor report should fail mentioning "pixel_size_x_um"

  Scenario: A facade listener bound beyond loopback fails the config check
    Given this service's config with the camera block {"enabled": true, "pixel_size_x_um": 2.9, "pixel_size_y_um": 2.9, "image_dir": "/var/lib/rusty-photon/phd2-images", "server": {"bind_address": "0.0.0.0"}} staged for doctor
    When the doctor subcommand runs
    Then the doctor report should fail mentioning "loopback"

  Scenario: A non-positive pixel size fails the config check
    Given this service's config with the camera block {"enabled": true, "pixel_size_x_um": 0.0, "pixel_size_y_um": 2.9, "image_dir": "/var/lib/rusty-photon/phd2-images"} staged for doctor
    When the doctor subcommand runs
    Then the doctor report should fail mentioning "pixel_size_x_um"

  Scenario: A capture grace beyond 60 s fails the config check
    Given this service's config with the camera block {"enabled": true, "pixel_size_x_um": 2.9, "pixel_size_y_um": 2.9, "image_dir": "/var/lib/rusty-photon/phd2-images", "capture_grace": "61s"} staged for doctor
    When the doctor subcommand runs
    Then the doctor report should fail mentioning "capture_grace"

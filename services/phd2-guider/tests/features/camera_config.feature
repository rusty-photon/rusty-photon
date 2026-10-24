Feature: The camera facade's configuration

  The facade is configured by the camera block of phd2-guider's config
  and is off unless camera.enabled is true. With it on, pixel_size_x_um,
  pixel_size_y_um and image_dir are required, the listener must bind to
  a loopback address, and PHD2 must be reached on a loopback address.
  These rules are checked when the file is parsed, so the doctor
  subcommand reports a broken block the way a start would refuse it.

  The device's ASCOM UniqueID is minted into the config file on the
  first start with the facade enabled, and never changed after. The
  block is edited on the guider's setup page (setup_pages.feature).

  Scenario: The facade's UniqueID is minted into the config file on the first enabled start
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled and no unique_id
    Then the config file should hold a camera unique_id
    And the facade camera's UniqueID should be the config file's camera unique_id

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

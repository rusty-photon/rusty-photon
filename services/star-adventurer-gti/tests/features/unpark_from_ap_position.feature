Feature: Unpark from AP position
  The Sky-Watcher firmware resets its encoder counter to (0, 0) on every
  power-up, so the driver must be told which physical pose the OTA is in.
  `mount.unpark_from_ap_position` carries that assumption, named after the
  Astro-Physics park positions ap_park_0..ap_park_5. The field is the
  operator's assertion about the physical world, so the ship default is
  ap_park_0 ("current position" — no seed, "I will plate-solve and
  sync"), the only honest value when nothing was declared; the frame
  stays unanchored until that sync and Park stops in place (see
  park.feature). Declaring a named pose (typically ap_park_3,
  Sky-Watcher's stock home) seeds the firmware encoder on the fresh
  power-up connect so the driver's celestial math matches the physical
  pose and the coordinate frame is anchored from the first connect.

  The power-up pose and the park target are configuration, edited on the
  mount's setup page: a saved power-up pose applies on the next
  fresh-power-up connect. The recovery is a command on that page, at its
  path with ?recover: the operator says the telescope is physically at a
  pose now, ticks the box that confirms it, and the driver resets the
  firmware encoder to that pose before clearing AtPark; from ap_park_0 it
  clears AtPark alone. It runs on a still mount, parked or not. The
  driver has no custom Actions.

  Scenario: Fresh power-up with ap_park_0 leaves the encoder untouched
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_0"
    When I connect the device
    Then the mount should not have received an encoder-seed command

  Scenario: Fresh power-up with ap_park_3 seeds the firmware encoder
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount should have received an encoder seed on both axes

  Scenario: The recovery from a named pose resets the encoder and unparks
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I recover the mount from "ap_park_3" on its setup page, confirming it is there
    Then the response status is 200
    And the page says "ap_park_3"
    And the mount should have received an encoder seed on both axes
    And AtPark should be false

  Scenario: The recovery of a still mount that is not parked resets the encoder
    Given a running star-adventurer service
    When I connect the device
    And I recover the mount from "ap_park_3" on its setup page, confirming it is there
    Then the response status is 200
    And the mount should have received an encoder seed on both axes
    And AtPark should be false

  Scenario: The recovery from ap_park_0 clears AtPark without seeding
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I recover the mount from "ap_park_0" on its setup page, confirming it is there
    Then the response status is 200
    And AtPark should be false
    And the mount should not have received an encoder-seed command

  Scenario: A power-up pose saved on the mount's page is persisted
    Given a running star-adventurer service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "telescope" 0 setting "mount.unpark_from_ap_position" to "ap_park_2"
    Then the page reports the configuration was saved and the service is reloading
    And the persisted config should have unpark_from_ap_position "ap_park_2"

  Scenario: A power-up pose saved on the mount's page applies on the next fresh-power-up connect
    Given a running star-adventurer service
    When the mount's setup page saves the power-up pose "ap_park_3"
    And the reloaded service serves the power-up pose "ap_park_3"
    And I connect the device
    Then the mount should have received an encoder seed on both axes

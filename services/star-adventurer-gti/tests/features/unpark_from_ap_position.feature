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

  Three driver-specific ASCOM Actions expose runtime control:
  SetUnparkFromApPosition persists a new value (applied on the next
  fresh-power-up connect), SetPreferredApPark sets the Park() target, and
  UnparkFromApPosition is a recovery operation that resets the firmware
  encoder to a named park before clearing AtPark. The three names are
  matched without regard to case, as the Alpaca specification requires of
  every Action name; SupportedActions lists the spelling above.

  Scenario: Fresh power-up with ap_park_0 leaves the encoder untouched
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_0"
    When I connect the device
    Then the mount should not have received an encoder-seed command

  Scenario: Fresh power-up with ap_park_3 seeds the firmware encoder
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    Then the mount should have received an encoder seed on both axes

  Scenario: UnparkFromApPosition with a named park resets the encoder and unparks
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I run the UnparkFromApPosition action with "ap_park_3"
    Then the mount should have received an encoder seed on both axes
    And AtPark should be false

  Scenario: UnparkFromApPosition with ap_park_0 clears AtPark without seeding
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I run the UnparkFromApPosition action with "ap_park_0"
    Then AtPark should be false
    And the mount should not have received an encoder-seed command

  Scenario: SetUnparkFromApPosition persists the value to the config file
    Given a running star-adventurer service
    When I connect the device
    And I run the SetUnparkFromApPosition action with "ap_park_2"
    Then the persisted config should have unpark_from_ap_position "ap_park_2"

  Scenario: SetUnparkFromApPosition applies on the next fresh-power-up connect
    Given a running star-adventurer service
    When I connect the device
    And I run the SetUnparkFromApPosition action with "ap_park_3"
    And I disconnect the device
    And I connect the device
    Then the mount should have received an encoder seed on both axes

  Scenario: SupportedActions advertises the driver-specific actions
    Given a running star-adventurer service
    Then SupportedActions should include "SetUnparkFromApPosition", "SetPreferredApPark", and "UnparkFromApPosition"

  Scenario: An action name is matched without regard to case
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I run the action "unparkfromapposition" with "ap_park_0"
    Then AtPark should be false

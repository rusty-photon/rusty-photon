Feature: Config file bootstrap at startup
  Each start makes sure every device has an ASCOM UniqueID in the config
  file: a device without one gets a freshly minted UUIDv4, written back to
  the file so every later start reads the same id. A device section the file
  leaves out entirely is first filled in from the driver's defaults, so a
  hand-written file holding only the serial and server sections loads.

  The driver writes the file only when the result is a configuration it
  loads. A file it refuses is left byte for byte as it was written, and the
  start fails with an error that names the problem. A file that parses as
  JSON but is not a valid configuration says exactly that, so that "not
  valid JSON" is kept for a syntax error.

  Scenario: A config file holding only the serial and server sections gains both device sections
    Given a config file holding only the serial and server sections
    When the driver starts with the config file
    Then the driver is serving
    And the config file's "switch" section has the name "Pegasus PPBA Switch"
    And the config file's "switch" section has the description "Pegasus Astro PPBA Gen2 Power Control"
    And the config file's "switch" section has a minted UUIDv4 unique_id
    And the config file's "observingconditions" section has the name "Pegasus PPBA Weather"
    And the config file's "observingconditions" section has the description "Pegasus Astro PPBA Environmental Sensors"
    And the config file's "observingconditions" section has a minted UUIDv4 unique_id

  Scenario: A config file with an unknown key is refused and left unchanged
    Given a config file holding only the serial and server sections
    And the config file's serial section carries the unknown key "baud"
    When the driver starts with the config file
    Then the driver refuses to start
    And the start error contains "is valid JSON but not a valid configuration"
    And the start error contains "unknown field `baud`"
    And the config file is byte for byte as it was written

  Scenario: A device section without its name is refused and left unchanged
    Given a config file holding only the serial and server sections
    And the config file has an empty "switch" section
    When the driver starts with the config file
    Then the driver refuses to start
    And the start error contains "is valid JSON but not a valid configuration"
    And the start error contains "missing field `name`"
    And the config file is byte for byte as it was written

  Scenario: A config file cut off mid-section is reported as not valid JSON and left unchanged
    Given a config file that ends in the middle of the serial section
    When the driver starts with the config file
    Then the driver refuses to start
    And the start error contains "is not valid JSON"
    And the config file is byte for byte as it was written

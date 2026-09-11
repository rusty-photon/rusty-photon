Feature: Switch Metadata
  As an ASCOM client
  I want to query switch device properties
  So that I can understand the device capabilities

  Switch names are the one part of the table an operator owns. `switch.labels`
  in the config replaces the built-in name of any of ids 0-4, the connectors
  on the box. Auto-Dew is a mode rather than a connector and the read-only
  rows are physical quantities, so neither can be labelled. Descriptions never
  change, so a labelled switch is still identifiable as the output it is.

  Scenario: Device static name from config
    Given a running PPBA server with switch name "Test PPBA"
    Then the switch device static name should be "Test PPBA"

  Scenario: Device unique ID from config
    Given a running PPBA server with switch unique ID "custom-id-123"
    Then the switch device unique ID should be "custom-id-123"

  Scenario: Device description from config
    Given a running PPBA server with switch description "Custom description"
    When I connect the switch device
    Then the switch device description should be "Custom description"

  Scenario: Device driver info contains PPBA
    Given a running PPBA server with the switch connected
    Then the switch device driver info should contain "PPBA"

  Scenario: Device driver version is not empty
    Given a running PPBA server with the switch connected
    Then the switch device driver version should not be empty

  Scenario: Max switch returns 16
    Given a running PPBA server
    Then the switch device max switch should be 16

  Scenario: All switches have names
    Given a running PPBA server with the switch connected
    Then all 16 switches should have non-empty names

  Scenario: All switches have descriptions
    Given a running PPBA server with the switch connected
    Then all 16 switches should have non-empty descriptions

  Scenario: Switch info consistency for all switches
    Given a running PPBA server with the switch connected
    Then all switches should have min less than max and positive step

  Scenario: Boolean switch min is 0 max is 1
    Given a running PPBA server with the switch connected
    Then switch 0 min value should be 0.0
    And switch 0 max value should be 1.0

  Scenario: PWM switch min is 0 max is 255
    Given a running PPBA server with the switch connected
    Then switch 2 min value should be 0.0
    And switch 2 max value should be 255.0

  Scenario: All switches have positive step
    Given a running PPBA server with the switch connected
    Then all 16 switches should have positive step values

  Scenario: Switch 15 is valid boundary
    Given a running PPBA server with the switch connected
    Then switch 15 name should be queryable

  Scenario: Switch 16 is invalid boundary
    Given a running PPBA server with the switch connected
    Then querying switch 16 name should fail

  Scenario Outline: An operator label replaces the built-in name of a connector
    Given a running PPBA server with the switch connected and these operator labels
      | switch          | label                |
      | Quad 12V Output | Mount and camera rail |
      | USB Hub         | Guide camera hub     |
    Then switch <id> name should be "<name>"

    Examples: the labelled connectors
      | id | name                  |
      | 0  | Mount and camera rail |
      | 4  | Guide camera hub      |

    Examples: switches left unlabelled keep the published name
      | id | name              |
      | 1  | Adjustable Output |
      | 5  | Auto-Dew          |
      | 12 | Temperature       |

  Scenario: A label leaves the description naming the physical output
    Given a running PPBA server with the switch connected and these operator labels
      | switch          | label                 |
      | Quad 12V Output | Mount and camera rail |
    Then switch 0 name should be "Mount and camera rail"
    And switch 0 description should be "Controls the quad 12V power output"

  Scenario: set_switch_name is not implemented
    Given a running PPBA server
    When I try to set switch 0 name to "New Name"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Device info methods return non-empty values
    Given a running PPBA server
    Then the switch device static name should not be empty
    And the switch device unique ID should not be empty

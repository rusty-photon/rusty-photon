Feature: Status Switch device
  A second ASCOM device on the same Alpaca server exposes two read-only
  switches the Rotator interface has no slot for: id 0 reports the Falcon's
  input voltage as a raw ADC count from VS (scale calibration deferred), id 1
  reports FA.limit_detect as a boolean. Both are synchronous (CanAsync
  false): SetAsync, SetAsyncValue and StateChangeComplete answer
  NOT_IMPLEMENTED (1024), CancelAsync, which ISwitchV3 makes mandatory,
  succeeds as a no-op, and every async member rejects id 2 with
  INVALID_VALUE (1025).

  Scenario: MaxSwitch reports the two read-only switches
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read MaxSwitch
    Then MaxSwitch should be 2

  Scenario: Neither switch is writable
    Given a running pa-falcon-rotator service
    When I connect the status switch
    Then CanWrite for id 0 should be false
    And CanWrite for id 1 should be false

  Scenario: GetSwitchValue for id 0 returns the raw VS ADC count
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And the device reports raw voltage 812
    And I read GetSwitchValue for id 0
    Then the switch value should be 812.0

  Scenario: GetSwitch for id 0 is true when the raw value is above zero
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And the device reports raw voltage 812
    And I read GetSwitch for id 0
    Then the switch boolean should be true

  Scenario: GetSwitch for id 1 mirrors the device's limit_detect flag
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And the device's limit_detect is true
    And I read GetSwitch for id 1
    Then the switch boolean should be true

  Scenario: GetSwitchValue for id 1 is 1.0 when limit_detect is set
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And the device's limit_detect is true
    And I read GetSwitchValue for id 1
    Then the switch value should be 1.0

  Scenario Outline: SetSwitch on either switch is rejected with NOT_IMPLEMENTED
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I call SetSwitch on id <id> with true
    Then the set should fail with code 1024

    Examples:
      | id |
      | 0  |
      | 1  |

  # Per-id metadata pins, mirroring the design doc's Switch layout table:
  # id 0 -> "Input Voltage (raw)", numeric, range [0, 1023] step 1
  # id 1 -> "Limit Hit",            boolean, range [0,    1] step 1

  Scenario: Switch id 0 advertises its name
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read GetSwitchName for id 0
    Then the switch name should be "Input Voltage (raw)"

  Scenario: Switch id 1 advertises its name
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read GetSwitchName for id 1
    Then the switch name should be "Limit Hit"

  Scenario: Switch id 0 advertises its description
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read GetSwitchDescription for id 0
    Then the switch description should mention "voltage"

  Scenario: Switch id 1 advertises its description
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read GetSwitchDescription for id 1
    Then the switch description should mention "limit"

  Scenario Outline: Switch ranges and step size match the design contract
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read MinSwitchValue for id <id>
    Then the switch value should be <min>
    When I read MaxSwitchValue for id <id>
    Then the switch value should be <max>
    When I read SwitchStep for id <id>
    Then the switch value should be <step>

    Examples:
      | id | min | max    | step |
      | 0  | 0.0 | 1023.0 | 1.0  |
      | 1  | 0.0 | 1.0    | 1.0  |

  Scenario Outline: Reads on an out-of-range switch id are rejected with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I read <method> for id 2
    Then the switch read should fail with code 1025

    Examples:
      | method                |
      | GetSwitchName         |
      | GetSwitchDescription  |
      | GetSwitchValue        |
      | GetSwitch             |
      | MinSwitchValue        |
      | MaxSwitchValue        |
      | SwitchStep            |
      | CanWrite              |

  Scenario Outline: Both switches report CanAsync false
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I call CanAsync on switch id <id>
    Then the switch boolean should be false

    Examples:
      | id |
      | 0  |
      | 1  |

  Scenario Outline: SetAsync, SetAsyncValue and StateChangeComplete are rejected with NOT_IMPLEMENTED
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I call <member> on switch id <id>
    Then the operation should fail with code 1024

    Examples:
      | member              | id |
      | SetAsync            | 0  |
      | SetAsync            | 1  |
      | SetAsyncValue       | 0  |
      | SetAsyncValue       | 1  |
      | StateChangeComplete | 0  |
      | StateChangeComplete | 1  |

  Scenario Outline: CancelAsync is mandatory and succeeds as a no-op
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I call CancelAsync on switch id <id>
    Then the async call should succeed

    Examples:
      | id |
      | 0  |
      | 1  |

  Scenario Outline: The async members reject an out-of-range switch id with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the status switch
    And I call <member> on switch id 2
    Then the operation should fail with code 1025

    Examples:
      | member              |
      | CanAsync            |
      | SetAsync            |
      | SetAsyncValue       |
      | StateChangeComplete |
      | CancelAsync         |

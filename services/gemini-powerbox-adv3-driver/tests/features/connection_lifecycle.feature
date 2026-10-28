Feature: Connection lifecycle
  The driver validates the hub before it serves it, and the validation is
  read-only: an identity request, a firmware request, one status frame.
  Nothing on the connect path switches an output or touches a heater.

  The identity reply must be the Advanced 3's, compared verbatim: another
  Gemini power box, Gemini's flat panel, or a device that answers without
  the Gemini prefix at all is refused by name, and the service does not
  start. A first identity request that draws no reply is asked once more
  after two seconds. Firmware below 3.0.8 is refused, naming the version
  read and the vendor's upgrade tool; 3.0.8 and later connect. A hub left
  streaming by an earlier session cuts a frame short to answer, and the
  reply arrives glued to that frame; the driver reads the reply all the
  same.

  Each ASCOM device starts disconnected, connects and disconnects
  idempotently, and survives repeated cycles.

  Scenario: Switch device starts disconnected
    Given a running Gemini hub server
    Then the switch device should report disconnected

  Scenario: Switch device connects successfully
    Given a running Gemini hub server
    When I connect the switch device
    Then the switch device should report connected

  Scenario: Switch device disconnects successfully
    Given a running Gemini hub server
    When I connect the switch device
    And I disconnect the switch device
    Then the switch device should report disconnected

  Scenario: Switch device connect is idempotent
    Given a running Gemini hub server
    When I connect the switch device
    And I connect the switch device
    Then the switch device should report connected

  Scenario: Switch device disconnect is idempotent
    Given a running Gemini hub server
    When I connect the switch device
    And I disconnect the switch device
    And I disconnect the switch device
    Then the switch device should report disconnected

  Scenario: Switch device survives multiple connect-disconnect cycles
    Given a running Gemini hub server
    When I cycle the switch device connection 5 times
    Then the switch device should report disconnected

  Scenario: OC device starts disconnected
    Given a running Gemini hub server
    Then the OC device should report disconnected

  Scenario: OC device connects successfully
    Given a running Gemini hub server
    When I connect the OC device
    Then the OC device should report connected

  Scenario: OC device disconnects successfully
    Given a running Gemini hub server
    When I connect the OC device
    And I disconnect the OC device
    Then the OC device should report disconnected

  Scenario: OC device connect is idempotent
    Given a running Gemini hub server
    When I connect the OC device
    And I connect the OC device
    Then the OC device should report connected

  Scenario: OC device disconnect is idempotent
    Given a running Gemini hub server
    When I connect the OC device
    And I disconnect the OC device
    And I disconnect the OC device
    Then the OC device should report disconnected

  Scenario: Connecting leaves every output as the hub was found
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch 1 boolean should be true
    And switch 4 boolean should be true
    And switch 5 boolean should be true
    And switch 10 boolean should be true
    And switch 11 value should be 100.0
    And switch 15 value should be 1.0

  Scenario: Firmware 3.0.9 connects
    Given a running Gemini hub server with firmware 309 and the switch connected
    Then the switch device should report connected

  Scenario: Firmware 3.0.8, the oldest accepted, connects
    Given a running Gemini hub server with firmware 308 and the switch connected
    Then the switch device should report connected

  Scenario: Firmware 3.0.7 is refused naming the version read and the upgrade tool
    When I try to start a Gemini hub server with firmware 307
    Then the server refuses to start
    And the refusal names "3.0.7"
    And the refusal names "FirmwareUpgradeTool"

  Scenario: Gemini's flat panel on the port is refused by name
    When I try to start a Gemini hub server whose identity reply is "GeminiFlatPanelV3"
    Then the server refuses to start
    And the refusal names "flat panel"
    And the refusal names "GeminiFlatPanelV3"

  Scenario: Another Gemini power box is refused as not the Advanced 3
    When I try to start a Gemini hub server whose identity reply is "GeminiPowerBoxMiniV2"
    Then the server refuses to start
    And the refusal names "not the Advanced 3"

  Scenario: An unknown identity is quoted as it answered
    When I try to start a Gemini hub server whose identity reply is "SomethingElse"
    Then the server refuses to start
    And the refusal names "*HSomethingElse#"

  Scenario: A device that never sends a Gemini identity is reported as no identity reply on the port
    When I try to start a Gemini hub server whose identity reply is "raw:HELLO#"
    Then the server refuses to start
    And the refusal names "no Gemini identity reply on /dev/mock"

  Scenario: An ignored first identity request is asked once more and the hub connects
    Given a running Gemini hub server with an ignored first identity request and the switch connected
    Then the switch device should report connected

  Scenario: A hub still streaming from an earlier session connects, its cut-short frames notwithstanding
    Given a running Gemini hub server with a hub still streaming from an earlier session and cutting frames short to answer and the switch connected
    When I wait for the switch data to be available
    Then the switch device should report connected
    And switch 17 value should be 12.2
    When I set switch 2 boolean to false
    Then switch 2 boolean should be false

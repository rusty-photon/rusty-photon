@serial
Feature: Focuser enumeration and connection lifecycle
  zwo-focuser enumerates every connected ZWO EAF at startup and registers
  each as an ASCOM Focuser device, index 0, 1, 2, ..., on one port (C0).
  Each device's UniqueID is derived from its SDK serial; because
  EAFGetSerialNumber requires an open focuser, enumeration opens each
  focuser briefly to read the serial, and caches its EAF_INFO (name) and
  working travel limit (EAFGetMaxStep) from the same open. Connecting a
  device opens its focuser (C1), and connect is per-device (C4). An open
  failure leaves the device not connected (C2).
  Disconnect closes the device (C3). With zero focusers discovered the
  service still starts, registering no Focuser devices and logging a
  warning. Against the zwo-rs simulation backend exactly one focuser
  (EAF-Simulated, working travel limit 60000, EAF_INFO ceiling 600000)
  is present.

  Background:
    Given the zwo-focuser service running with the simulation backend

  Scenario: The simulated focuser is registered as device 0
    Then ASCOM focuser device 0 is available
    And focuser device 0 reports a non-empty UniqueID

  Scenario: A focuser starts disconnected
    Then focuser device 0 reports Connected as false

  Scenario: Connecting opens the focuser
    When I connect focuser device 0
    Then focuser device 0 reports Connected as true

  Scenario: Disconnecting leaves the focuser not connected
    When I connect focuser device 0
    And I disconnect focuser device 0
    Then focuser device 0 reports Connected as false

  Scenario: The service starts with no Focuser devices when no focuser is present
    Given the zwo-focuser service running with an empty simulation backend
    Then no ASCOM focuser devices are registered
    And the service is healthy

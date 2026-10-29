Feature: A reload keeps its clients
  A setup-page save, SIGHUP and the Windows service's ParamChange each
  reload the service in process, and the reload keeps the safety
  monitor's clients: to a client, a reload is a reply that comes a second
  or two late. While server.port, server.bind_address and server.tls are
  unchanged the server keeps accepting and closes no connection: the
  reload puts the rebuilt server in front of every connection, so a
  kept-alive connection's next request is answered by it. A request the
  old server took before then is answered by the old server, whose device
  stays connected until the last of those is answered. When the device
  was connected before the reload, the rebuilt one is connected before
  it answers: it re-reads the watched file, so a client's session
  survives the reload and its next read is the new configuration's
  reading. A device that was not connected stays disconnected, and so
  does one whose new file.path does not exist: its reads fail with
  NotConnected, as after a lost session. A changed address closes the
  old socket before the new one is bound, so a connection to the old
  port is refused. A changed address or server.tls starts the accepting
  afresh, and the old connections are closed once their requests are
  answered. A TLS handshake still in progress holds up no reload.

  Scenario: A connected client's session survives a save and reads the new configuration
    Given a monitoring file containing "Roof CLOSED"
    And a second monitoring file containing "Roof OPEN"
    And a contains rule with pattern "CLOSED" that evaluates to safe
    And a contains rule with pattern "OPEN" that evaluates to unsafe
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true
    When I submit the server setup page pointing "file.path" at the second monitoring file
    And the rebuilt server answers its setup page
    Then the device should be connected
    And is_safe should return false

  Scenario: A kept-alive connection stays open through a reload and reads the new configuration
    Given a monitoring file containing "Roof CLOSED"
    And a second monitoring file containing "Roof OPEN"
    And a contains rule with pattern "CLOSED" that evaluates to safe
    And a contains rule with pattern "OPEN" that evaluates to unsafe
    And filemonitor is running with these rules
    When I connect the device
    And a client opens a kept-alive connection
    Then is_safe read over the kept-alive connection returns true
    When I submit the server setup page pointing "file.path" at the second monitoring file
    And the rebuilt server answers its setup page
    Then is_safe read over the kept-alive connection returns false

  Scenario: Every read a client makes through a reload is answered
    Given a monitoring file containing "Roof CLOSED"
    And a contains rule with pattern "CLOSED" that evaluates to safe
    And filemonitor is running with these rules
    When I connect the device
    And a client reads is_safe every 10 milliseconds
    And I submit the server setup page setting "file.polling_interval" to "30s"
    And the rebuilt server answers its setup page
    Then every read the client made through the reload returned true

  Scenario: A device that was not connected stays disconnected through a reload
    Given a monitoring file containing "Roof CLOSED"
    And filemonitor is running
    When I submit the server setup page setting "file.polling_interval" to "30s"
    And the rebuilt server answers its setup page
    Then the device should be disconnected

  Scenario: A reload to a file path that does not exist leaves the device disconnected
    Given a monitoring file containing "Roof CLOSED"
    And a contains rule with pattern "CLOSED" that evaluates to safe
    And filemonitor is running with these rules
    When I connect the device
    And I submit the server setup page setting "file.path" to "/nonexistent/path/file.txt"
    And the rebuilt server answers its setup page
    Then the device should be disconnected
    And is_safe should fail with a not connected error

  Scenario: A changed port refuses connections on the old one
    Given a monitoring file containing "Roof CLOSED"
    And filemonitor is running
    When I connect the device
    And I submit the server setup page moving "server.port" to a free port
    And the rebuilt server answers its setup page
    Then a connection to the port the service served before is refused

  Scenario: A TLS handshake still in progress does not hold up a reload
    Given filemonitor is running with TLS and auth
    And a client has started a TLS handshake and stalled
    When I submit the server setup page setting "file.polling_interval" to "30s"
    And the rebuilt server answers its setup page
    Then the stalled handshake is still open

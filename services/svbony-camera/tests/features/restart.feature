Feature: Restart on request
  Svbony-camera restarts when a client asks it to.

  A POST to /service/v1/restart with an empty JSON body is accepted with
  202, naming the process that is restarting, the one GET
  /service/v1/instance names. The service stops as a shutdown stops it,
  closing its devices, and exits with code 75, the code its service
  manager starts it again on.

  Scenario: A restart request ends the service with the restart code
    Given a running svbony-camera service with the simulation backend
    When a client asks the service to restart
    Then the service accepts the restart request with status 202, naming its own process
    And the service exits with code 75

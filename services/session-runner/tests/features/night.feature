Feature: Night schedule
  With a night block in its configuration, session-runner keeps its place
  in the rig's night in night.json and reports it on GET /night. It reads
  each night's civil twilight from rp: dusk is the evening end of civil
  twilight plus dusk_offset, and dawn the morning end plus dawn_offset.
  The schedule stays on a night until its dawn, in phase day until dusk.
  Without a night block, GET /night reports phase off and nothing else.

  At dusk, once the roof reads safe, the schedule has rp power the daily
  outputs up, checks each camera for a wedge, opens the covers, starts
  the cooldown and starts the configured runs in turn, each under the
  session id night-<date>-<n>.

  Dawn runs every morning, a start in the day included. It stops the
  active run, warms the cameras, parks the mount, closes the covers the
  power map lists, waits for the coolers to stop, and has rp cut every
  daily output the cut rules allow. With nothing left on, night.json
  moves to the night ahead.

  The night block is checked when the service starts: a run whose
  document does not resolve in workflows_dir fails the start.

  Scenario: Without a night block the schedule is off
    Given session-runner is running without a night block
    When the night schedule's place is read
    Then the night schedule should report the JSON {"phase": "off"}

  Scenario: The schedule reads the night ahead from rp's civil twilight
    Given rp is running with no devices at a site whose dawn has passed
    And session-runner is running with a night block whose dusk_offset is "-30m"
    When the night schedule has read the night ahead
    Then the night schedule's phase should be "day"
    And the night schedule's dusk should be that night's civil twilight begin moved by -30 minutes
    And the night schedule's dawn should be that night's civil twilight end
    And the night schedule's dawn should be in the future
    And night.json should name the night, dusk and dawn the schedule reports

  Scenario: A night block whose run names no document fails the start
    Given rp is running with a site at latitude 47.6 and longitude -122.3
    When session-runner is started with a night block whose run names the workflow "no_such_document"
    Then session-runner should fail to start

  @serial
  Scenario: At a site whose dawn has passed, the schedule parks the mount and cuts the daily output
    Given a stub power box whose daily output "Camera Power" feeds the simulator's camera
    And rp is running at an observing site whose dawn has passed
    When session-runner starts with a night block
    Then the stub power box's "Camera Power" output should go off
    And rp should report the mount parked
    And the night schedule should be on the night ahead in phase "day"

  @serial
  Scenario: Dawn stops the run the previous process left
    Given a stub power box whose daily output "Camera Power" feeds the simulator's camera
    And rp is running at an observing site whose dawn has passed
    And a run of "night_long_wait" left in the state directory by the previous process
    When session-runner starts with a night block
    Then the run the previous process left should end "stopped"
    And the stub power box's "Camera Power" output should go off

  @serial
  Scenario: At a site whose dusk has passed, the schedule powers the rig up and starts the night's run
    Given a stub power box whose daily output "Camera Power" feeds the simulator's camera and reads off
    And rp is running at an observing site whose dusk has passed
    When session-runner starts with a night block that runs "night_short"
    Then the stub power box's "Camera Power" output should go on
    And the night's run of "night_short" should complete under the session id of its night and index 0
    And the night schedule should reach phase "runs"

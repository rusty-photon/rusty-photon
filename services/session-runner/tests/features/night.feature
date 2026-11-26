Feature: Night schedule
  With a night block in its configuration, session-runner keeps its place
  in the rig's night in night.json and reports it on GET /night. It reads
  each night's civil twilight from rp: dusk is the evening end of civil
  twilight plus dusk_offset, and dawn the morning end plus dawn_offset.
  The schedule stays on a night until its dawn, in phase day until dusk.
  Without a night block, GET /night reports phase off and nothing else.

  The night block is checked when the service starts: a run whose
  document does not resolve in workflows_dir fails the start.

  Scenario: Without a night block the schedule is off
    Given session-runner is running without a night block
    When the night schedule's place is read
    Then the night schedule should report the JSON {"phase": "off"}

  Scenario: The schedule reads the night ahead from rp's civil twilight
    Given rp is running with a site at latitude 47.6 and longitude -122.3
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

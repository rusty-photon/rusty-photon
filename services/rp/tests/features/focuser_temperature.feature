@serial
Feature: Focuser temperature watch
  rp polls the temperature probe of every connected focuser on a slow
  cadence (equipment.temperature_poll_interval, production default 30s;
  these scenarios shorten it) and emits a "temperature_changed" event
  with the focuser's config id as "sensor" and the reading in °C as
  "value" once the probe has moved by at least
  equipment.temperature_event_delta_c (default 0.5) since the last
  emission. The first reading through a session seeds the baseline
  silently; a drift below the delta leaves the baseline alone, so the
  delta accumulates across polls instead of resetting at each one. A
  focuser whose Temperature property is not implemented never emits, a
  failed read keeps the baseline, and a re-established session starts
  over from its first reading. Reading a probe moves nothing: the event
  is the whole output, and the session document decides what to do
  with it.

  The probe is an in-process Alpaca stub focuser whose reading the
  scenario scripts and whose served reads it counts, so every step
  waits on rp having polled again rather than on a clock.

  Scenario: A probe drifting past the delta emits once, measured from the last emission
    Given a stub Alpaca service hosting a focuser reporting 10.0 °C
    And rp is configured with a focuser on the stub service
    And a focuser temperature poll interval of 200 milliseconds and an event delta of 0.5 °C
    And a test webhook receiver subscribed to "temperature_changed"
    When rp starts
    # The first reading seeds the baseline; nothing is emitted for it.
    And the watch has sampled the stub focuser at least 2 more times
    And the stub focuser reports 10.3 °C
    # 0.3 below the delta: quiet, and the baseline stays at 10.0.
    And the watch has sampled the stub focuser at least 2 more times
    And the stub focuser reports 10.6 °C
    # 0.6 from the 10.0 baseline (only 0.3 from the last poll): emits.
    Then the first "temperature_changed" event should name sensor "stub-focuser" at 10.6 °C
    And the watch has sampled the stub focuser at least 2 more times
    And exactly 1 "temperature_changed" event should have been received

  Scenario: A failed probe read keeps the baseline
    Given a stub Alpaca service hosting a focuser reporting 10.0 °C
    And rp is configured with a focuser on the stub service
    And a focuser temperature poll interval of 200 milliseconds and an event delta of 0.5 °C
    And a test webhook receiver subscribed to "temperature_changed"
    When rp starts
    And the watch has sampled the stub focuser at least 2 more times
    And the stub focuser's probe starts failing
    And the watch has sampled the stub focuser at least 2 more times
    And the stub focuser reports 10.6 °C
    # Had the failures dropped the baseline, 10.6 would have seeded a
    # new one silently and no event would ever arrive.
    Then the first "temperature_changed" event should name sensor "stub-focuser" at 10.6 °C

  Scenario: A focuser without a temperature probe never emits
    Given a stub Alpaca service hosting a focuser without a temperature probe
    And rp is configured with a focuser on the stub service
    And a focuser temperature poll interval of 200 milliseconds and an event delta of 0.5 °C
    And a test webhook receiver subscribed to "temperature_changed"
    When rp starts
    And the watch has sampled the stub focuser at least 3 more times
    Then the test webhook receiver should not have received any events

  Scenario: A re-established session seeds a fresh baseline from its first reading
    Given a stub Alpaca service hosting a focuser reporting 10.0 °C
    And rp is configured with a focuser on the stub service
    And an equipment reconnect interval of 500 milliseconds
    And a focuser temperature poll interval of 200 milliseconds and an event delta of 0.5 °C
    And a test webhook receiver subscribed to "temperature_changed" and "equipment_changed"
    When rp starts
    And the watch has sampled the stub focuser at least 2 more times
    And the stub Alpaca service stops
    And the stub focuser reports 25.0 °C
    And the stub Alpaca service comes back with its session state lost
    Then an "equipment_changed" event should report the device "stub-focuser" as connected
    # The first reading through the new session (25.0) seeds silently;
    # against the old 10.0 baseline it would have emitted.
    And the watch has sampled the stub focuser at least 2 more times
    When the stub focuser reports 25.6 °C
    Then the first "temperature_changed" event should name sensor "stub-focuser" at 25.6 °C
    And exactly 1 "temperature_changed" event should have been received

Feature: Connection lifecycle
  The driver opens its transport at service startup and runs the
  initialisation handshake there, before the HTTP listener binds — a
  wrong device or an unreachable mount fails the process rather than
  advertising a broken one. `Connected = true` then acquires a session on
  the already-open transport, a refcount bump that puts no handshake on
  the wire; concurrent clients share the one transport. The port stays
  open across every disconnect and closes at service shutdown.

  Disconnect still aborts motion in progress and stops tracking, because
  that is the state the mount should be left in with nobody attached.
  Startup is that same no-client state reached from the other end, so the
  driver halts the mount on the way up for the reason it halts it on the
  way down.

  Scenario: Device starts disconnected
    Given a running star-adventurer service
    Then the device should be disconnected

  Scenario: Device connects successfully after handshake
    Given a running star-adventurer service
    When I connect the device
    Then the device should be connected

  Scenario: Startup runs the initialisation handshake in order
    The handshake belongs to service startup, not to Connected = true: the
    port opens eagerly at start and a later connect is a refcount bump on
    the already-open transport. The :e1 motor-board-version inquiry runs
    first so the driver can refuse to send mount-specific init commands to
    a device that isn't a Sky-Watcher motor controller. See issue #254.
    Given a running star-adventurer service
    Then the mount should have received startup commands in order:
      | command |
      | :e1     |
      | :F1     |
      | :F2     |
      | :a1     |
      | :a2     |
      | :b1     |
      | :g1     |
      | :g2     |
      | :j1     |
      | :j2     |

  Scenario: Startup halts the mount
    A driver does not get to assume the device it has just opened is idle.
    A reload builds a new transport and a restart keeps nothing at all, so
    a halt the previous lifecycle could not land is not remembered by
    anything in this one — but the mount is still doing whatever it was
    doing. The startup handshake is therefore followed by the same
    :L1, :L2, :K1 sequence a last-client disconnect issues. That the halt
    also precedes the *publish* — so no client can command the mount ahead
    of it — is not observable from here, since reading this log needs the
    listener the halt runs before; it is pinned in the shared crate by
    `a_cold_start_whose_safety_stop_misses_the_wire_does_not_serve`.
    See issue #1251.
    Given a running star-adventurer service
    Then the mount should have received startup commands in order:
      | command |
      | :j2     |
      | :L1     |
      | :L2     |
      | :K1     |

  Scenario: A reload halts the mount again on its fresh conduit
    A reload tears the transport down and builds a new one, so the mount
    sees a second startup over the same link: the :F1 initialisation runs
    again, and the :L1, :L2, :K1 halt runs again behind it. That second
    halt is the one that matters — a shutdown whose link was already dead
    cannot land its own, and nothing in the new lifecycle remembers that
    it did not. See issue #1251.
    Given a running star-adventurer service
    When config.apply pins the bound port and sets the mount description to "Reloaded Mount"
    Then the reloaded service serves mount description "Reloaded Mount"
    And the mount should have been initialised and halted a second time

  Scenario: Startup populates the parameter cache from handshake replies
    The cache is seeded by the startup handshake, not by a connect: the
    :a1 / :a2 / :b1 inquiries run once at start and a later
    Connected = true reuses what they cached.
    Given a mount that reports CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And a mount that reports timer frequency 16000000
    And a running star-adventurer service
    Then the parameter cache should report CPR 3628800 on the RA axis
    And the parameter cache should report CPR 2903040 on the Dec axis
    And the parameter cache should report timer frequency 16000000

  Scenario: Disconnect after connect releases the transport
    Given a running star-adventurer service
    When I connect the device
    And I disconnect the device
    Then the device should be disconnected

  Scenario: Concurrent connects share one transport
    Given a running star-adventurer service
    When two clients connect the device
    Then the underlying transport should have been opened exactly once

  # Multi-client disconnect ref-counting is unit-tested at
  # `services/star-adventurer-gti/src/transport_manager.rs::tests::
  # connect_is_reference_counted` because BDD against a single-process
  # binary cannot drive two distinct ASCOM client sessions through one
  # device instance. Keeping the description here so a reader has a
  # pointer to the assertion.
  Scenario: Last disconnect tears the transport down
    Given a running star-adventurer service
    When I connect the device
    And I disconnect the device
    Then the device should be disconnected

  Scenario: Disconnect aborts motion in progress
    Given a running star-adventurer service
    When I connect the device
    And the mount is slewing
    And I disconnect the device
    Then the mount should have received command :L1
    And the mount should have received command :L2

//! Thin manager wrapping `SharedTransport<FalconCodec>` plus the
//! per-driver state both ASCOM devices read from.
//!
//! The refcount, slot, open/close transitions, command-lock arbitration,
//! and the (here unused) while-open task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The Falcon handshake (`F#` → `FV` → `DR:0` → `FA` → `VS`, with the
//!   `FA` / `VS` results discarded — the no-cache design pulls a fresh
//!   read for every property access).
//! * The three small pieces of driver-side state pinned by the design doc:
//!   [`sync_offset`](FalconManager::sync_offset) (ASCOM `Sync` offset,
//!   driver-side per the
//!   [Sync semantics](../../../docs/services/falcon-rotator.md#sync-semantics--why-driver-side-not-sd)
//!   section); [`target_position`](FalconManager::target_position) (the
//!   sky-coordinate target of the last `Move*`); and the
//!   `last_limit_detected` edge tracker used to fire a one-shot `warn!`
//!   on the rising edge of `FA.limit_detect`. The offset is kept in the
//!   [sync-offset record](crate::sync_record) when the manager has one.
//! * The count of clients connected across both devices, behind
//!   [`acquire`](FalconManager::acquire) and
//!   [`release`](FalconManager::release): the release that leaves none
//!   clears the stored target and the edge tracker.
//! * The protocol surface (`read_status`, `read_voltage_raw`,
//!   `move_mechanical`, `halt`, `set_reverse`, `sync`) that the device
//!   types call through their held `&Session<FalconCodec>`.
//!
//! No while-open task: the Falcon driver issues a fresh wire command on
//! every property read and never polls in the background. `Hooks::while_open`
//! is `None`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SharedTransport, StateAssertion, TransportFactory,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::codec::{FalconCodec, FalconCodecError, FalconResponse};
use crate::error::{FalconRotatorError, Result};
use crate::protocol::{validate_echo, Command, FalconStatus};
use crate::sync_record::{self, ReadRecord};
use crate::units::{MechanicalDegrees, SkyDegrees, SyncOffset};

/// How a manager writes its sync-offset record: [`sync_record::write`],
/// or, in a unit test, a writer the test can pause mid-write.
type RecordWriter = Arc<dyn Fn(&Path, SyncOffset) -> std::io::Result<()> + Send + Sync>;

/// Manager wrapping the shared transport plus driver-side Falcon state.
///
/// One instance per process; both ASCOM devices (`FalconRotatorDevice`
/// and `FalconStatusSwitchDevice`) hold an `Arc<FalconManager>` and each
/// hold their own `Option<Session<FalconCodec>>`.
pub struct FalconManager {
    transport: Arc<SharedTransport<FalconCodec>>,
    sync_offset: Mutex<SyncOffset>,
    /// Where [`Self::sync`] keeps the offset; `None` keeps it in memory
    /// only.
    sync_record: Option<PathBuf>,
    write_record: RecordWriter,
    /// Orders the syncs: held across a sync's record write and the
    /// assignment after it, so the last sync to write is the last to
    /// assign, while `sync_offset` is held only for the assignment and a
    /// reader of the offset never waits on the write.
    sync_writes: Mutex<()>,
    target_position: Mutex<Option<SkyDegrees>>,
    last_limit_detected: Arc<Mutex<Option<bool>>>,
    /// Clients connected across both devices. [`Self::acquire`] and
    /// [`Self::release`] hold it across the session change, so the reset
    /// the last release makes is complete before the next client starts.
    clients: Mutex<usize>,
}

impl FalconManager {
    /// Build a manager around `factory`.
    ///
    /// The Falcon driver has no cached state to seed and no poll interval
    /// to capture, so unlike `PpbaManager` / `FocuserManager` this
    /// constructor does **not** take a `Config` — the only per-call
    /// configuration is the transport factory itself, which already owns
    /// the port path / baud rate / timeout it was built from.
    ///
    /// The sync offset starts at zero and lives in memory only; the
    /// binary builds its manager with [`Self::with_sync_record`].
    pub fn new(factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        Self::build(
            factory,
            None,
            SyncOffset::ZERO,
            Arc::new(sync_record::write),
        )
    }

    /// Build a manager around `factory` whose sync offset is kept in the
    /// record at `path`, starting from the offset the record holds.
    ///
    /// No record means no `Sync` yet, and a record that cannot be read
    /// starts the offset at zero with a `warn!`, the record left for the
    /// next `Sync` to replace (design doc §The sync-offset record).
    pub async fn with_sync_record(factory: Arc<dyn TransportFactory>, path: PathBuf) -> Arc<Self> {
        let read_path = path.clone();
        let read = tokio::task::spawn_blocking(move || sync_record::read(&read_path))
            .await
            .unwrap_or_else(|e| ReadRecord::Unreadable(format!("the read did not finish: {e}")));
        let offset = match read {
            ReadRecord::Absent => {
                debug!(
                    "no sync-offset record at {}: the offset starts at zero",
                    path.display()
                );
                SyncOffset::ZERO
            }
            ReadRecord::Offset(offset) => {
                debug!(
                    "sync offset {:.4} read from {}",
                    offset.value(),
                    path.display()
                );
                offset
            }
            ReadRecord::Unreadable(why) => {
                warn!("the sync-offset record {why}; the offset starts at zero until a Sync replaces it");
                SyncOffset::ZERO
            }
        };
        Self::build(factory, Some(path), offset, Arc::new(sync_record::write))
    }

    fn build(
        factory: Arc<dyn TransportFactory>,
        sync_record: Option<PathBuf>,
        sync_offset: SyncOffset,
        write_record: RecordWriter,
    ) -> Arc<Self> {
        let last_limit_detected = Arc::new(Mutex::new(None));
        let last_for_hooks = Arc::clone(&last_limit_detected);
        let hooks = Hooks {
            handshake: Box::new(move |conn| {
                let last = Arc::clone(&last_for_hooks);
                Box::pin(handshake(conn, last))
            }),
            on_last_disconnect: Box::new(|_| {
                // Nothing to assert: no device state has to hold while no
                // client is attached, so the answer is trivially yes.
                Box::pin(async { StateAssertion::Asserted })
            }),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: None,
        };
        let transport = SharedTransport::new(factory, FalconCodec, hooks);
        Arc::new(Self {
            transport,
            sync_offset: Mutex::new(sync_offset),
            sync_record,
            write_record,
            sync_writes: Mutex::new(()),
            target_position: Mutex::new(None),
            last_limit_detected,
            clients: Mutex::new(0),
        })
    }

    /// Access the shared transport: the server starts and shuts it down.
    pub const fn transport(&self) -> &Arc<SharedTransport<FalconCodec>> {
        &self.transport
    }

    /// Take a session for a device's client, and count the client.
    ///
    /// # Errors
    ///
    /// Returns the transport's refusal to hand out a session.
    pub async fn acquire(&self) -> Result<Session<FalconCodec>> {
        let mut clients = self.clients.lock().await;
        let session = self
            .transport
            .acquire()
            .await
            .map_err(FalconRotatorError::from)?;
        *clients = clients.saturating_add(1);
        drop(clients);
        Ok(session)
    }

    /// Close a device's session, and stop counting its client.
    ///
    /// The release that leaves no client on either device clears the
    /// stored target and the `limit_detect` edge tracker, whatever the
    /// state of the link; the sync offset stays (design doc §Connection
    /// Lifecycle, step 8).
    ///
    /// # Errors
    ///
    /// Returns [`Session::close`]'s error. The session is released and the
    /// client uncounted either way.
    pub async fn release(&self, session: Session<FalconCodec>) -> Result<()> {
        let mut clients = self.clients.lock().await;
        let closed = session.close().await;
        *clients = clients.saturating_sub(1);
        if *clients == 0 {
            *self.target_position.lock().await = None;
            *self.last_limit_detected.lock().await = None;
            debug!("no client left: the stored target and the limit edge are cleared");
        }
        drop(clients);
        closed.map_err(FalconRotatorError::from)
    }

    /// Cheap, non-blocking snapshot — true from a successful open to the
    /// start of a teardown or a reconnect.
    pub fn is_available(&self) -> bool {
        self.transport.is_available()
    }

    /// Issue `FA` on the caller's session, parse it, and perform the
    /// `limit_detect` edge log.
    ///
    /// Logs `warn!` exactly once on the `None → true` or
    /// `Some(false) → true` transition of `FA.limit_detect`. The handshake
    /// — at service start and on every reconnect — initialises the state
    /// to `None`, so a fresh connection whose very first observation
    /// reports `limit_detect = 1` still surfaces the warning.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a
    /// [`FalconRotatorError`] — a transport error, a response the codec
    /// cannot decode (malformed `FA` data included), or an exhausted skip
    /// budget; a decoded frame of the wrong variant is consumed by the
    /// skip budget rather than returned.
    pub async fn read_status(&self, session: &Session<FalconCodec>) -> Result<FalconStatus> {
        let resp = session
            .request(Command::FullStatus)
            .await
            .map_err(FalconRotatorError::from)?;
        let status = match resp {
            FalconResponse::Status(s) => s,
            other => {
                return Err(FalconRotatorError::InvalidResponse(format!(
                    "FA returned non-status frame: {other:?}"
                )));
            }
        };

        let target = *self.target_position.lock().await;
        let mut last = self.last_limit_detected.lock().await;
        let rising_edge = is_limit_detect_rising_edge(*last, status.motion.limit_detect);
        *last = Some(status.motion.limit_detect);
        drop(last);

        if rising_edge {
            warn!(
                "Falcon reported limit_detect after move toward {:?}",
                target
            );
        }

        Ok(status)
    }

    /// Issue `VS` on the caller's session and return the raw ADC count.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a
    /// [`FalconRotatorError`] — a transport error, a response the codec
    /// cannot decode (malformed `VS` data included), or an exhausted skip
    /// budget; a decoded frame of the wrong variant is consumed by the
    /// skip budget rather than returned.
    pub async fn read_voltage_raw(&self, session: &Session<FalconCodec>) -> Result<u32> {
        let resp = session
            .request(Command::Voltage)
            .await
            .map_err(FalconRotatorError::from)?;
        match resp {
            FalconResponse::Voltage(v) => Ok(v),
            other => Err(FalconRotatorError::InvalidResponse(format!(
                "VS returned non-voltage frame: {other:?}"
            ))),
        }
    }

    /// Move to a mechanical angle on the caller's session.
    ///
    /// The caller has already applied any sync offset (the
    /// [`MechanicalDegrees`] type carries a normalised, finite angle by
    /// construction). This method quantises to the `MD:nn.nn` wire precision
    /// — which also re-normalises into `[0, 360)` — emits the command, and
    /// returns the actual wire-quantised mechanical value that was sent.
    ///
    /// Returning the wire-quantised value matters because a near-boundary
    /// input (e.g. `359.999`) becomes `MD:0.00` on the wire; callers that
    /// cache a sky-coordinate `TargetPosition` derive it from this
    /// return value so the cached target matches what the device was
    /// actually told to reach.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a
    /// [`FalconRotatorError`] — a transport error, a response the codec
    /// cannot decode, or an exhausted skip budget — or
    /// [`FalconRotatorError::InvalidResponse`] if the echoed body does not
    /// match the `MD` command sent.
    pub async fn move_mechanical(
        &self,
        session: &Session<FalconCodec>,
        target: MechanicalDegrees,
    ) -> Result<MechanicalDegrees> {
        let wire = target.quantise_to_wire();
        let cmd = Command::MoveDeg(wire);
        let resp = session
            .request(cmd.clone())
            .await
            .map_err(FalconRotatorError::from)?;
        let echo = expect_echo(resp, "MD")?;
        validate_echo(&cmd, &echo)?;
        Ok(wire)
    }

    /// Issue `FH`, validate the `FH:1` echo, and clear the stored target.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a
    /// [`FalconRotatorError`] — a transport error, a response the codec
    /// cannot decode, or an exhausted skip budget — or
    /// [`FalconRotatorError::InvalidResponse`] if the echoed body is not
    /// `FH:1`; the stored target is left in place in either case.
    pub async fn halt(&self, session: &Session<FalconCodec>) -> Result<()> {
        let cmd = Command::Halt;
        let resp = session
            .request(cmd.clone())
            .await
            .map_err(FalconRotatorError::from)?;
        let echo = expect_echo(resp, "FH")?;
        validate_echo(&cmd, &echo)?;
        *self.target_position.lock().await = None;
        Ok(())
    }

    /// Read `FA` then write `FN:b` iff the device's `motor_reverse` differs.
    ///
    /// EEPROM-wear protection (design doc Reverse semantics): the Falcon
    /// persists `FN:b` to EEPROM on every write, so we read first and
    /// skip the write when the device already reports the requested value.
    ///
    /// # Errors
    ///
    /// Returns [`Self::read_status`]'s failure, or — when the write is
    /// needed — the `FN` request's [`Session::request`] failure as a
    /// [`FalconRotatorError`], or [`FalconRotatorError::InvalidResponse`] if
    /// the echoed body does not match the `FN` command sent.
    pub async fn set_reverse(&self, session: &Session<FalconCodec>, want: bool) -> Result<()> {
        let current = self.read_status(session).await?.settings.motor_reverse;
        if current == want {
            debug!(
                "set_reverse({}): device already matches, skipping FN write",
                want
            );
            return Ok(());
        }
        let cmd = Command::SetReverse(want);
        let resp = session
            .request(cmd.clone())
            .await
            .map_err(FalconRotatorError::from)?;
        let echo = expect_echo(resp, "FN")?;
        validate_echo(&cmd, &echo)?;
        Ok(())
    }

    /// Driver-side sync: store `(sky_deg - mech) mod 360` in `sync_offset`.
    ///
    /// Per the design doc Sync semantics, ASCOM `Sync` must leave
    /// `MechanicalPosition` unchanged, so the offset lives in the driver
    /// and the Falcon's `SD` command is never issued. With a sync-offset
    /// record, the record is replaced before the offset is put in force.
    ///
    /// # Errors
    ///
    /// Returns [`FalconRotatorError::InvalidValue`] if `sky_deg` is not
    /// finite, [`Self::read_status`]'s failure, or
    /// [`FalconRotatorError::SyncNotSaved`] if the record cannot be
    /// written; the offset is left unchanged in every case.
    pub async fn sync(&self, session: &Session<FalconCodec>, sky_deg: f64) -> Result<()> {
        if !sky_deg.is_finite() {
            return Err(FalconRotatorError::InvalidValue(format!(
                "sync target must be finite, got {sky_deg}"
            )));
        }
        let sky = SkyDegrees::new(sky_deg);
        let mech = self.read_status(session).await?.position_deg;
        let offset = sky - mech;
        let writing = self.sync_writes.lock().await;
        if let Some(path) = &self.sync_record {
            let write_path = path.clone();
            let write = Arc::clone(&self.write_record);
            tokio::task::spawn_blocking(move || write(&write_path, offset))
                .await
                .map_err(|e| {
                    FalconRotatorError::SyncNotSaved(format!("the write did not finish: {e}"))
                })?
                .map_err(|e| {
                    FalconRotatorError::SyncNotSaved(format!("{}: {e}", path.display()))
                })?;
        }
        *self.sync_offset.lock().await = offset;
        drop(writing);
        debug!(
            "sync: sky={:.4} mech={:.4} → sync_offset={:.4}",
            sky.value(),
            mech.value(),
            offset.value()
        );
        Ok(())
    }

    /// Read the current driver-side sync offset.
    pub async fn sync_offset(&self) -> SyncOffset {
        *self.sync_offset.lock().await
    }

    /// Store the last-requested sky-coordinate target.
    pub async fn set_target_position(&self, target: SkyDegrees) {
        *self.target_position.lock().await = Some(target);
    }

    /// Read the last-requested sky-coordinate target.
    pub async fn target_position(&self) -> Option<SkyDegrees> {
        *self.target_position.lock().await
    }
}

/// The `limit_detect` rising-edge rule: `None → true` and
/// `Some(false) → true` are edges, everything else is not.
///
/// `previous` is the `last_limit_detected` tracker, which the handshake
/// initialises to `None` so a fresh connection whose very first
/// observation reports `limit_detect = 1` still counts as an edge.
///
/// Split out of [`FalconManager::read_status`] deliberately: it makes the
/// decision the `warn!` hangs off testable as a pure function, so the tests
/// do not have to capture tracing output. See
/// [`testing.md` §6.8](../../../docs/skills/testing.md#68-do-not-assert-on-captured-tracing-events)
/// for why asserting on captured events is unsound in a parallel test binary.
const fn is_limit_detect_rising_edge(previous: Option<bool>, current: bool) -> bool {
    match previous {
        None => current,
        Some(prev) => !prev && current,
    }
}

/// Extract the trimmed echo string from a [`FalconResponse::Echo`], or
/// return an `InvalidResponse` describing the unexpected shape.
fn expect_echo(resp: FalconResponse, label: &str) -> Result<String> {
    match resp {
        FalconResponse::Echo(s) => Ok(s),
        other => Err(FalconRotatorError::InvalidResponse(format!(
            "{label} returned non-echo frame: {other:?}"
        ))),
    }
}

impl std::fmt::Debug for FalconManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FalconManager")
            .field("is_available", &self.transport.is_available())
            .finish_non_exhaustive()
    }
}

/// Handshake, run on every open of the port — at service start and on
/// each reconnect: `F#` → `FV` → `DR:0` → `FA` → `VS`.
///
/// The `FA` and `VS` reads are smoke tests — we just want to confirm
/// the wire format is honoured and that the device responds, so the
/// parsed results are discarded. Initialises the `last_limit_detected`
/// edge tracker to `None` so the first `read_status` after handshake
/// sees a fresh observation.
async fn handshake(
    conn: &Connection<FalconCodec>,
    last_limit_detected: Arc<Mutex<Option<bool>>>,
) -> std::result::Result<(), FalconCodecError> {
    // F# — ping
    let ping = conn.request(Command::Ping).await?;
    if !matches!(ping, FalconResponse::Ack) {
        return Err(FalconCodecError::InvalidResponse(
            "ping: expected FR_OK ack".to_string(),
        ));
    }

    // FV — firmware version (surfaced at info!)
    let fv = conn.request(Command::FirmwareVersion).await?;
    let version = match fv {
        FalconResponse::FirmwareVersion(v) => v,
        other => {
            return Err(FalconCodecError::InvalidResponse(format!(
                "firmware: expected FV reply, got {other:?}"
            )));
        }
    };
    info!("Falcon firmware v{}", version);

    // DR:0 — force de-rotation off
    let dr = conn.request(Command::DerotationOff).await?;
    match dr {
        FalconResponse::Echo(ref s) if s == "DR:0" => {}
        other => {
            return Err(FalconCodecError::InvalidResponse(format!(
                "derotation: expected DR:0 echo, got {other:?}"
            )));
        }
    }

    // FA — smoke test full status (parsed result discarded; no-cache design)
    let fa = conn.request(Command::FullStatus).await?;
    if !matches!(fa, FalconResponse::Status(_)) {
        return Err(FalconCodecError::InvalidResponse(format!(
            "full status: expected FR_OK:.. reply, got {fa:?}"
        )));
    }

    // VS — smoke test voltage
    let vs = conn.request(Command::Voltage).await?;
    if !matches!(vs, FalconResponse::Voltage(_)) {
        return Err(FalconCodecError::InvalidResponse(format!(
            "voltage: expected VS:.. reply, got {vs:?}"
        )));
    }

    // Initialise the edge tracker so the first read_status after the
    // handshake sees a fresh observation (rising-edge log fires on
    // None → true).
    *last_limit_detected.lock().await = None;
    Ok(())
}

/// Exhaustive tests for the `limit_detect` edge rule.
///
/// Not gated on `feature = "mock"` — the rule is a pure function and is
/// worth checking in every build profile.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod edge_rule_tests {
    use super::is_limit_detect_rising_edge;

    #[test]
    fn first_observation_high_is_an_edge() {
        assert!(is_limit_detect_rising_edge(None, true));
    }

    #[test]
    fn first_observation_low_is_not_an_edge() {
        assert!(!is_limit_detect_rising_edge(None, false));
    }

    #[test]
    fn low_to_high_is_an_edge() {
        assert!(is_limit_detect_rising_edge(Some(false), true));
    }

    #[test]
    fn high_to_high_is_not_an_edge() {
        assert!(!is_limit_detect_rising_edge(Some(true), true));
    }

    #[test]
    fn high_to_low_is_not_an_edge() {
        assert!(!is_limit_detect_rising_edge(Some(true), false));
    }

    #[test]
    fn low_to_low_is_not_an_edge() {
        assert!(!is_limit_detect_rising_edge(Some(false), false));
    }
}

/// Mock-backed integration tests for the manager.
///
/// Exercises the handshake + protocol API against the deterministic
/// [`MockFalconTransportFactory`](crate::mock::MockFalconTransportFactory).
/// Nothing here calls `start()`, so the transport runs `LazyAcquire`: the
/// first acquire opens the port and runs the handshake, and the last
/// release closes it.
/// Race / refcount / rollback invariants are tested once for everyone in
/// `rusty-photon-shared-transport`'s own test suite — not duplicated here.
#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod mock_tests {
    use super::*;
    use crate::mock::MockFalconTransportFactory;

    fn make_manager() -> (Arc<FalconManager>, Arc<MockFalconTransportFactory>) {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::new(Arc::<MockFalconTransportFactory>::clone(&factory));
        (manager, factory)
    }

    async fn acquire_session(manager: &FalconManager) -> Session<FalconCodec> {
        manager
            .transport()
            .acquire()
            .await
            .expect("acquire should succeed against the mock factory")
    }

    // ---- handshake + lifecycle ------------------------------------------

    #[tokio::test]
    async fn acquire_makes_available_and_runs_handshake_sequence() {
        let (manager, factory) = make_manager();
        assert!(!manager.is_available());
        let session = acquire_session(&manager).await;
        assert!(manager.is_available());

        // Pin the exact handshake sequence per the design-doc Connection
        // Lifecycle: F# → FV → DR:0 → FA → VS. A reorder or dropped
        // smoke-test command fails loudly here.
        let log = factory.command_log().await;
        assert_eq!(
            log,
            vec![
                "F#".to_string(),
                "FV".to_string(),
                "DR:0".to_string(),
                "FA".to_string(),
                "VS".to_string(),
            ],
            "handshake order must match design doc"
        );

        session.close().await.unwrap();
        assert!(!manager.is_available());
    }

    #[tokio::test]
    async fn second_acquire_reuses_transport_without_extra_commands() {
        let (manager, factory) = make_manager();
        let first = acquire_session(&manager).await;
        let after_first = factory.command_log().await;

        let second = acquire_session(&manager).await;
        assert_eq!(
            factory.command_log().await,
            after_first,
            "second acquire must not run another handshake"
        );

        // First close just drops the refcount; transport stays open.
        second.close().await.unwrap();
        assert!(manager.is_available());

        first.close().await.unwrap();
        assert!(!manager.is_available());
    }

    #[tokio::test]
    async fn the_last_release_clears_the_target_and_the_edge_but_keeps_the_offset() {
        let (manager, factory) = make_manager();
        let session = manager.acquire().await.unwrap();

        manager.set_target_position(SkyDegrees::new(123.45)).await;
        factory.set_limit_detect(true).await;
        let _ = manager.read_status(&session).await.unwrap();
        assert_eq!(*manager.last_limit_detected.lock().await, Some(true));
        factory.set_mech_position_deg(45.0).await;
        manager.sync(&session, 90.0).await.unwrap();

        manager.release(session).await.unwrap();

        assert_eq!(manager.target_position().await, None);
        assert_eq!(*manager.last_limit_detected.lock().await, None);
        assert!((manager.sync_offset().await.value() - 45.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_release_that_leaves_a_client_keeps_the_target_and_the_edge() {
        let (manager, factory) = make_manager();
        let first = manager.acquire().await.unwrap();
        let second = manager.acquire().await.unwrap();

        manager.set_target_position(SkyDegrees::new(123.45)).await;
        factory.set_limit_detect(true).await;
        let _ = manager.read_status(&first).await.unwrap();

        manager.release(first).await.unwrap();

        assert_eq!(
            manager.target_position().await,
            Some(SkyDegrees::new(123.45))
        );
        assert_eq!(*manager.last_limit_detected.lock().await, Some(true));
        manager.release(second).await.unwrap();
    }

    // ---- the sync-offset record -------------------------------------------

    #[tokio::test]
    async fn sync_writes_the_record_and_puts_the_offset_in_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::with_sync_record(
            Arc::<MockFalconTransportFactory>::clone(&factory),
            path.clone(),
        )
        .await;
        let session = manager.acquire().await.unwrap();
        factory.set_mech_position_deg(45.0).await;

        manager.sync(&session, 90.0).await.unwrap();

        assert!((manager.sync_offset().await.value() - 45.0).abs() < 1e-9);
        assert_eq!(
            crate::sync_record::read(&path),
            ReadRecord::Offset(SyncOffset::new(45.0))
        );
        manager.release(session).await.unwrap();
    }

    /// A record writer that reports each write as it starts and as it
    /// ends, naming the offset, and holds every write until the test sends
    /// a release: a test can act while a sync is mid-write.
    fn paused_writer() -> (
        RecordWriter,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (events, events_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let writer: RecordWriter = Arc::new(move |path: &Path, offset: SyncOffset| {
            events.send(format!("start {:.1}", offset.value())).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            let written = crate::sync_record::write(path, offset);
            events.send(format!("end {:.1}", offset.value())).unwrap();
            written
        });
        (writer, events_rx, release)
    }

    #[tokio::test]
    async fn a_sync_mid_write_holds_up_the_next_sync_but_no_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        let (writer, mut events, release) = paused_writer();
        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::build(
            Arc::<MockFalconTransportFactory>::clone(&factory),
            Some(path.clone()),
            SyncOffset::ZERO,
            writer,
        );
        let first = manager.acquire().await.unwrap();
        let second = manager.acquire().await.unwrap();
        factory.set_mech_position_deg(45.0).await;

        let syncing = Arc::clone(&manager);
        let first_sync = tokio::spawn(async move {
            let synced = syncing.sync(&first, 90.0).await;
            (synced, first)
        });
        assert_eq!(events.recv().await.unwrap(), "start 45.0");

        // The first sync is mid-write: a reader answers at once, with the
        // offset still in force.
        let offset =
            tokio::time::timeout(std::time::Duration::from_millis(500), manager.sync_offset())
                .await
                .expect("a reader must not wait on a sync's write");
        assert!(offset.value().abs() < 1e-9, "{}", offset.value());

        // A second sync reads the rotator, then waits for the first: its
        // write does not start while the first one's is in progress.
        let fa_before = status_reads(&factory).await;
        let syncing = Arc::clone(&manager);
        let second_sync = tokio::spawn(async move {
            let synced = syncing.sync(&second, 120.0).await;
            (synced, second)
        });
        while status_reads(&factory).await == fa_before {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "the second sync must not start writing while the first is"
        );

        release.send(()).unwrap();
        assert_eq!(events.recv().await.unwrap(), "end 45.0");
        assert_eq!(events.recv().await.unwrap(), "start 75.0");
        release.send(()).unwrap();
        assert_eq!(events.recv().await.unwrap(), "end 75.0");

        let (synced, first) = first_sync.await.unwrap();
        synced.unwrap();
        let (synced, second) = second_sync.await.unwrap();
        synced.unwrap();
        assert!(
            (manager.sync_offset().await.value() - 75.0).abs() < 1e-9,
            "the last sync to write is the last to assign"
        );
        assert_eq!(
            crate::sync_record::read(&path),
            ReadRecord::Offset(SyncOffset::new(75.0))
        );
        manager.release(first).await.unwrap();
        manager.release(second).await.unwrap();
    }

    /// How many `FA` status reads the mock has answered.
    async fn status_reads(factory: &MockFalconTransportFactory) -> usize {
        factory
            .command_log()
            .await
            .iter()
            .filter(|command| *command == "FA")
            .count()
    }

    #[tokio::test]
    async fn a_sync_whose_record_cannot_be_written_fails_and_keeps_the_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        // A directory where the record goes: no write can replace it.
        std::fs::create_dir(&path).unwrap();
        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::with_sync_record(
            Arc::<MockFalconTransportFactory>::clone(&factory),
            path,
        )
        .await;
        let session = manager.acquire().await.unwrap();
        factory.set_mech_position_deg(45.0).await;

        let err = manager.sync(&session, 90.0).await.unwrap_err();

        assert!(
            matches!(err, FalconRotatorError::SyncNotSaved(_)),
            "got {err:?}"
        );
        assert!(manager.sync_offset().await.value().abs() < 1e-9);
        manager.release(session).await.unwrap();
    }

    #[tokio::test]
    async fn a_manager_with_a_record_starts_from_the_recorded_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        crate::sync_record::write(&path, SyncOffset::new(254.2)).unwrap();

        let manager =
            FalconManager::with_sync_record(Arc::new(MockFalconTransportFactory::default()), path)
                .await;

        assert!((manager.sync_offset().await.value() - 254.2).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_manager_with_an_unreadable_record_starts_unsynced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.sync-offset.json");
        std::fs::write(&path, "not json").unwrap();

        let manager = FalconManager::with_sync_record(
            Arc::new(MockFalconTransportFactory::default()),
            path.clone(),
        )
        .await;

        assert!(manager.sync_offset().await.value().abs() < 1e-9);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not json",
            "the record stays until a Sync replaces it"
        );
    }

    // ---- read_status / read_voltage_raw ---------------------------------

    #[tokio::test]
    async fn read_status_returns_parsed_status() {
        let (manager, factory) = make_manager();
        factory.set_mech_position_deg(50.0).await;
        let session = acquire_session(&manager).await;

        let status = manager.read_status(&session).await.unwrap();
        assert!((status.position_deg.value() - 50.0).abs() < 1e-9);
        assert!(!status.motion.is_moving);
    }

    #[tokio::test]
    async fn read_voltage_raw_returns_default() {
        let (manager, factory) = make_manager();
        factory.set_voltage_raw(812).await;
        let session = acquire_session(&manager).await;
        let v = manager.read_voltage_raw(&session).await.unwrap();
        assert_eq!(v, 812);
    }

    // ---- move / halt ----------------------------------------------------

    #[tokio::test]
    async fn move_mechanical_sends_md_with_normalised_angle() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        manager
            .move_mechanical(&session, MechanicalDegrees::new(-30.0))
            .await
            .unwrap(); // → 330°
        let log = factory.command_log().await;
        assert_eq!(log, vec!["MD:330.00".to_string()]);
    }

    // Non-finite rejection lives at the ASCOM boundary (the
    // `MechanicalDegrees` argument is finite by construction); see the
    // `move_*_rejects_non_finite` tests in `rotator_device`.

    #[tokio::test]
    async fn move_mechanical_wraps_just_under_360_to_zero() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        manager
            .move_mechanical(&session, MechanicalDegrees::new(359.999))
            .await
            .unwrap();
        assert_eq!(factory.command_log().await, vec!["MD:0.00".to_string()]);
    }

    #[tokio::test]
    async fn move_mechanical_returns_wire_quantised_value() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        let wire = manager
            .move_mechanical(&session, MechanicalDegrees::new(359.999))
            .await
            .unwrap();
        assert!(
            (wire.value() - 0.0).abs() < 1e-9,
            "expected wire-quantised 0.0, got {}",
            wire.value()
        );

        let wire = manager
            .move_mechanical(&session, MechanicalDegrees::new(123.456))
            .await
            .unwrap();
        assert!(
            (wire.value() - 123.46).abs() < 1e-9,
            "expected wire-quantised 123.46, got {}",
            wire.value()
        );
    }

    #[tokio::test]
    async fn move_mechanical_exact_360_wraps_to_zero() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        manager
            .move_mechanical(&session, MechanicalDegrees::new(360.0))
            .await
            .unwrap();
        assert_eq!(factory.command_log().await, vec!["MD:0.00".to_string()]);
    }

    #[tokio::test]
    async fn halt_sends_fh_and_clears_target() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;
        manager.set_target_position(SkyDegrees::new(180.0)).await;
        factory.clear_command_log().await;

        manager.halt(&session).await.unwrap();
        assert_eq!(factory.command_log().await, vec!["FH".to_string()]);
        assert_eq!(manager.target_position().await, None);
    }

    // ---- set_reverse (EEPROM-wear protection) --------------------------

    #[tokio::test]
    async fn set_reverse_skips_write_when_equal() {
        let (manager, factory) = make_manager();
        factory.set_motor_reverse(true).await;
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        manager.set_reverse(&session, true).await.unwrap();
        // Only the FA read; no FN write.
        assert_eq!(factory.command_log().await, vec!["FA".to_string()]);
    }

    #[tokio::test]
    async fn set_reverse_writes_when_different() {
        let (manager, factory) = make_manager();
        // Mock starts with reverse=false.
        let session = acquire_session(&manager).await;
        factory.clear_command_log().await;

        manager.set_reverse(&session, true).await.unwrap();
        // Reads FA first, then writes FN:1.
        assert_eq!(
            factory.command_log().await,
            vec!["FA".to_string(), "FN:1".to_string()]
        );
    }

    // ---- sync (driver-side offset) -------------------------------------

    #[tokio::test]
    async fn sync_offset_arithmetic() {
        let (manager, factory) = make_manager();
        factory.set_mech_position_deg(120.0).await;
        let session = acquire_session(&manager).await;

        // mech = 120°, sync to 30° → offset = (30 - 120) mod 360 = 270.
        manager.sync(&session, 30.0).await.unwrap();
        let offset = manager.sync_offset().await.value();
        assert!(
            (offset - 270.0).abs() < 1e-9,
            "expected offset 270.0, got {offset}"
        );
    }

    #[tokio::test]
    async fn sync_rejects_non_finite() {
        let (manager, _factory) = make_manager();
        let session = acquire_session(&manager).await;
        let starting_offset = manager.sync_offset().await.value();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = manager.sync(&session, bad).await.unwrap_err();
            assert!(
                matches!(err, FalconRotatorError::InvalidValue(_)),
                "expected InvalidValue for {bad}, got {err:?}"
            );
        }
        assert!((manager.sync_offset().await.value() - starting_offset).abs() < 1e-9);
    }

    // ---- limit_detect edge tracker --------------------------------------
    //
    // The rising-edge *rule* is covered exhaustively as a pure function in
    // `edge_rule_tests`. What is left to pin here is the wiring: that
    // `read_status` feeds the tracker every observation, and that the
    // handshake leaves it at `None` so a fresh connection reporting
    // `limit_detect = 1` is a genuine `None → true` edge.
    //
    // These tests deliberately do NOT capture the `warn!` itself. A
    // thread-local subscriber cannot reliably observe an event whose
    // callsite another test in the same binary may register first — see
    // `docs/skills/testing.md` §6.8.

    #[tokio::test]
    async fn read_status_records_each_limit_detect_observation() {
        let (manager, factory) = make_manager();
        let session = acquire_session(&manager).await;

        // Handshake leaves the tracker cleared.
        assert_eq!(*manager.last_limit_detected.lock().await, None);

        // First read: limit_detect=false. None → Some(false), no edge.
        let _ = manager.read_status(&session).await.unwrap();
        assert_eq!(*manager.last_limit_detected.lock().await, Some(false));

        // Flip the device's flag: Some(false) → Some(true), a rising edge.
        factory.set_limit_detect(true).await;
        let status = manager.read_status(&session).await.unwrap();
        assert!(status.motion.limit_detect);
        assert_eq!(*manager.last_limit_detected.lock().await, Some(true));

        // Flag stays high: Some(true) → Some(true), not an edge.
        let _ = manager.read_status(&session).await.unwrap();
        assert_eq!(*manager.last_limit_detected.lock().await, Some(true));
    }

    #[tokio::test]
    async fn handshake_clears_tracker_even_when_limit_detect_is_high() {
        let (manager, factory) = make_manager();
        factory.set_limit_detect(true).await;
        let session = acquire_session(&manager).await;

        // The handshake issues its own FA, but that read does not pass
        // through `read_status`, so the tracker is still `None` — which is
        // what makes the first real read a `None → true` rising edge.
        assert_eq!(*manager.last_limit_detected.lock().await, None);

        let status = manager.read_status(&session).await.unwrap();
        assert!(status.motion.limit_detect);
        assert_eq!(*manager.last_limit_detected.lock().await, Some(true));
    }

    // ---- Debug ----------------------------------------------------------

    #[tokio::test]
    async fn debug_representation_contains_struct_name() {
        let (manager, _) = make_manager();
        let debug_str = format!("{manager:?}");
        assert!(debug_str.contains("FalconManager"));
    }

    // ========================================================================
    // Transport-failure error-branch coverage
    //
    // The happy-path tests above leave the wire-error arms of the protocol
    // methods (read_status / read_voltage_raw / move_mechanical / halt /
    // set_reverse / sync) and the handshake's rollback path uncovered.
    // A `FailingFactory` around the mock fails the next send with EOF,
    // which arrives at the device layer as
    // `FalconRotatorError::Communication("Connection closed")`.
    // ========================================================================

    use transport_doubles::FailingFactory;

    fn make_failing_manager() -> (
        Arc<FalconManager>,
        Arc<FailingFactory<MockFalconTransportFactory>>,
    ) {
        let factory = Arc::new(FailingFactory::new(MockFalconTransportFactory::default()));
        let manager = FalconManager::new(Arc::<FailingFactory<_>>::clone(&factory));
        (manager, factory)
    }

    #[tokio::test]
    async fn read_status_propagates_transport_failure() {
        let (manager, factory) = make_failing_manager();
        let session = acquire_session(&manager).await;

        factory.fail_next_send();
        let err = manager
            .read_status(&session)
            .await
            .expect_err("read_status should propagate the transport failure");
        assert!(
            matches!(err, FalconRotatorError::Communication(_)),
            "expected Communication (Eof maps to it), got {err:?}"
        );
    }

    #[tokio::test]
    async fn read_voltage_raw_propagates_transport_failure() {
        let (manager, factory) = make_failing_manager();
        let session = acquire_session(&manager).await;

        factory.fail_next_send();
        let err = manager
            .read_voltage_raw(&session)
            .await
            .expect_err("read_voltage_raw should propagate the transport failure");
        assert!(
            matches!(err, FalconRotatorError::Communication(_)),
            "expected Communication, got {err:?}"
        );
    }

    #[tokio::test]
    async fn move_mechanical_propagates_transport_failure() {
        let (manager, factory) = make_failing_manager();
        let session = acquire_session(&manager).await;

        factory.fail_next_send();
        let err = manager
            .move_mechanical(&session, MechanicalDegrees::new(180.0))
            .await
            .expect_err("move_mechanical should propagate the transport failure");
        assert!(
            matches!(err, FalconRotatorError::Communication(_)),
            "expected Communication, got {err:?}"
        );
    }

    #[tokio::test]
    async fn halt_propagates_transport_failure_and_leaves_target_unchanged() {
        // halt's contract: on success, clear target_position. On
        // transport failure, propagate the error and leave the cache
        // untouched (the device may still be moving — better to reflect
        // that uncertainty than lie about completion).
        let (manager, factory) = make_failing_manager();
        let session = acquire_session(&manager).await;
        manager.set_target_position(SkyDegrees::new(180.0)).await;

        factory.fail_next_send();
        let err = manager
            .halt(&session)
            .await
            .expect_err("halt should propagate the transport failure");
        assert!(
            matches!(err, FalconRotatorError::Communication(_)),
            "expected Communication, got {err:?}"
        );
        assert_eq!(
            manager.target_position().await,
            Some(SkyDegrees::new(180.0)),
            "halt's target-clearing side effect must NOT fire on transport failure"
        );
    }

    #[tokio::test]
    async fn sync_propagates_transport_failure_and_leaves_offset_unchanged() {
        // sync's read-FA-first arm: if the FA read fails, the driver-side
        // sync_offset must not be touched.
        let (manager, factory) = make_failing_manager();
        let session = acquire_session(&manager).await;
        let starting_offset = manager.sync_offset().await.value();

        factory.fail_next_send();
        let err = manager
            .sync(&session, 90.0)
            .await
            .expect_err("sync should propagate the transport failure");
        assert!(
            matches!(err, FalconRotatorError::Communication(_)),
            "expected Communication, got {err:?}"
        );
        assert!((manager.sync_offset().await.value() - starting_offset).abs() < 1e-9);
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_manager_unavailable_when_handshake_send_fails() {
        // Failing the first handshake send (F#) must:
        // - propagate Err from acquire(),
        // - leave is_available() == false (the RollbackGuard fired),
        // - leave driver-side state at its defaults (no partial handshake).
        let (manager, factory) = make_failing_manager();

        factory.fail_next_send();
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake failure should propagate out of acquire");
        // The send returned TransportError::Eof; that arrives at the
        // device layer as SessionError::Codec(FalconCodecError::Transport(
        // TransportError::Eof)), which routes through From<TransportError>
        // to FalconRotatorError::Communication("Connection closed").
        let mapped = FalconRotatorError::from(err);
        assert!(
            matches!(mapped, FalconRotatorError::Communication(_)),
            "expected Communication, got {mapped:?}"
        );

        assert!(!manager.is_available());
        assert!((manager.sync_offset().await.value()).abs() < 1e-9);
        assert_eq!(manager.target_position().await, None);
    }
}

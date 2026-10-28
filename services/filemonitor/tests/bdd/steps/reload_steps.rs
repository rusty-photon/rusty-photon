//! Step definitions for `reload.feature`: what a client of the safety
//! monitor sees across the service's in-process reload.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::SafetyMonitor;
use bdd_infra::setup_pages;
use cucumber::{given, then, when};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::world::FilemonitorWorld;

/// How many reads the background client makes once the rebuilt server
/// has answered, so its reads run past the whole reload.
const READS_AFTER_THE_RELOAD: usize = 20;

/// How long the background client may take to finish its reads, a bound
/// on a read that never returns rather than an expected duration.
const READER_BUDGET: Duration = Duration::from_secs(60);

/// The first byte of a TLS handshake record.
const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// A client reading `is_safe` on a fixed period until told the reload is
/// over, then for [`READS_AFTER_THE_RELOAD`] more reads.
#[derive(Debug)]
pub struct Reader {
    reloaded: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Vec<Result<bool, String>>>,
}

impl Reader {
    fn start(monitor: Arc<dyn SafetyMonitor>, period: Duration) -> Self {
        let (reloaded, mut reloaded_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut results = Vec::new();
            let mut after = 0;
            let mut ticker = tokio::time::interval(period);
            while after < READS_AFTER_THE_RELOAD {
                let _ = ticker.tick().await;
                results.push(monitor.is_safe().await.map_err(|e| e.to_string()));
                if *reloaded_rx.borrow_and_update() {
                    after += 1;
                }
            }
            results
        });
        Self { reloaded, task }
    }

    /// Tell the reader the reload is over and collect every read it made.
    async fn finish(self) -> Vec<Result<bool, String>> {
        self.reloaded.send_replace(true);
        tokio::time::timeout(READER_BUDGET, self.task)
            .await
            .unwrap_or_else(|_| panic!("the reader did not finish within {READER_BUDGET:?}"))
            .expect("the reader task panicked")
    }
}

#[given(expr = "a second monitoring file containing {string}")]
fn second_monitoring_file_containing(world: &mut FilemonitorWorld, content: String) {
    world.create_second_temp_file(&content);
}

/// Sends the first byte of a TLS handshake and nothing more, so the
/// server waits on the rest of it for its handshake timeout.
#[given("a client has started a TLS handshake and stalled")]
async fn stalled_tls_handshake(world: &mut FilemonitorWorld) {
    let port = world
        .filemonitor
        .as_ref()
        .expect("filemonitor is not running")
        .port();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the stalled client connects");
    stream
        .write_all(&[TLS_HANDSHAKE_CONTENT_TYPE])
        .await
        .expect("the stalled client sends its first byte");
    world.stalled = Some(stream);
}

#[when(expr = "I submit the server setup page pointing {string} at the second monitoring file")]
async fn submit_pointing_at_second_file(world: &mut FilemonitorWorld, name: String) {
    let path = world
        .second_file_path
        .as_ref()
        .expect("no second monitoring file")
        .to_string_lossy()
        .into_owned();
    setup_pages::submit(
        &mut world.setup_pages,
        setup_pages::SERVER_PATH,
        &[(&name, &path)],
        None,
    )
    .await;
}

/// Waits until the server page is answered by the server the reload
/// built, which connects its device, when it does, before it serves.
#[when("the rebuilt server answers its setup page")]
async fn rebuilt_server_answers(world: &mut FilemonitorWorld) {
    setup_pages::wait_until(
        &mut world.setup_pages,
        setup_pages::SERVER_PATH,
        "a page",
        |_| true,
    )
    .await;
}

#[when(expr = "a client reads is_safe every {int} milliseconds")]
fn client_reads_periodically(world: &mut FilemonitorWorld, period_ms: u64) {
    world.reader = Some(Reader::start(
        Arc::clone(world.monitor()),
        Duration::from_millis(period_ms),
    ));
}

#[then(expr = "every read the client made through the reload returned {word}")]
async fn every_read_returned(world: &mut FilemonitorWorld, expected: String) {
    let expected = expected == "true";
    let results = world
        .reader
        .take()
        .expect("no client is reading")
        .finish()
        .await;
    assert!(
        results.len() > READS_AFTER_THE_RELOAD,
        "the client made no read before the reload was over: {results:?}"
    );
    let wrong: Vec<(usize, &Result<bool, String>)> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.as_ref() != Ok(&expected))
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} reads did not return {expected}: {wrong:?}",
        wrong.len(),
        results.len()
    );
}

#[then("a connection to the port the service served before is refused")]
async fn old_port_refuses(world: &mut FilemonitorWorld) {
    let port = world.started_port.expect("the service was not started");
    let err = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect_err("the port the service served before still accepts");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "the connection to the old port failed, but not as refused: {err}"
    );
}

/// The stalled client has not been answered or closed: the server that
/// accepted it is still waiting on its handshake.
#[then("the stalled handshake is still open")]
fn stalled_handshake_still_open(world: &mut FilemonitorWorld) {
    let stream = world.stalled.as_ref().expect("no stalled client");
    let mut buf = [0u8; 1];
    match stream.try_read(&mut buf) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(0) => panic!("the server closed the stalled handshake"),
        Ok(n) => panic!("the server answered the stalled handshake with {n} byte(s)"),
        Err(e) => panic!("the stalled handshake failed: {e}"),
    }
}

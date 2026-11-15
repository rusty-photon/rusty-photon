#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `transport-doubles` — test doubles for the transports of
//! `rusty-photon-shared-transport`.
//!
//! A driver stores what a request brought back after its turn on the wire
//! ends, so a poll that read the device before a command can store after the
//! command's answer. A unit test that proves the driver keeps the newer value
//! has to put the two stores in that order, and [`HoldingFactory`] lets it:
//! it wraps the driver's own [`TransportFactory`], holds the reply to the
//! next request the test names, says when it holds it, and lets it go when
//! the test says so. `docs/skills/testing.md` has the walk-through.
//!
//! The crate depends only on the public transport traits, so any driver's
//! mock factory is the inner factory, and is a dev-dependency only.

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::Notify;

/// How long [`HoldingFactory::reply_held`] waits for a request to reach the
/// reply it holds.
pub const HOLD_DEADLINE: Duration = Duration::from_secs(10);

/// Wraps the transports `inner` opens and holds one reply each time a test
/// arms it.
///
/// A request that reaches the armed reply waits in `recv_frame`, before the
/// reply is read, until the test releases it. The device has the request by
/// then, so the held reply is already decided.
pub struct HoldingFactory<F> {
    inner: F,
    hold: Arc<ReplyHold>,
}

/// The state every transport a [`HoldingFactory`] opens shares.
#[derive(Default)]
struct ReplyHold {
    armed: Mutex<Option<Arm>>,
    /// A frame that took the arm has gone out: the next reply read is held.
    holding: AtomicBool,
    reached: Notify,
    released: Notify,
}

/// Which reply the next hold takes.
enum Arm {
    /// The reply to the next frame sent that starts with these bytes.
    ReplyTo(Vec<u8>),
    /// The next reply read, whatever asked for it — for a device that sends
    /// frames unasked.
    NextReply,
}

impl<F: TransportFactory> HoldingFactory<F> {
    /// Wrap `inner`, holding nothing until armed.
    #[must_use]
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            hold: Arc::default(),
        }
    }

    /// Hold the reply to the next frame sent that starts with `frame_start`.
    /// Frames that start otherwise pass, and leave the arm set.
    pub fn hold_next_reply_to(&self, frame_start: impl Into<Vec<u8>>) {
        self.hold.arm(Arm::ReplyTo(frame_start.into()));
    }

    /// Hold the next reply read, whatever frame went out before it.
    pub fn hold_next_reply(&self) {
        self.hold.arm(Arm::NextReply);
    }

    /// Wait until a request is held.
    ///
    /// # Panics
    ///
    /// When no request reaches the armed reply within [`HOLD_DEADLINE`].
    pub async fn reply_held(&self) {
        let reached = tokio::time::timeout(HOLD_DEADLINE, self.hold.reached.notified()).await;
        assert!(
            reached.is_ok(),
            "no request reached its reply in {HOLD_DEADLINE:?}"
        );
    }

    /// Let the held request read its reply.
    pub fn release_reply(&self) {
        self.hold.released.notify_one();
    }
}

impl ReplyHold {
    fn arm(&self, arm: Arm) {
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some(arm);
    }

    /// Take the arm if `takes` accepts it.
    fn take_arm(&self, takes: impl FnOnce(&mut Arm) -> bool) -> bool {
        self.armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take_if(takes)
            .is_some()
    }
}

#[async_trait]
impl<F: TransportFactory> TransportFactory for HoldingFactory<F> {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        let inner = self.inner.open().await?;
        Ok(Box::new(HoldingTransport {
            inner,
            hold: Arc::clone(&self.hold),
        }))
    }
}

struct HoldingTransport {
    inner: Box<dyn FrameTransport>,
    hold: Arc<ReplyHold>,
}

#[async_trait]
impl FrameTransport for HoldingTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let takes = self.hold.take_arm(
            |arm| matches!(arm, Arm::ReplyTo(frame_start) if bytes.starts_with(frame_start)),
        );
        let sent = self.inner.send_frame(bytes).await;
        if takes && sent.is_ok() {
            self.hold.holding.store(true, Ordering::SeqCst);
        }
        sent
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        if self.hold.holding.swap(false, Ordering::SeqCst)
            || self.hold.take_arm(|arm| matches!(arm, Arm::NextReply))
        {
            self.hold.reached.notify_one();
            self.hold.released.notified().await;
        }
        self.inner.recv_frame(buf).await
    }

    async fn discard_input(&mut self) -> Result<(), TransportError> {
        self.inner.discard_input().await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A wire that answers every frame with the frame itself, and counts the
    /// replies read and the input discards.
    #[derive(Default)]
    struct Wire {
        replies_read: AtomicUsize,
        discards: AtomicUsize,
    }

    #[derive(Default)]
    struct EchoFactory {
        wire: Arc<Wire>,
        fail_open: bool,
        fail_send: bool,
    }

    struct EchoTransport {
        wire: Arc<Wire>,
        fail_send: bool,
        last_sent: Vec<u8>,
    }

    #[async_trait]
    impl TransportFactory for EchoFactory {
        async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
            if self.fail_open {
                return Err(TransportError::Eof);
            }
            Ok(Box::new(EchoTransport {
                wire: Arc::clone(&self.wire),
                fail_send: self.fail_send,
                last_sent: Vec::new(),
            }))
        }
    }

    #[async_trait]
    impl FrameTransport for EchoTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            if self.fail_send {
                return Err(TransportError::Eof);
            }
            self.last_sent = bytes.to_vec();
            Ok(())
        }

        async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
            self.wire.replies_read.fetch_add(1, Ordering::SeqCst);
            buf.clone_from(&self.last_sent);
            Ok(())
        }

        async fn discard_input(&mut self) -> Result<(), TransportError> {
            self.wire.discards.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn holding_echo() -> (Arc<HoldingFactory<EchoFactory>>, Arc<Wire>) {
        let inner = EchoFactory::default();
        let wire = Arc::clone(&inner.wire);
        (Arc::new(HoldingFactory::new(inner)), wire)
    }

    /// Send `frame` and read its reply on a task of its own.
    fn exchange(
        mut transport: Box<dyn FrameTransport>,
        frame: &'static [u8],
    ) -> tokio::task::JoinHandle<(Box<dyn FrameTransport>, Vec<u8>)> {
        tokio::spawn(async move {
            transport.send_frame(frame).await.unwrap();
            let mut reply = Vec::new();
            transport.recv_frame(&mut reply).await.unwrap();
            (transport, reply)
        })
    }

    /// Send `frame` and read its reply, failing if the reply is held.
    async fn exchange_unheld(transport: &mut dyn FrameTransport, frame: &[u8]) -> Vec<u8> {
        transport.send_frame(frame).await.unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(HOLD_DEADLINE, transport.recv_frame(&mut reply))
            .await
            .expect("the reply was held")
            .unwrap();
        reply
    }

    #[tokio::test]
    async fn the_reply_to_a_frame_with_the_armed_start_waits_until_released() {
        let (factory, wire) = holding_echo();
        let transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        let request = exchange(transport, b"PA:1");
        factory.reply_held().await;
        assert_eq!(
            wire.replies_read.load(Ordering::SeqCst),
            0,
            "the held reply was read"
        );

        factory.release_reply();
        let (_, reply) = request.await.unwrap();
        assert_eq!(reply, b"PA:1");
        assert_eq!(wire.replies_read.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_frame_with_another_start_passes_and_leaves_the_arm_set() {
        let (factory, _) = holding_echo();
        let mut transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PS");
        assert_eq!(exchange_unheld(transport.as_mut(), b"PA").await, b"PA");

        let request = exchange(transport, b"PS");
        factory.reply_held().await;
        factory.release_reply();
        let (_, reply) = request.await.unwrap();
        assert_eq!(reply, b"PS");
    }

    #[tokio::test]
    async fn a_released_hold_holds_nothing_more() {
        let (factory, _) = holding_echo();
        let transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        let request = exchange(transport, b"PA");
        factory.reply_held().await;
        factory.release_reply();
        let (mut transport, _) = request.await.unwrap();

        assert_eq!(exchange_unheld(transport.as_mut(), b"PA").await, b"PA");
    }

    #[tokio::test]
    async fn a_frame_that_fails_to_send_holds_no_reply() {
        let inner = EchoFactory {
            fail_send: true,
            ..EchoFactory::default()
        };
        let factory = HoldingFactory::new(inner);
        let mut transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        transport.send_frame(b"PA").await.unwrap_err();

        let mut reply = Vec::new();
        tokio::time::timeout(HOLD_DEADLINE, transport.recv_frame(&mut reply))
            .await
            .expect("a reply was held after its frame failed to send")
            .unwrap();
    }

    #[tokio::test]
    async fn hold_next_reply_holds_a_reply_no_request_asked_for() {
        let (factory, wire) = holding_echo();
        let mut transport = factory.open().await.unwrap();

        factory.hold_next_reply();
        let read = tokio::spawn(async move {
            let mut frame = Vec::new();
            transport.recv_frame(&mut frame).await.unwrap();
        });
        factory.reply_held().await;
        assert_eq!(
            wire.replies_read.load(Ordering::SeqCst),
            0,
            "the held reply was read"
        );

        factory.release_reply();
        read.await.unwrap();
        assert_eq!(wire.replies_read.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_arm_set_while_a_reply_is_held_holds_the_next_matching_reply() {
        let (factory, _) = holding_echo();
        let transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        let first = exchange(transport, b"PA:1");
        factory.reply_held().await;

        factory.hold_next_reply_to(b"PA");
        factory.release_reply();
        let (transport, reply) = first.await.unwrap();
        assert_eq!(reply, b"PA:1");

        let second = exchange(transport, b"PA:2");
        factory.reply_held().await;
        factory.release_reply();
        let (_, reply) = second.await.unwrap();
        assert_eq!(reply, b"PA:2");
    }

    #[tokio::test]
    async fn the_hold_spans_the_transports_the_factory_opens() {
        let (factory, _) = holding_echo();
        let first = factory.open().await.unwrap();
        drop(first);
        let reopened = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        let request = exchange(reopened, b"PA");
        factory.reply_held().await;
        factory.release_reply();
        request.await.unwrap();
    }

    #[tokio::test]
    async fn discard_input_reaches_the_inner_transport() {
        let (factory, wire) = holding_echo();
        let mut transport = factory.open().await.unwrap();

        transport.discard_input().await.unwrap();

        assert_eq!(wire.discards.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn open_fails_as_the_inner_factory_does() {
        let inner = EchoFactory {
            fail_open: true,
            ..EchoFactory::default()
        };
        let factory = HoldingFactory::new(inner);

        let err = factory.open().await.err().unwrap();

        assert!(matches!(err, TransportError::Eof), "{err:?}");
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "no request reached its reply in 10s")]
    async fn reply_held_fails_when_no_request_reaches_the_reply_by_the_deadline() {
        let (factory, _) = holding_echo();
        let _transport = factory.open().await.unwrap();

        factory.hold_next_reply_to(b"PA");
        factory.reply_held().await;
    }
}

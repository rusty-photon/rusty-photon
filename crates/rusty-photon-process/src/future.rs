//! Awaiting a run without an async runtime: the blocking wait moves to a
//! thread of its own, and the future is woken when it ends.

use std::future::{Future, IntoFuture};
use std::io;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;

use crate::{Error, Event, Outcome, Running};

/// The future of a [`Running`] being awaited.
///
/// Dropping it before it completes force-stops the run: its thread kills the
/// child's tree and reaps the child, without waiting for a grace period —
/// unless the run was built with [`Bounded::finish_if_abandoned`], in which
/// case the thread waits the run out under its deadline instead.
///
/// [`Bounded::finish_if_abandoned`]: crate::Bounded::finish_if_abandoned
#[derive(Debug)]
#[must_use = "a run is stopped when the future waiting for it is dropped"]
pub struct Finishing {
    shared: Arc<Mutex<Slot>>,
    /// Tells the waiting thread the run was abandoned. `None` once the
    /// result has been taken, when there is nothing left to abandon, and
    /// from the start for a run that finishes if abandoned.
    abandon: Option<Sender<Event>>,
}

#[derive(Debug, Default)]
struct Slot {
    result: Option<Result<Outcome, Error>>,
    waker: Option<Waker>,
}

impl IntoFuture for Running {
    type Output = Result<Outcome, Error>;
    type IntoFuture = Finishing;

    fn into_future(self) -> Finishing {
        let shared = Arc::new(Mutex::new(Slot::default()));
        // A run that finishes when abandoned is never told it was.
        let abandon = (!self.finish_if_abandoned).then(|| self.events.clone());
        let mut delivery = Delivery {
            shared: Arc::clone(&shared),
            delivered: false,
        };
        let spawned = thread::Builder::new()
            .name("bounded-wait".to_string())
            .spawn(move || delivery.deliver(self.wait()));
        if let Err(error) = spawned {
            // The closure was dropped unrun, and with it the run, whose guard
            // has already stopped the child and reaped it. Its delivery's drop
            // filled the slot with the generic loss; the actual cause is known
            // here.
            lock(&shared).result = Some(Err(Error::Thread(error)));
        }
        Finishing { shared, abandon }
    }
}

impl Future for Finishing {
    type Output = Result<Outcome, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut slot = lock(&this.shared);
        if let Some(result) = slot.result.take() {
            drop(slot);
            this.abandon = None;
            return Poll::Ready(result);
        }
        if !slot
            .waker
            .as_ref()
            .is_some_and(|waker| waker.will_wake(cx.waker()))
        {
            slot.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Finishing {
    fn drop(&mut self) {
        if let Some(abandon) = self.abandon.take() {
            // A closed channel means the run has already ended.
            drop(abandon.send(Event::Abandoned));
        }
    }
}

/// The waiting thread's side of the slot. Fills it with the run's result —
/// or, if the thread ends without one, with the fact that it did, so the
/// future cannot pend forever.
struct Delivery {
    shared: Arc<Mutex<Slot>>,
    delivered: bool,
}

impl Delivery {
    fn deliver(&mut self, result: Result<Outcome, Error>) {
        self.delivered = true;
        let waker = {
            let mut slot = lock(&self.shared);
            slot.result = Some(result);
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if !self.delivered {
            self.deliver(Err(Error::Thread(io::Error::other(
                "the thread waiting for the child ended without a result",
            ))));
        }
    }
}

/// A poisoned slot is still a valid slot: every write to it is a single
/// assignment, so a panic elsewhere cannot leave it half-written.
fn lock(shared: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

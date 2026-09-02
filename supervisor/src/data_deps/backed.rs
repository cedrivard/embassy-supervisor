use core::cell::RefCell;
use core::ops::Deref;
use core::task::Poll;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_sync::waitqueue::MultiWakerRegistration;
use embassy_time::Duration;
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

use super::{Gated, producer_of};
use crate::{Coupling, Sig, TaskNode};

static SERVING_EVT: Mutex<CriticalSectionRawMutex, RefCell<MultiWakerRegistration<4>>> =
    Mutex::new(RefCell::new(MultiWakerRegistration::new()));

const GATE_RETRY: embassy_time::Duration = embassy_time::Duration::from_millis(250);

pub(crate) fn notify_serving() {
    SERVING_EVT.lock(|w| w.borrow_mut().wake());
}

fn serving(producer: &TaskNode) -> bool {
    producer.is_running() && producer.is_ready()
}

/// Wait until the producer is serving, its running state changes, or the
/// retry interval passes. Returns true if serving. If false, the caller may
/// request a start so a stopped producer is retried promptly.
async fn wait_serving(producer: &'static TaskNode) -> bool {
    use embassy_futures::select::select;
    let running = producer.is_running();
    let settled = || serving(producer) || producer.is_running() != running;
    let woke = core::future::poll_fn(|cx| {
        if settled() {
            return Poll::Ready(());
        }
        SERVING_EVT.lock(|w| w.borrow_mut().register(cx.waker()));
        // Registered-then-recheck closes the race against a concurrent
        // `set_ready` or `ack_dropped`: after this the event cannot fire unseen.
        if settled() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    });
    let _ = select(woke, embassy_time::Timer::after(GATE_RETRY)).await;
    serving(producer)
}

/// A signal whose producer is started by the first reader that calls
/// [`open`](crate::TaskNode::open). The value is not handed out until the
/// producer is running and ready. The gate counts readers so the producer can
/// retire once none are left ([`unwatched`](Self::unwatched), [`TaskNode::retire`]).
#[repr(C)]
pub struct Backed<T> {
    inner: T,
    requested: AtomicBool,
    openers: AtomicU32,
    watch: Signal<CriticalSectionRawMutex, ()>,
}

impl<T> Backed<T> {
    /// Wrap `inner` as a backed signal.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            requested: AtomicBool::new(false),
            openers: AtomicU32::new(0),
            watch: Signal::new(),
        }
    }

    /// How many [`Open`] guards are alive right now.
    pub fn openers(&self) -> u32 {
        self.openers.load(Ordering::Acquire)
    }

    /// Resolve once no reader has held the gate for `cooldown`, continuously.
    pub async fn unwatched(&self, cooldown: Duration) {
        use embassy_futures::select::{Either, select};
        loop {
            while self.openers() > 0 {
                self.watch.wait().await;
            }
            self.watch.reset();
            if let Either::First(()) =
                select(embassy_time::Timer::after(cooldown), self.watch.wait()).await
                && self.openers() == 0
            {
                return;
            }
        }
    }

    fn admit_reader(&self) {
        if self.openers.fetch_add(1, Ordering::AcqRel) == 0 {
            self.watch.signal(());
        }
    }

    fn drop_reader(&self) {
        if self.openers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.watch.signal(());
        }
    }
}

/// A reader's hold on a [`Backed`] signal, from [`open`](crate::TaskNode::open).
/// `Deref` gives the wrapped signal's API; dropping the guard lets the producer
/// notice the last reader has left.
pub struct Open<T: 'static> {
    target: &'static Backed<T>,
}

impl<T> Open<T> {
    /// The wrapped signal with the `'static` lifetime `Deref` cannot lend, for
    /// APIs taking `&'static self` such as [`Leased::lease`](crate::Leased::lease).
    /// It outlives the guard, so keeping it past the drop escapes the count.
    pub fn signal(&self) -> &'static T {
        &self.target.inner
    }
}

impl<T> Deref for Open<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.target.inner
    }
}

impl<T> Drop for Open<T> {
    fn drop(&mut self) {
        self.target.drop_reader();
    }
}

impl<T> Deref for Backed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Sync + 'static> Gated for Backed<T> {
    type Handle = Open<T>;

    fn admit(&'static self) -> Open<T> {
        self.admit_reader();
        Open { target: self }
    }

    async fn ensure(&'static self, caller: &'static TaskNode, entry: &'static Coupling) {
        let Some(producer) = producer_of(caller, entry) else {
            warn!(
                "supervisor: {} is gated but no graph declares a writer for it",
                entry.name()
            );
            return;
        };
        // Name the waits that cannot resolve on their own, once per open.
        if !producer.is_running() {
            // `Activate` re-enables an OnDemand node but never spawns it —
            // that is its pool policy's job.
            #[cfg(feature = "control")]
            if matches!(producer.mode(), crate::Mode::OnDemand) {
                warn!(
                    "supervisor: {} gates on OnDemand {}, which a control start \
                     re-enables but does not spawn: this read returns only once \
                     its pool grows it",
                    entry.name(),
                    producer.name()
                );
            }
            #[cfg(not(feature = "control"))]
            warn!(
                "supervisor: {} gates on {}, which is not running, and without \
                 `control` there is nothing that can start it: this read will \
                 not return",
                entry.name(),
                producer.name()
            );
        }
        loop {
            if serving(producer) {
                break;
            }
            #[cfg(feature = "control")]
            if !producer.is_running() && !self.requested.swap(true, Ordering::Relaxed) {
                crate::request_control(producer, crate::ControlOp::Activate).await;
            }
            if !wait_serving(producer).await {
                self.requested.store(false, Ordering::Relaxed);
            }
        }
        self.requested.store(false, Ordering::Relaxed);
    }
}

impl TaskNode {
    /// Wait until no reader has held `s` for `cooldown`, then clear readiness
    /// and, with `control`, request deactivation. If a reader arrives during
    /// the wait, restart the cooldown. The readiness handshake keeps the stop
    /// race-free: readers admitted after `clear_ready` wait for the next
    /// activation instead of reading a producer that is shutting down.
    pub async fn retire<T: Sync>(&'static self, s: Sig<Backed<T>>, cooldown: Duration) {
        loop {
            s.target.unwatched(cooldown).await;
            self.clear_ready();
            if s.target.openers() == 0 {
                break;
            }
            self.set_ready();
        }
        #[cfg(feature = "control")]
        crate::request_control(self, crate::ControlOp::Deactivate).await;
    }
}

#[cfg(feature = "coupling-observe")]
impl<T: crate::Observable> crate::Observable for Backed<T> {
    fn change_token(&self) -> u32 {
        self.inner.change_token()
    }
}

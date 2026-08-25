use core::cell::RefCell;
use core::ops::Deref;
use core::task::Poll;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::waitqueue::MultiWakerRegistration;
use portable_atomic::AtomicBool;

use super::{Gated, producer_of};
use crate::{Coupling, TaskNode};

static SERVING_EVT: Mutex<CriticalSectionRawMutex, RefCell<MultiWakerRegistration<4>>> =
    Mutex::new(RefCell::new(MultiWakerRegistration::new()));

const GATE_RETRY: embassy_time::Duration = embassy_time::Duration::from_millis(250);

pub(crate) fn notify_serving() {
    SERVING_EVT.lock(|w| w.borrow_mut().wake());
}

fn serving(producer: &TaskNode) -> bool {
    producer.is_running() && producer.is_ready()
}

async fn wait_serving(producer: &'static TaskNode) -> bool {
    use embassy_futures::select::{Either, select};
    let served = core::future::poll_fn(|cx| {
        if serving(producer) {
            return Poll::Ready(());
        }
        SERVING_EVT.lock(|w| w.borrow_mut().register(cx.waker()));
        // Registered-then-recheck closes the race against a concurrent
        // `set_ready`: after this the event cannot fire unseen.
        if serving(producer) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    });
    match select(served, embassy_time::Timer::after(GATE_RETRY)).await {
        Either::First(()) => true,
        Either::Second(()) => false,
    }
}

/// A signal whose producer is started by the first reader that
/// [`open`](crate::TaskNode::open)s it, and which is not handed out
/// until that producer is running AND reports ready — a stopped producer's
#[repr(C)]
pub struct Backed<T> {
    inner: T,
    requested: AtomicBool,
}

impl<T> Backed<T> {
    /// Wrap `inner` as a backed signal.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            requested: AtomicBool::new(false),
        }
    }
}

impl<T> Deref for Backed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Sync> Gated for Backed<T> {
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
            if !producer.is_running()
                && !self
                    .requested
                    .swap(true, portable_atomic::Ordering::Relaxed)
            {
                crate::request_control(producer, crate::ControlOp::Activate).await;
            }
            if !wait_serving(producer).await {
                self.requested
                    .store(false, portable_atomic::Ordering::Relaxed);
            }
        }
        self.requested
            .store(false, portable_atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "coupling-observe")]
impl<T: crate::Observable> crate::Observable for Backed<T> {
    fn change_token(&self) -> u32 {
        self.inner.change_token()
    }
}

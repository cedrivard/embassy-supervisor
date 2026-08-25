use core::sync::atomic::AtomicI32;

use embassy_futures::select::{Either3, select3};
use embassy_supervisor::TaskNode;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use embedded_hal::digital::StatefulOutputPin;

const DEFAULT_PERIOD_MS: i32 = 500;

static PERIOD_MS: AtomicI32 = AtomicI32::new(DEFAULT_PERIOD_MS);

static CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Set the heartbeat LED blink period. Positive values blink, zero turns the
/// LED off, and negative values turn it steadily on.
#[embassy_supervisor::dataflow]
pub fn set_period_ms(node: &'static TaskNode, ms: i32) {
    node.put(&PERIOD_MS, ms);
    CHANGED.signal(());
}

/// Plain generic async worker — NOT a `#[embassy_executor::task]`; the graph's
#[embassy_supervisor::dataflow]
pub async fn heartbeat_task<L: StatefulOutputPin>(node: &'static TaskNode, mut led: L) {
    loop {
        // The active phase is the healthy state: `bench` is `bound` to this
        // readiness, so it runs only while the heartbeat is actively running.
        node.set_ready();
        // Active phase: drive the LED per `PERIOD_MS` until the supervisor requests
        // a pause (`wait_shutdown` ends the phase). `CHANGED` makes a new setting
        // take effect at once — including from a steady state, which has no timer.
        'active: loop {
            let ms = node.get(&crate::heartbeat::PERIOD_MS);
            if ms > 0 {
                match select3(
                    Timer::after(Duration::from_millis(ms as u64)),
                    CHANGED.wait(),
                    node.wait_shutdown(),
                )
                .await
                {
                    Either3::First(()) => {
                        let _ = led.toggle();
                        node.beat();
                        warn_stalled();
                    }
                    Either3::Second(()) => {}
                    Either3::Third(()) => break 'active, // pause requested
                }
            } else {
                // Steady: `0` off, `<0` on. Hold until a change or a pause.
                if ms == 0 {
                    let _ = led.set_low();
                } else {
                    let _ = led.set_high();
                }
                // A steady LED produces no edges to beat on. Healthy but
                // silent: wake every 5 s to place a beat anyway.
                match select3(
                    Timer::after(Duration::from_secs(5)),
                    CHANGED.wait(),
                    node.wait_shutdown(),
                )
                .await
                {
                    Either3::First(()) => node.beat(),
                    Either3::Second(()) => {} // changed → re-read
                    Either3::Third(()) => break 'active,
                }
            }
        }

        let _ = led.set_low();
        node.ack_dropped();
        node.wait_resume().await;
    }
}

fn warn_stalled() {
    const STALL_TICKS: u32 = (embassy_time::TICK_HZ / 10) as u32;
    for id in embassy_supervisor::trace::executors() {
        if id == 0 {
            continue;
        }
        if let Some((task, ticks)) = embassy_supervisor::trace::stalled_task(id, STALL_TICKS) {
            defmt::warn!(
                "trace: {} is blocking executor {:x} ({} ticks and counting)",
                task.name(),
                id,
                ticks
            );
        }
    }
}

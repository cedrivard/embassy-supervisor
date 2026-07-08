//! `resources:` — safe resource threading. The macro emits one
//! `pub static NAME: ResourceSlot<Type>` per entry; `main` MOVES the resource in
//! with `provide()` (compile-time exclusive ownership — no `steal()`), the
//! generated glue `take()`s it before the spawn, the shell lends the worker
//! `&mut Type` and `restore()`s it after the worker returns. Covers: a single
//! resource, two resources (arg-order lock), `executor:` composition, and
//! partial-call extras after the resource params.

use embassy_supervisor::{TaskNode, supervisor_graph};

/// A stand-in for a HAL peripheral driver: owned, not Copy, not 'static-borrowable.
struct FakeLed {
    #[allow(dead_code)]
    level: u8,
}
struct FakeUart {
    #[allow(dead_code)]
    baud: u32,
}

/// Worker receiving one threaded resource after the node.
async fn blink(_node: &'static TaskNode, _led: &mut FakeLed) {}

/// Two resources, in `resources:` declaration order, then a partial-call extra.
async fn duplex(_node: &'static TaskNode, _led: &mut FakeLed, _uart: &mut FakeUart, _extra: u32) {}

supervisor_graph! {
    executor AUX;
    node BLINK = Terminate, deps: [], task: blink,
        resources: [LED: FakeLed];
    node DUPLEX = Pause, deps: [BLINK], executor: AUX,
        task: duplex(42),
        resources: [LED2: FakeLed, UART: FakeUart];
}

fn main() {
    // The emitted slots are ordinary statics: provide/take round-trips work and
    // an unprovided slot reads back None (the glue's fail-closed SpawnError path).
    assert!(LED.take().is_none(), "unprovided slot must be empty");
    LED.provide(FakeLed { level: 1 });
    let led = LED.take().expect("provided value must be takeable");
    LED.restore(led);
    assert!(LED.take().is_some(), "restore must refill the slot");

    UART.provide(FakeUart { baud: 115_200 });
    assert!(UART.take().is_some());

    // Slots: BLINK(0), DUPLEX(1); dep edge DUPLEX -> BLINK.
    assert_eq!(GRAPH.nodes.len(), 2);
    assert_eq!(GRAPH.deps[1], [0u8].as_slice());
}

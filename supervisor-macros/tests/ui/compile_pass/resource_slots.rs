
use embassy_supervisor::{TaskNode, supervisor_graph};

struct FakeLed {
    #[allow(dead_code)]
    level: u8,
}
struct FakeUart {
    #[allow(dead_code)]
    baud: u32,
}

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
    assert!(LED.take().is_none(), "unprovided slot must be empty");
    LED.provide(FakeLed { level: 1 });
    let led = LED.take().expect("provided value must be takeable");
    LED.restore(led);
    assert!(LED.take().is_some(), "restore must refill the slot");

    UART.provide(FakeUart { baud: 115_200 });
    assert!(UART.take().is_some());

    assert_eq!(GRAPH.nodes.len(), 2);
    assert_eq!(GRAPH.deps_of(1), [0u8].as_slice());
}

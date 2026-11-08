// Every `enabled_if:` form compiles: a path, a local fn, and a clause kept by
// `#[cfg(all())]` or stripped by `#[cfg(any())]`. supervisor/tests/enabled_if.rs
// asserts which ones `start()` applies.
use embassy_supervisor::{TaskNode, supervisor_graph};

mod settings {
    pub fn lora_on() -> bool {
        false
    }
}

fn always() -> bool {
    true
}

async fn worker(_node: &'static TaskNode) {}

supervisor_graph! {
    node LORA = Terminate, deps: [], task: worker, enabled_if: crate::settings::lora_on;
    node WIFI = Terminate, deps: [LORA], task: worker, enabled_if: always, slot_timeout: 250;
    node GATED = Terminate, deps: [], task: worker, #[cfg(all())] enabled_if: always;
    node STRIPPED = Terminate, deps: [], task: worker, #[cfg(any())] enabled_if: always;
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 4);
    assert_eq!(GRAPH.order().len(), 4);
    // The predicate is evaluated by start(), not at declaration.
    assert!(!LORA.is_disabled());
    assert_eq!(WIFI.slot_timeout(), embassy_time::Duration::from_millis(250));
}

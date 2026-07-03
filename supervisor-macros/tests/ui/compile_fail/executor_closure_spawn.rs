//! `executor:` cannot combine with a verbatim spawn closure: the closure owns
//! the spawn, so the macro has nowhere to inject the slot's spawner. The closure
//! should use the named SpawnerSlot itself.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [], executor: HIGH,
        spawn: |s| { let _ = s; Ok(()) };
}

fn main() {}

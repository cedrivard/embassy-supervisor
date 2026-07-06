//! `executor:` asks the macro to spawn through a named slot, so it is
//! meaningless on a parked node (no `spawn:`): the application performs that
//! spawn itself and picks its own spawner.

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Pause, deps: [], executor: HIGH;
}

fn main() {}


use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    executor HIGH;
    node A = Terminate, deps: [], executor: HIGH,
        spawn: |s| { let _ = s; Ok(()) };
}

fn main() {}

//! `task:` wraps an async worker fn in a generated shell, so it needs a name to
//! call — a closure has none. Closures belong to `spawn:` (emitted verbatim).

use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [], task: |_s| Ok(());
}

fn main() {}

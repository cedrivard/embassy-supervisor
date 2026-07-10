//! The `local` resource kind (feature `local-resources` — the graph-site slot
//! type carries an `unsafe impl Sync`, so it is opt-in): plain `local`,
//! `local consume`, and `shared local` compositions with genuinely `!Send`
//! payloads, plus the contextual-keyword disambiguation for a path literally
//! starting with `local::`.

use std::cell::Cell;
use std::rc::Rc;

use embassy_supervisor::{TaskNode, supervisor_graph};

/// A module literally named `local`: `W: local local::Widget` below parses the
/// first `local` as the kind marker and the second as this path's head.
mod local {
    pub struct Widget {
        #[allow(dead_code)]
        pub v: u8,
    }
}

/// A `!Send` resource (`Rc` is `!Send`): only a `local` slot can carry it.
type Blob = Rc<Cell<u32>>;

async fn omni(
    _node: &'static TaskNode,
    _w: &mut local::Widget, // `local` kind: threaded as usual, graph-site slot type
    _b: Blob,               // `local consume`: by value AND `!Send`
) {
}

/// A `Copy` fan-out handle that is also `!Send` (raw pointer) — the
/// `embassy_net::Stack` shape, needing `shared local`.
type LocalHandle = (u32, *const ());

async fn consumer(_node: &'static TaskNode, _s: LocalHandle) {}

supervisor_graph! {
    node OMNI = Terminate, deps: [], task: omni,
        resources: [
            W: local local::Widget,
            B: consume local Blob, // markers are order-free
        ];

    // The SAME `shared local` slot on two nodes: one static, non-destructive reads.
    node USER_A = Terminate, deps: [OMNI],
        task: consumer,
        resources: [S: shared local LocalHandle];
    node USER_B = Terminate, deps: [OMNI],
        task: consumer,
        resources: [S: shared local LocalHandle];
}

fn main() {
    // The `local` slots are ordinary statics with the ResourceSlot protocol.
    assert!(W.take().is_none(), "unprovided local slot must be empty");
    W.provide(local::Widget { v: 1 });
    let w = W.take().expect("provided value must be takeable");
    W.restore(w);
    assert!(W.take().is_some(), "restore must refill the local slot");

    // A `!Send` value round-trips through its `local` slot.
    B.provide(Rc::new(Cell::new(7)));
    assert_eq!(B.take().expect("Rc must be takeable").get(), 7);

    // `shared local`: `get()` copies without emptying.
    S.provide((3, core::ptr::null()));
    assert_eq!(S.get().expect("shared local get").0, 3);
    assert!(S.get().is_some(), "shared local slot stays filled");

    assert_eq!(GRAPH.nodes.len(), 3);
}

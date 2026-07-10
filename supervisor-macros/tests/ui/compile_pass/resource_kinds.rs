//! `resources:` kind markers — `local` (a `!Send`-capable graph-site slot),
//! `consume` (worker receives the value BY VALUE, no restore emitted), and
//! `shared` (fan-out: several nodes/pools declare the SAME slot; the glue
//! copies the `Copy` handle out non-destructively) — plus their compositions,
//! per-entry `#[cfg]` (in and out), and the `slot_timeout:` clause on a node
//! and a pool. Also locks the contextual-keyword disambiguation: `local` /
//! `consume` right after the colon are markers ONLY when more of the entry
//! follows them — followed by `::` they start a type path, and standing alone
//! they ARE the type.

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

/// A type literally named `consume`: `C: consume` below has no marker — the
/// ident is followed by the entry end, so it is the type itself.
#[allow(non_camel_case_types)]
struct consume;

/// A Send resource threaded by value (`consume` kind).
struct Probe {
    #[allow(dead_code)]
    runs: u32,
}

/// A `!Send` resource (`Rc` is `!Send`): only a `local` slot can carry it.
type Blob = Rc<Cell<u32>>;

async fn omni(
    _node: &'static TaskNode,
    _w: &mut local::Widget, // `local` kind: threaded as usual, slot is the graph-site type
    _c: &mut consume,       // default kind (the type is just named `consume`)
    _p: Probe,              // `consume` kind: by value, worker owns (and drops) it
    _b: Blob,               // `local consume`: by value AND `!Send`
    // Per-entry cfg: the worker gates the matching param with the same #[cfg].
    #[cfg(feature = "nope")] _gone: &mut Probe,
) {
}

/// A `Copy` fan-out handle (also `!Send`, so the STACK slot below is
/// `shared local` — the `embassy_net::Stack` shape).
#[derive(Clone, Copy)]
struct Handle {
    v: u32,
}
type LocalHandle = (Handle, *const ()); // Copy + !Send (raw pointer)

/// Two nodes and a pool consume the same shared handles (by value).
async fn consumer(_node: &'static TaskNode, _h: Handle, _s: LocalHandle) {}

supervisor_graph! {
    node OMNI = Terminate, deps: [], task: omni,
        slot_timeout: 2500,
        resources: [
            W: local local::Widget,
            C: consume,
            P: consume Probe,
            B: consume local Blob, // markers are order-free
            #[cfg(feature = "nope")]
            GONE: Probe, // cfg'd OUT: no slot, no param, no gate
        ];

    // The SAME shared slots on two nodes and a pool: one static each, every
    // consumer's glue copies the value out (`get()` — slot stays filled).
    node USER_A = Terminate, deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle, S: shared local LocalHandle];
    node USER_B = Terminate, deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle, S: shared local LocalHandle];
    pool CREW = [Terminate, OnDemand], deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle, S: shared local LocalHandle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2,
        slot_timeout: 3000;
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

    // Default-kind slots are unchanged `ResourceSlot`s.
    C.provide(consume);
    P.provide(Probe { runs: 0 });
    assert!(C.take().is_some() && P.take().is_some());

    // Shared slots: `get()` copies without emptying — any number of readers.
    H.provide(Handle { v: 9 });
    assert_eq!(H.get().expect("shared get").v, 9);
    assert_eq!(H.get().expect("still filled after get").v, 9);
    S.provide((Handle { v: 3 }, core::ptr::null()));
    assert_eq!(S.get().expect("shared local get").0.v, 3);
    assert!(S.get().is_some(), "shared local slot stays filled");

    // OMNI, USER_A, USER_B, CREW[0], CREW[1].
    assert_eq!(GRAPH.nodes.len(), 5);
}

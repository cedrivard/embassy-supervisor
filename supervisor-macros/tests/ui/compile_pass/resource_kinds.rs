//! `resources:` kind markers that need no opt-in feature — `consume` (worker
//! receives the value BY VALUE, no restore emitted) and `shared` (fan-out:
//! several nodes/pools declare the SAME slot; the glue copies the `Copy` handle
//! out non-destructively) — plus per-entry `#[cfg]` (in and out) and the
//! `slot_timeout:` clause on a node and a pool. Also locks the
//! contextual-keyword disambiguation: `consume` right after the colon is a
//! marker ONLY when more of the entry follows it — standing alone it IS the
//! type. (The `local` kind's cases live in `compile_pass_local/`, gated on the
//! `local-resources` feature.)

use embassy_supervisor::{TaskNode, supervisor_graph};

/// A type literally named `consume`: `C: consume` below has no marker — the
/// ident is followed by the entry end, so it is the type itself.
#[allow(non_camel_case_types)]
struct consume;

/// A Send resource threaded by value (`consume` kind).
struct Probe {
    #[allow(dead_code)]
    runs: u32,
}

async fn omni(
    _node: &'static TaskNode,
    _c: &mut consume, // default kind (the type is just named `consume`)
    _p: Probe,        // `consume` kind: by value, worker owns (and drops) it
    // Per-entry cfg: the worker gates the matching param with the same #[cfg].
    #[cfg(feature = "nope")] _gone: &mut Probe,
) {
}

/// A `Copy` fan-out handle for the `shared` slots below.
#[derive(Clone, Copy)]
struct Handle {
    v: u32,
}

/// Two nodes and a pool consume the same shared handle (by value).
async fn consumer(_node: &'static TaskNode, _h: Handle) {}

supervisor_graph! {
    node OMNI = Terminate, deps: [], task: omni,
        slot_timeout: 2500,
        resources: [
            C: consume,
            P: consume Probe,
            #[cfg(feature = "nope")]
            GONE: Probe, // cfg'd OUT: no slot, no param, no gate
        ];

    // The SAME shared slot on two nodes and a pool: one static, every
    // consumer's glue copies the value out (`get()` — slot stays filled).
    node USER_A = Terminate, deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle];
    node USER_B = Terminate, deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle];
    pool CREW = [Terminate, OnDemand], deps: [OMNI],
        task: consumer,
        resources: [H: shared Handle],
        policy: embassy_supervisor::DeferredShrink::new(embassy_time::Duration::from_secs(1)),
        min: 1, max: 2,
        slot_timeout: 3000;
}

fn main() {
    // Default-kind and consume-kind slots are ordinary `ResourceSlot`s.
    C.provide(consume);
    P.provide(Probe { runs: 0 });
    assert!(C.take().is_some() && P.take().is_some());

    // Shared slots: `get()` copies without emptying — any number of readers.
    H.provide(Handle { v: 9 });
    assert_eq!(H.get().expect("shared get").v, 9);
    assert_eq!(H.get().expect("still filled after get").v, 9);

    // OMNI, USER_A, USER_B, CREW[0], CREW[1].
    assert_eq!(GRAPH.nodes.len(), 5);
}

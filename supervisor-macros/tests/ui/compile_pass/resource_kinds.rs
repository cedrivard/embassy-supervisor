
use embassy_supervisor::{TaskNode, supervisor_graph};

#[allow(non_camel_case_types)]
struct consume;

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
            GONE: Probe, 
        ];

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
    C.provide(consume);
    P.provide(Probe { runs: 0 });
    assert!(C.take().is_some() && P.take().is_some());

    H.provide(Handle { v: 9 });
    assert_eq!(H.get().expect("shared get").v, 9);
    assert_eq!(H.get().expect("still filled after get").v, 9);

    assert_eq!(GRAPH.nodes.len(), 5);
}

//! The token-relay spike behind `supervisor_fragment!`/`compose_graph!`: a
//! hand-written macro_rules "fragment" forwards its items through a callback
//! chain into ONE `supervisor_graph!` expansion, so every whole-graph pass
//! (name map, u8 slots, topo order, dedup) still sees all items. This locks the
//! plumbing the proc macro emits: the accumulate-then-invoke recursion shape.

use embassy_supervisor::{TaskNode, supervisor_graph};

async fn net_worker(_node: &'static TaskNode) {}
async fn app_worker(_node: &'static TaskNode) {}

// What `supervisor_fragment! { name: NET_FRAG; ... }` will emit, by hand:
// a callback macro forwarding its item tokens plus the remaining-fragment list
// and the accumulated items.
macro_rules! net_frag {
    (@emit $cb:path, [$($rest:tt)*], {$($acc:tt)*}, {$($g:tt)*}) => {
        $cb! { @next [$($rest)*],
               {$($acc)* node NET = Terminate, deps: [], task: net_worker;},
               {$($g)*} }
    };
}

// What `compose_graph!` does: expand each fragment in turn, then hand the
// accumulated items + the compose-site's own items to `supervisor_graph!`.
macro_rules! compose {
    (fragments: [$f:path $(, $r:path)* $(,)?], graph: {$($g:tt)*}) => {
        $f! { @emit compose, [$($r),*], {}, {$($g)*} }
    };
    (@next [], {$($acc:tt)*}, {$($g:tt)*}) => {
        supervisor_graph! { $($acc)* $($g)* }
    };
    (@next [$f:path $(, $r:path)*], {$($acc:tt)*}, $g:tt) => {
        $f! { @emit compose, [$($r),*], {$($acc)*}, $g }
    };
}

compose! {
    fragments: [net_frag],
    graph: {
        // Cross-fragment dep by name: legal because everything reaches one
        // supervisor_graph! expansion.
        node APP = Terminate, deps: [NET], task: app_worker;
    }
}

fn main() {
    assert_eq!(GRAPH.order, [0, 1], "fragment node first, dependent second");
    assert!(GRAPH.nodes[0].is_some() && GRAPH.nodes[1].is_some());
    assert_eq!(NET.name, "net");
    assert_eq!(APP.name, "app");
}

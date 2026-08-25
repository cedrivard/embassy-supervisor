
use embassy_supervisor::{TaskNode, supervisor_graph};

async fn net_worker(_node: &'static TaskNode) {}
async fn app_worker(_node: &'static TaskNode) {}

macro_rules! net_frag {
    (@emit $cb:path, [$($rest:tt)*], {$($acc:tt)*}, {$($g:tt)*}) => {
        $cb! { @next [$($rest)*],
               {$($acc)* node NET = Terminate, deps: [], task: net_worker;},
               {$($g)*} }
    };
}

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
        node APP = Terminate, deps: [NET], task: app_worker;
    }
}

fn main() {
    assert!(
        GRAPH.order().eq([0, 1]),
        "fragment node first, dependent second"
    );
    assert!(GRAPH.nodes[0].is_some() && GRAPH.nodes[1].is_some());
    assert_eq!(NET.name(), "net");
    assert_eq!(APP.name(), "app");
}

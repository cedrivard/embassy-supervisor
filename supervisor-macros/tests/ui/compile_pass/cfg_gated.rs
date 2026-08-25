
use embassy_supervisor::supervisor_graph;

supervisor_graph! {
    node A = Terminate, deps: [];
    #[cfg(any())]
    node GONE = Terminate, deps: [A];
    #[cfg(all())]
    node HERE = Terminate, deps: [A];
    node D = Terminate, deps: [A, #[cfg(any())] GONE, #[cfg(all())] HERE];
}

fn main() {
    assert_eq!(GRAPH.nodes.len(), 4);
    assert!(GRAPH.nodes[0].is_some()); 
    assert!(GRAPH.nodes[1].is_none()); 
    assert!(GRAPH.nodes[2].is_some()); 
    assert!(GRAPH.nodes[3].is_some()); 

    assert_eq!(GRAPH.deps_of(3), [0u8, 2u8].as_slice()); 
}

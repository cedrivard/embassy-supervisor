use crate::TaskNode;

#[cfg(feature = "trace-self")]
static SELF_NODE_CFG: crate::NodeCfg = crate::NodeCfg::new("supervisor", crate::Mode::Pause, None);

/// A reference to a registered graph, used for runtime introspection.
pub struct GraphRef {
    nodes: &'static [Option<&'static TaskNode>],
    #[cfg(feature = "trace")]
    chain: Chain,
    #[cfg(feature = "trace-self")]
    self_node: Option<TaskNode>,
}

impl GraphRef {
    /// Build a graph reference from the fixed node slot array.
    pub const fn new(nodes: &'static [Option<&'static TaskNode>]) -> Self {
        Self {
            nodes,
            #[cfg(feature = "trace")]
            chain: Chain::new(),
            #[cfg(feature = "trace-self")]
            self_node: Some(TaskNode::new(&SELF_NODE_CFG, false)),
        }
    }

    /// Return the graph's node slot array.
    pub const fn nodes(&self) -> &'static [Option<&'static TaskNode>] {
        self.nodes
    }

    #[cfg(feature = "trace-self")]
    /// Return the supervisor's own introspection node, if enabled.
    pub const fn self_node(&'static self) -> Option<&'static TaskNode> {
        self.self_node.as_ref()
    }
}

#[cfg(feature = "data-deps")]
pub(crate) static NO_GRAPH: GraphRef = GraphRef {
    nodes: &[],
    #[cfg(feature = "trace")]
    chain: Chain::new(),
    #[cfg(feature = "trace-self")]
    self_node: None,
};

#[cfg(feature = "trace")]
mod chain {
    use core::cell::Cell;
    use core::sync::atomic::Ordering;

    use embassy_sync::blocking_mutex::Mutex;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use portable_atomic::AtomicBool;

    use super::GraphRef;

    type Link = Mutex<CriticalSectionRawMutex, Cell<Option<&'static GraphRef>>>;

    const fn unlinked() -> Link {
        Mutex::new(Cell::new(None))
    }

    /// Per-graph link state.
    pub(super) struct Chain {
        next: Link,
        /// Latched at registration so a graph started twice (a sub-graph
        /// supervisor is legitimately `start()`/`teardown()`-cycled) is linked
        /// once, without walking the chain to look for itself.
        linked: AtomicBool,
    }

    impl Chain {
        pub(super) const fn new() -> Self {
            Self {
                next: unlinked(),
                linked: AtomicBool::new(false),
            }
        }
    }

    /// Head of the chain: the most recently registered graph.
    static HEAD: Link = unlinked();

    impl GraphRef {
        /// Link this graph into the binary-wide chain, so the trace hooks can
        /// resolve a task id to one of its nodes. Called by
        /// [`Supervisor::start`](crate::Supervisor::start); idempotent, and
        /// unbounded in the number of graphs.
        pub fn register(&'static self) {
            if self.chain.linked.swap(true, Ordering::AcqRel) {
                return;
            }
            HEAD.lock(|head| {
                self.chain.next.lock(|next| next.set(head.get()));
                head.set(Some(self));
            });
        }
    }

    /// Return an iterator over every registered graph, most recent first.
    pub fn graphs() -> Graphs {
        Graphs {
            next: HEAD.lock(Cell::get),
        }
    }

    /// Iterator over registered graphs returned by [`graphs`](fn@graphs).
    pub struct Graphs {
        next: Option<&'static GraphRef>,
    }

    impl Iterator for Graphs {
        type Item = &'static GraphRef;

        fn next(&mut self) -> Option<Self::Item> {
            let cur = self.next?;
            self.next = cur.chain.next.lock(Cell::get);
            Some(cur)
        }
    }
}

#[cfg(feature = "trace")]
use chain::Chain;
#[cfg(feature = "trace")]
pub use chain::{Graphs, graphs};

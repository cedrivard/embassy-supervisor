//! The static graph: its topology, shape bits and the const topological
//! sort.

use super::*;

// ─── Topology ────────────────────────────────────────────────────────────

/// Structural facts about a graph, as bits in [`Topology::SHAPE`]: what the
/// graph contains, decided at `supervisor_graph!` expansion and carried in the
/// topology's type, so lifecycle code serving an absent structure folds away at
/// compile time.
pub mod shape {
    /// The graph contains at least one `ready` dependency.
    pub const READY_DEPS: u32 = 1 << 0;
    /// The graph declares at least one named executor slot.
    pub const EXEC_SLOTS: u32 = 1 << 1;
    /// The graph declares at least one resource slot.
    pub const RESOURCES: u32 = 1 << 2;
    /// The graph contains at least one `Pause` node or pool member.
    pub const PAUSE: u32 = 1 << 3;
    /// The graph contains at least one `OnDemand` node or pool member.
    pub const ON_DEMAND: u32 = 1 << 4;
    /// Set when at least one node declares `beat_timeout:`.
    pub const BEATS: u32 = 1 << 5;
    /// The graph declares at least one `observed` signal entry.
    pub const OBSERVED: u32 = 1 << 6;
    /// The graph contains at least one `bound` dependency.
    pub const BOUND_DEPS: u32 = 1 << 7;
    /// The graph declares at least one elastic pool.
    pub const POOLS: u32 = 1 << 8;
    /// The graph declares at least one `divisible` resource.
    pub const CLAIMS: u32 = 1 << 9;
    /// All shape bits set.
    pub const ALL: u32 = u32::MAX;
}

/// Structural information about a graph, used by [`Supervisor`] to decide
/// which lifecycle code paths can be compiled out.
///
/// `order_at` must be a topological order of `deps_of`: the control cascades
/// compute closures in one pass over it.
pub trait Topology<const N: usize>: 'static {
    /// Structural-fact bits (see [`shape`]). An unset bit promises the
    /// structure is absent from the whole graph.
    const SHAPE: u32;

    /// The dependency indices of slot `i` — what node `i` declared it needs
    /// spawned first. **Spawn ordering, not runtime coupling**: see the crate
    /// docs on what a `deps:` edge does and does not assert.
    fn deps_of(&self, i: u8) -> &'static [u8];

    /// Return the slot index at topological position `k` (0..N).
    fn order_at(&self, k: usize) -> u8;
}

/// A [`Topology`] whose nodes are topologically sorted by their `deps:` edges.
pub struct Ordered<const N: usize, const SHAPE: u32> {
    deps: &'static [&'static [u8]; N],
    order: [u8; N],
}

impl<const N: usize, const SHAPE: u32> Ordered<N, SHAPE> {
    /// Build a topology from a static array of dependency lists.
    pub const fn new(deps: &'static [&'static [u8]; N]) -> Self {
        Self {
            deps,
            order: topo_sort_const(deps),
        }
    }
}

impl<const N: usize, const SHAPE: u32> Topology<N> for Ordered<N, SHAPE> {
    const SHAPE: u32 = SHAPE;

    fn deps_of(&self, i: u8) -> &'static [u8] {
        self.deps[i as usize]
    }

    fn order_at(&self, k: usize) -> u8 {
        self.order[k]
    }
}

/// The [`Topology`] of a graph with **no** `deps:` edges anywhere: zero-sized,
/// every dep list is empty by type, and the topological order is declaration
/// order. The walks a [`Supervisor`] runs over it fold to plain index loops,
/// and the dependency cascades (`activate`, `deactivate`, `restart`) collapse
/// to their seed sets.
pub struct Flat<const SHAPE: u32>;

impl<const SHAPE: u32> Flat<SHAPE> {
    /// The (zero-sized) flat topology.
    pub const fn new() -> Self {
        Self
    }
}

impl<const SHAPE: u32> Default for Flat<SHAPE> {
    fn default() -> Self {
        Self
    }
}

impl<const N: usize, const SHAPE: u32> Topology<N> for Flat<SHAPE> {
    const SHAPE: u32 = SHAPE;

    fn deps_of(&self, _i: u8) -> &'static [u8] {
        &[]
    }

    fn order_at(&self, k: usize) -> u8 {
        k as u8
    }
}

pub(crate) const fn has(shape_bits: u32, bit: u32) -> bool {
    shape_bits & bit != 0
}

/// A static task graph: the nodes, the topology over them, and optional pools.
///
/// `supervisor_graph!` emits one as a `static` (named `GRAPH` unless the graph
/// is named). It holds only references and the topology: the node slots
/// point at each node's own `static` config and handle, the topology carries
/// the dep rows and the spawn order, and `pools` lists the elastic pools the
/// driver scales. Nothing in it changes at run time; every lifecycle state
/// lives in the nodes' atomics. Read its fields freely; build one by hand with
/// [`Graph::new`] and the `with_*` builders.
#[non_exhaustive]
pub struct Graph<const N: usize, T: Topology<N> = Ordered<N, { shape::ALL }>> {
    /// The fixed array of node slots; `None` marks a slot whose node was
    /// `#[cfg]`-ed out (a `disabled` node is `Some`).
    pub nodes: &'static [Option<&'static TaskNode>; N],
    /// The topology that defines spawn order and dependency rows.
    pub topo: T,
    #[cfg(feature = "pool")]
    /// The elastic pools declared in the graph.
    pub pools: &'static [&'static dyn Pool],
    #[cfg(feature = "graph-ref")]
    /// A reference used to enumerate the graph at runtime.
    pub graph_ref: &'static GraphRef,
}

impl<const N: usize, T: Topology<N>> Graph<N, T> {
    /// A graph over `nodes` in the order `topo` gives, with no pools and no
    /// graph reference of its own.
    pub const fn new(nodes: &'static [Option<&'static TaskNode>; N], topo: T) -> Self {
        Self {
            nodes,
            topo,
            #[cfg(feature = "pool")]
            pools: &[],
            #[cfg(feature = "graph-ref")]
            graph_ref: &graph_ref::NO_GRAPH,
        }
    }

    /// The elastic pools the driver scales.
    #[cfg(feature = "pool")]
    pub const fn with_pools(mut self, pools: &'static [&'static dyn Pool]) -> Self {
        self.pools = pools;
        self
    }

    /// The reference that links this graph into the binary-wide chain the
    /// trace hooks and the data-driven gates resolve nodes through.
    #[cfg(feature = "graph-ref")]
    pub const fn with_graph_ref(mut self, graph_ref: &'static GraphRef) -> Self {
        self.graph_ref = graph_ref;
        self
    }

    /// Slot index of `node` in this graph (pointer identity — every node is a
    /// `&'static`), or `None` if it belongs to another graph. The inverse of
    /// indexing [`nodes`](Self::nodes), and the bridge an app-side health view
    /// needs to get from a node back to its [`deps_of`](Self::deps_of) row.
    pub fn index_of(&self, node: &'static TaskNode) -> Option<u8> {
        self.nodes
            .iter()
            .position(|s| s.is_some_and(|n| core::ptr::eq(n, node)))
            .map(|i| i as u8)
    }

    /// The dependency indices of slot `i` — what node `i` declared it needs
    /// spawned first. **Spawn ordering, not runtime coupling**: see the crate
    /// docs on what a `deps:` edge does and does not assert.
    ///
    /// # Panics
    /// If `i >= N` (on an [`Ordered`] topology; [`Flat`] has no rows to index).
    pub fn deps_of(&self, i: u8) -> &'static [u8] {
        self.topo.deps_of(i)
    }

    /// The slot indices in topological order (dependencies before their
    /// dependents; `.rev()` is the teardown order). Declaration order on a
    /// [`Flat`] topology.
    pub fn order(&self) -> impl DoubleEndedIterator<Item = u8> + ExactSizeIterator + '_ {
        (0..N).map(|k| self.topo.order_at(k))
    }

    /// Call `visit` with the slot index of every node that declares slot `i` as
    /// a dependency (direct dependents only). Computed by a forward scan of
    /// the dep rows — no reverse-edge table is stored, so this is
    /// O(N·E) and meant for control paths and status endpoints, not hot loops.
    ///
    /// `&mut dyn FnMut` rather than a generic: one instantiation, no
    /// monomorphization per call site.
    pub fn dependents_of(&self, i: u8, visit: &mut dyn FnMut(u8)) {
        for j in 0..N {
            if self.topo.deps_of(j as u8).contains(&i) {
                visit(j as u8);
            }
        }
    }

    /// Iterate the live nodes with their slot indices, skipping `#[cfg]`-ed-out
    /// slots — the ergonomic form of `GRAPH.nodes.iter().enumerate()` for a
    /// status endpoint that needs the index (to reach `deps`) alongside the node.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (u8, &'static TaskNode)> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|n| (i as u8, n)))
    }

    /// Every node declaring `signal` in its `writes:`, by slot and node.
    /// Matched by address, so it is the *static* that is compared, not the
    /// path text.
    #[cfg(feature = "coupling")]
    pub fn writers_of(&self, signal: &Coupling, visit: &mut dyn FnMut(u8, &'static TaskNode)) {
        for (i, node) in self.iter_nodes() {
            if node.has_entry(signal, true) {
                visit(i, node);
            }
        }
    }

    /// Every node declaring `signal` in its `reads:`. The counterpart to
    /// [`writers_of`](Self::writers_of); together they answer the structural
    /// questions about one signal — who produces it, who consumes it, which
    /// pairs are coupled in a loop.
    #[cfg(feature = "coupling")]
    pub fn readers_of(&self, signal: &Coupling, visit: &mut dyn FnMut(u8, &'static TaskNode)) {
        for (i, node) in self.iter_nodes() {
            if node.has_entry(signal, false) {
                visit(i, node);
            }
        }
    }

    /// Every policed node that is currently stale: it declares
    /// `beat_timeout:`, is running and not detached, and has not beaten within
    /// that budget. Empty means [`all_fresh`](Self::all_fresh).
    ///
    /// Synchronous (atomics plus one clock read per policed node), so it can
    /// sit in the feeder of a hardware watchdog. It grants a pending `beat()`
    /// as it looks, but unlike the monitor's sweep it does not poll `observed`
    /// writes: an observed-beat node is credited only by a running
    /// [`Supervisor::monitor`], and without one it reads stale here once its
    /// budget passes. A node without `beat_timeout:` never appears, and
    /// neither does a stopped, paused or detached one.
    #[cfg(feature = "liveness-monitor")]
    pub fn stale_nodes(&self) -> impl Iterator<Item = &'static TaskNode> + '_ {
        self.nodes.iter().flatten().copied().filter(|n| {
            n.beat_timeout()
                .is_some_and(|budget| !n.is_detached() && n.is_stale(budget))
        })
    }

    /// `true` when no policed node is stale ([`stale_nodes`](Self::stale_nodes)
    /// is empty): the AND-gate for a hardware watchdog feed. Feed only while it
    /// holds, and a node that stops beating resets the chip instead of being
    /// kept alive by an unconditional feeder:
    ///
    /// ```ignore
    /// loop {
    ///     if GRAPH.all_fresh() {
    ///         wd.feed(Duration::from_secs(8));
    ///     }
    ///     Timer::after_secs(2).await;
    /// }
    /// ```
    ///
    /// Unlike [`Supervisor::monitor`], it has no `beat_window:` hysteresis:
    /// size the hardware timeout to cover the jitter the window would absorb.
    #[cfg(feature = "liveness-monitor")]
    pub fn all_fresh(&self) -> bool {
        self.stale_nodes().next().is_none()
    }
}

// ─── Topological sort (Kahn's algorithm, const) ───────────────────────────

#[doc(hidden)]
#[must_use]
pub const fn topo_sort_const<const N: usize>(deps: &[&'static [u8]; N]) -> [u8; N] {
    assert!(
        N <= 256,
        "supervisor graph exceeds 256 node slots (indices are u8)"
    );
    // in_degree[i] = number of deps of node i not yet resolved.
    let mut in_degree = [0u8; N];
    let mut i = 0;
    while i < N {
        in_degree[i] = deps[i].len() as u8;
        i += 1;
    }

    // Queue (fixed array, head/tail indices) seeded with the dependency-free nodes.
    let mut queue = [0u8; N];
    let mut tail = 0;
    i = 0;
    while i < N {
        if in_degree[i] == 0 {
            queue[tail] = i as u8;
            tail += 1;
        }
        i += 1;
    }

    let mut order = [0u8; N];
    let mut produced = 0;
    let mut head = 0;
    while head < tail {
        let node = queue[head] as usize;
        head += 1;
        order[produced] = node as u8;
        produced += 1;

        // Decrement the in-degree of every node that depends on `node`, once
        // per entry naming it, so a repeated dep resolves like any other.
        let mut j = 0;
        while j < N {
            if in_degree[j] != 0 {
                let mut k = 0;
                while k < deps[j].len() {
                    if deps[j][k] as usize == node {
                        in_degree[j] -= 1;
                        if in_degree[j] == 0 {
                            queue[tail] = j as u8;
                            tail += 1;
                        }
                    }
                    k += 1;
                }
            }
            j += 1;
        }
    }

    // A cycle leaves some nodes unproduced. During const eval this panic is a
    // compile error, so cyclic graphs are rejected at build time.
    if produced != N {
        core::panic!("supervisor_graph!: dependency cycle");
    }
    order
}

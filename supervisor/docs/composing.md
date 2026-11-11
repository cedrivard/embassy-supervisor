# Composing graphs across crates

`supervisor_graph!` is one closed invocation, but it does not have to be one closed
*file*. A module (or a whole crate) declares its slice of the graph as a **fragment**,
and one compose site assembles them:

```rust,ignore
// net.rs (or a separate crate)
embassy_supervisor::supervisor_fragment! {
    name: NET_FRAG;
    node NET = Terminate, task: crate::net::net_task,
        resources: [USB_DEV: Peri<'static, USB>];
}

// main.rs: the one compose site per binary
embassy_supervisor::compose_graph! {
    fragments: [NET_FRAG, ::http_stack::HTTP_FRAG],
    graph: {
        node APP = Terminate, deps: [NET], task: app_worker;  // cross-fragment dep
    }
}
```

A fragment emits a `#[macro_export]` relay macro that forwards its items (verbatim,
with their spans) into the compose site's single `supervisor_graph!` expansion. So
**every compile-time pass still sees the whole graph**: cross-fragment deps resolve by
name in either direction (forward references included), duplicate names and shared-slot
shape mismatches error naming the declaring item and, when it came from one, its
fragment; the topological order and the 256-node cap span everything. All statics
(nodes, slots, `GRAPH`) land at the compose site.

Rules and caveats:

- **Paths**: a fragment references its own workers/types with plain `crate::…`, as
  anywhere else in its crate; the macro normalizes it to `$crate`, which is what
  resolves to the fragment's crate at any compose site (`$crate::…` may also be
  written directly). Another crate's items take a fully-qualified `::crate_name::…`.
  No `$` other than `$crate` is permitted (validated).
- **Shared names across a fragment and the compose site**: a resource or a veto gate
  declared both in a fragment and at the compose site must be named through an imported
  bare identifier in both places (`use crate::net::STACK;`, then `STACK`): inside a
  fragment a `crate::` path is carried as `$crate::`, which does not compare equal to
  the same path written at the compose site.
- **`#[cfg(...)]` inside a fragment is evaluated against the COMPOSE crate's features**
  (the tokens expand there). A fragment crate that wants feature-dependent shapes
  exports differently-named fragment variants instead.
- One compose site per binary (it emits the graph statics and, under `trace-hooks`,
  the hook symbols); fragment names are crate-root macros, so prefix them.
- Fragment item syntax is validated at the fragment site with its own spans; only
  name *resolution* waits for the compose site.

### Multiple graphs per binary

`name: IDENT;` as a graph's first item (also `compose_graph! { name: X, … }`) renames
the emitted static and suffixes every generated helper, so several supervisors coexist,
e.g. an always-on primary graph plus a [subordinate sub-graph](crate::guide::recipes#subordinate-sub-graph-under-an-app-state-machine) the app cycles. Rules:

- **The unnamed graph is the primary**: under `trace-hooks` only it emits the
  once-per-binary `_embassy_trace_*` symbols; named graphs are secondary (their nodes
  still resolve in the trace recorders: each `start()` links its graph into the chain
  the recorders walk, and there is no cap on how many).
- **The control mailbox, the scale signal and the bound-readiness signal are shared.**
  Run ONE driver (one `run()`, or one loop over `wait_control()`, the pools and, with
  `bound-deps`, `wait_bind()`) and apply each command, and each `wait_bind()` wake as
  `apply_bind`, to every supervisor in turn: a command naming a node outside a
  supervisor's graph is a safe no-op that counts nothing against a `restart_budget:`.
  Two independent driver loops would race each other for these wakes. A single `run()`
  serves only its own graph: use it as the one driver only when no other graph is
  commanded, scaled or bound; a command for a foreign node is logged and dropped.
- Only the graph static itself is renamed (plus internal generated helpers); node
  and resource-slot statics keep exactly their declared names: `WIFI` in a named
  graph is still `WIFI`. Two graphs reusing a node name in one module is therefore an
  ordinary duplicate-static error; the 256-node cap is per graph.

use embassy_supervisor_syntax::{Dep, GraphSpec, Item, NodeItem, PoolItem, SignalDecl};
use syn::Result as SynResult;

/// The macro crate's features as one value. The entry points pass `ENABLED`;
/// a unit test passes its own, because this crate's lib tests are always
/// built with the features the `embassy-supervisor` dev-dependency forwards,
/// so `cfg!` alone could never show a rejection.
pub(crate) struct Features {
    pub(crate) readiness: bool,
    pub(crate) liveness: bool,
    pub(crate) liveness_monitor: bool,
    pub(crate) enabled_if: bool,
    pub(crate) restart_intensity: bool,
    pub(crate) bound_deps: bool,
    pub(crate) dataflow: bool,
    pub(crate) veto: bool,
    pub(crate) budget: bool,
    pub(crate) coupling: bool,
    pub(crate) coupling_observe: bool,
    pub(crate) data_deps: bool,
    pub(crate) graph_ref: bool,
    pub(crate) pool: bool,
    pub(crate) metadata_names: bool,
    pub(crate) trace: bool,
    pub(crate) trace_hooks: bool,
    pub(crate) trace_self: bool,
    pub(crate) fault_inject: bool,
    pub(crate) local_resources: bool,
    pub(crate) heap_state: bool,
}

/// This build's features.
pub(crate) const ENABLED: Features = Features {
    readiness: cfg!(feature = "readiness"),
    liveness: cfg!(feature = "liveness"),
    liveness_monitor: cfg!(feature = "liveness-monitor"),
    enabled_if: cfg!(feature = "enabled-if"),
    restart_intensity: cfg!(feature = "restart-intensity"),
    bound_deps: cfg!(feature = "bound-deps"),
    dataflow: cfg!(feature = "dataflow"),
    veto: cfg!(feature = "veto"),
    budget: cfg!(feature = "budget"),
    coupling: cfg!(feature = "coupling"),
    coupling_observe: cfg!(feature = "coupling-observe"),
    data_deps: cfg!(feature = "data-deps"),
    graph_ref: cfg!(feature = "graph-ref"),
    pool: cfg!(feature = "pool"),
    metadata_names: cfg!(feature = "metadata-names"),
    trace: cfg!(feature = "trace"),
    trace_hooks: cfg!(feature = "trace-hooks"),
    trace_self: cfg!(feature = "trace-self"),
    fault_inject: cfg!(feature = "fault-inject"),
    local_resources: cfg!(feature = "local-resources"),
    heap_state: cfg!(feature = "heap-state"),
};

pub fn gate(spec: &GraphSpec, f: &Features) -> SynResult<()> {
    for (kw, _) in [&spec.observe_writes, &spec.observe_reads]
        .into_iter()
        .flatten()
    {
        require_observe(kw, f)?;
    }
    for item in &spec.items {
        match item {
            Item::Node(n) => gate_node(n, f)?,
            Item::Pool(p) => gate_pool(p, f)?,
            Item::Executor(_) => {}
        }
    }
    Ok(())
}

/// Feature-required texts for clauses the emitter also handles: [`gate_node`]
/// reports an ungated clause with one, and lib.rs's deferred `compile_error!`
/// for a cfg-gated clause (`GatedBuilder::err`) carries the same text.
pub(crate) const BEAT_TIMEOUT_NEEDS: &str = "`beat_timeout:` requires the `liveness-monitor` \
     feature (embassy-supervisor feature `liveness-monitor`) — the supervisor then reports \
     this node once it has been running that long without a beat()";
pub(crate) const BEAT_WINDOW_NEEDS: &str = "`beat_window:` requires the `liveness-monitor` \
     feature (embassy-supervisor feature `liveness-monitor`) — it sets how many consecutive \
     stale sweeps are reported on";
pub(crate) const ENABLED_IF_NEEDS: &str = "`enabled_if:` requires the `enabled-if` feature \
     (embassy-supervisor feature `enabled-if`) — start() then evaluates the predicate and \
     disables the node when it returns false";
pub(crate) const RESTART_BUDGET_NEEDS: &str = "`restart_budget:` requires the \
     `restart-intensity` feature (embassy-supervisor feature `restart-intensity`) — \
     Supervisor::restart then refuses the N+1th restart inside the window with \
     FaultKind::RestartStorm";

fn gate_node(n: &NodeItem, f: &Features) -> SynResult<()> {
    gate_deps(&n.deps, f)?;
    gate_coupling(&n.reads, &n.writes, f)?;
    require_discover(&n.discover, f)?;
    require_dataflow(&n.dataflow, f)?;
    gate_resource_list(&n.resources, f)?;
    if let Some((k, _, _)) = &n.state {
        require_heap_state(k, f)?;
    }
    if let Some(row) = &n.ready_on_write {
        require_feature_ungated(
            &row.cfg,
            &row.kw,
            f.coupling_observe,
            "`ready_on_write` requires the `coupling-observe` feature \
             (embassy-supervisor feature of the same name) — readiness \
             is asserted by the monitor sweep seeing an `observed beat` \
             write advance. A body that beats through its verbs asserts \
             its own readiness, with `set_ready()` at the write",
        )?;
        require_feature_ungated(
            &row.cfg,
            &row.kw,
            f.readiness,
            "`ready_on_write` requires the `readiness` feature \
             (embassy-supervisor feature `readiness`) — it is what \
             set_ready() and `deps: [X ready]` come from",
        )?;
    }
    if let Some(bt) = &n.beat_timeout {
        require_feature_ungated(&bt.cfg, &bt.kw, f.liveness_monitor, BEAT_TIMEOUT_NEEDS)?;
    }
    if let Some(bw) = &n.beat_window {
        require_feature_ungated(&bw.cfg, &bw.kw, f.liveness_monitor, BEAT_WINDOW_NEEDS)?;
    }
    if let Some(e) = &n.enabled_if {
        require_feature_ungated(&e.cfg, &e.kw, f.enabled_if, ENABLED_IF_NEEDS)?;
    }
    if let Some(rb) = &n.restart_budget {
        require_feature_ungated(&rb.cfg, &rb.kw, f.restart_intensity, RESTART_BUDGET_NEEDS)?;
    }
    Ok(())
}

fn require_feature_ungated<T: quote::ToTokens>(
    cfg: &[syn::Attribute],
    tok: &T,
    feature_on: bool,
    msg: &str,
) -> SynResult<()> {
    if !feature_on && cfg.is_empty() {
        return Err(syn::Error::new_spanned(tok, msg));
    }
    Ok(())
}

fn gate_pool(p: &PoolItem, f: &Features) -> SynResult<()> {
    gate_deps(&p.deps, f)?;
    gate_coupling(&p.reads, &p.writes, f)?;
    require_discover(&p.discover, f)?;
    require_dataflow(&p.dataflow, f)?;
    gate_resource_list(&p.resources, f)?;
    if let Some((k, _, _)) = &p.state {
        require_heap_state(k, f)?;
    }
    Ok(())
}

fn gate_resource_list(
    resources: &[embassy_supervisor_syntax::ResourceDecl],
    f: &Features,
) -> SynResult<()> {
    for r in resources {
        if let Some(l) = &r.local {
            require_local(l, f)?;
        }
        if let Some(d) = &r.divisible {
            require_budget(d, f)?;
        }
    }
    Ok(())
}

fn gate_deps(deps: &[Dep], f: &Features) -> SynResult<()> {
    for d in deps {
        if let Some(m) = &d.ready
            && !f.readiness
        {
            return Err(syn::Error::new_spanned(
                m,
                "the `ready` dep marker requires the `readiness` feature \
                 (embassy-supervisor feature `readiness`) — bring-up then \
                 awaits the dep's set_ready() before spawning this node",
            ));
        }
        if let Some(m) = &d.bound
            && !f.bound_deps
        {
            return Err(syn::Error::new_spanned(
                m,
                "the `bound` dep marker requires the `bound-deps` feature \
                 (embassy-supervisor feature `bound-deps`) — readiness \
                 then propagates across this edge: the dep's clear_ready() \
                 stops this node, its set_ready() brings it back",
            ));
        }
    }
    Ok(())
}

fn gate_coupling(reads: &[SignalDecl], writes: &[SignalDecl], f: &Features) -> SynResult<()> {
    for (clause, list) in [("reads", reads), ("writes", writes)] {
        if let Some(first) = list.first() {
            require_coupling(&first.path, clause, f)?;
        }
        for s in list {
            if let Some(o) = &s.observed {
                require_observe(o, f)?;
            }
            if let Some(v) = &s.veto {
                require_veto(v, f)?;
            }
        }
    }
    Ok(())
}

fn require_discover(
    discover: &Option<embassy_supervisor_syntax::Gated<embassy_supervisor_syntax::kw::discover>>,
    f: &Features,
) -> SynResult<()> {
    if let Some(k) = discover {
        require_feature_ungated(
            &k.cfg,
            &k.kw,
            f.dataflow,
            "`discover` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`) — it binds the \
             coupling tables the task fn's `#[dataflow]` attribute derives, \
             in place of `reads:`/`writes:` declarations, which may then \
             only add markers",
        )?;
    }
    Ok(())
}

fn require_dataflow(
    dataflow: &[embassy_supervisor_syntax::AdoptedFn],
    f: &Features,
) -> SynResult<()> {
    if let Some(first) = dataflow.first().map(|a| &a.path)
        && !f.dataflow
    {
        return Err(syn::Error::new_spanned(
            first,
            "`dataflow:` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`) — it adopts the \
             named fns' `#[dataflow]` tables, so their accesses attribute to \
             this item",
        ));
    }
    Ok(())
}

fn require_coupling<T: quote::ToTokens>(tok: &T, clause: &str, f: &Features) -> SynResult<()> {
    if f.coupling {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        format!(
            "`{clause}:` requires the `coupling` feature (embassy-supervisor \
             feature `coupling`) — it declares the signals this node exchanges \
             with the rest of the graph"
        ),
    ))
}

fn require_observe<T: quote::ToTokens>(tok: &T, f: &Features) -> SynResult<()> {
    if f.coupling_observe {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "the `observed` entry marker requires the `coupling-observe` feature \
         (embassy-supervisor feature `coupling-observe`) — it names an accessor \
         that answers whether the signal moved, which an `observed beat` write \
         turns into the node's heartbeat",
    ))
}

fn require_local<T: quote::ToTokens>(tok: &T, f: &Features) -> SynResult<()> {
    if f.local_resources {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`local` resources emit an `unsafe impl Sync` — opt in by \
         enabling embassy-supervisor's `local-resources` feature",
    ))
}

fn require_veto<T: quote::ToTokens>(tok: &T, f: &Features) -> SynResult<()> {
    if f.veto {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "the `veto` entry marker requires the `veto` feature (embassy-supervisor \
         feature `veto`) — it gives this writer one contributor slot of a \
         `VetoGate<N>`, asserted while any writer holds it",
    ))
}

fn require_budget<T: quote::ToTokens>(tok: &T, f: &Features) -> SynResult<()> {
    if f.budget {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`divisible` resources require the `budget` feature (embassy-supervisor \
         feature `budget`) — the slot is a `Budget<K>` whose shares the \
         supervisor releases when a holder stops",
    ))
}

fn require_heap_state<T: quote::ToTokens>(tok: &T, f: &Features) -> SynResult<()> {
    if f.heap_state {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`state:` requires the `heap-state` feature \
         (embassy-supervisor feature `heap-state`), which boxes the state \
         per activation and drops it when the activation ends",
    ))
}

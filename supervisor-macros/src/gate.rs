use embassy_supervisor_syntax::{Dep, GraphSpec, Item, NodeItem, PoolItem, SignalDecl};
use syn::Result as SynResult;

pub fn gate(spec: &GraphSpec) -> SynResult<()> {
    for (kw, _) in [&spec.observe_writes, &spec.observe_reads]
        .into_iter()
        .flatten()
    {
        require_observe(kw)?;
    }
    for item in &spec.items {
        match item {
            Item::Node(n) => gate_node(n)?,
            Item::Pool(p) => gate_pool(p)?,
            Item::Executor(_) => {}
        }
    }
    Ok(())
}

fn gate_node(n: &NodeItem) -> SynResult<()> {
    gate_deps(&n.deps)?;
    gate_coupling(&n.reads, &n.writes)?;
    require_discover(&n.discover)?;
    require_dataflow(&n.dataflow)?;
    gate_resources(n)?;
    if let Some((k, _, _)) = &n.state {
        require_heap_state(k)?;
    }
    if let Some(row) = &n.ready_on_write {
        require_feature_ungated(
            &row.cfg,
            &row.kw,
            cfg!(feature = "coupling-observe"),
            "`ready_on_write` requires the `coupling-observe` feature \
             (embassy-supervisor feature of the same name) — readiness \
             is asserted by the monitor sweep seeing an `observed beat` \
             write advance. A body that beats through its verbs asserts \
             its own readiness, with `set_ready()` at the write",
        )?;
        require_feature_ungated(
            &row.cfg,
            &row.kw,
            cfg!(feature = "readiness"),
            "`ready_on_write` requires the `readiness` feature \
             (embassy-supervisor feature `readiness`) — it is what \
             set_ready() and `deps: [X ready]` come from",
        )?;
    }
    if let Some(bt) = &n.beat_timeout {
        require_feature_ungated(
            &bt.cfg,
            &bt.kw,
            cfg!(feature = "liveness-monitor"),
            "`beat_timeout:` requires the `liveness-monitor` feature \
             (embassy-supervisor feature `liveness-monitor`) — the \
             supervisor then reports this node once it has been running \
             that long without a beat()",
        )?;
    }
    if let Some(bw) = &n.beat_window {
        require_feature_ungated(
            &bw.cfg,
            &bw.kw,
            cfg!(feature = "liveness-monitor"),
            "`beat_window:` requires the `liveness-monitor` feature \
             (embassy-supervisor feature `liveness-monitor`) — it sets \
             how many consecutive stale sweeps are reported on",
        )?;
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

fn gate_pool(p: &PoolItem) -> SynResult<()> {
    gate_deps(&p.deps)?;
    gate_coupling(&p.reads, &p.writes)?;
    require_discover(&p.discover)?;
    require_dataflow(&p.dataflow)?;
    gate_resource_list(&p.resources)?;
    if let Some((k, _, _)) = &p.state {
        require_heap_state(k)?;
    }
    Ok(())
}

fn gate_resources(n: &NodeItem) -> SynResult<()> {
    gate_resource_list(&n.resources)
}

fn gate_resource_list(resources: &[embassy_supervisor_syntax::ResourceDecl]) -> SynResult<()> {
    for r in resources {
        if let Some(l) = &r.local {
            require_local(l)?;
        }
        if let Some(d) = &r.divisible {
            require_budget(d)?;
        }
    }
    Ok(())
}

fn gate_deps(deps: &[Dep]) -> SynResult<()> {
    for d in deps {
        if let Some(m) = &d.ready
            && !cfg!(feature = "readiness")
        {
            return Err(syn::Error::new_spanned(
                m,
                "the `ready` dep marker requires the `readiness` feature \
                 (embassy-supervisor feature `readiness`) — bring-up then \
                 awaits the dep's set_ready() before spawning this node",
            ));
        }
        if let Some(m) = &d.bound
            && !cfg!(feature = "bound-deps")
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

fn gate_coupling(reads: &[SignalDecl], writes: &[SignalDecl]) -> SynResult<()> {
    for (clause, list) in [("reads", reads), ("writes", writes)] {
        if let Some(first) = list.first() {
            require_coupling(&first.path, clause)?;
        }
        for s in list {
            if let Some(o) = &s.observed {
                require_observe(o)?;
            }
            if let Some(v) = &s.veto {
                require_veto(v)?;
            }
        }
    }
    Ok(())
}

fn require_discover(
    discover: &Option<embassy_supervisor_syntax::Gated<embassy_supervisor_syntax::kw::discover>>,
) -> SynResult<()> {
    if let Some(k) = discover {
        require_feature_ungated(
            &k.cfg,
            &k.kw,
            cfg!(feature = "dataflow"),
            "`discover` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`) — it binds the \
             coupling tables the task fn's `#[dataflow]` attribute derives, \
             in place of `reads:`/`writes:` declarations, which may then \
             only add markers",
        )?;
    }
    Ok(())
}

fn require_dataflow(dataflow: &[embassy_supervisor_syntax::AdoptedFn]) -> SynResult<()> {
    if let Some(first) = dataflow.first().map(|f| &f.path)
        && !cfg!(feature = "dataflow")
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

fn require_coupling<T: quote::ToTokens>(tok: &T, clause: &str) -> SynResult<()> {
    if cfg!(feature = "coupling") {
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

fn require_observe<T: quote::ToTokens>(tok: &T) -> SynResult<()> {
    if cfg!(feature = "coupling-observe") {
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

fn require_local<T: quote::ToTokens>(tok: &T) -> SynResult<()> {
    if cfg!(feature = "local-resources") {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`local` resources emit an `unsafe impl Sync` — opt in by \
         enabling embassy-supervisor's `local-resources` feature",
    ))
}

fn require_veto<T: quote::ToTokens>(tok: &T) -> SynResult<()> {
    if cfg!(feature = "veto") {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "the `veto` entry marker requires the `veto` feature (embassy-supervisor \
         feature `veto`) — it gives this writer one contributor slot of a \
         `VetoGate<N>`, asserted while any writer holds it",
    ))
}

fn require_budget<T: quote::ToTokens>(tok: &T) -> SynResult<()> {
    if cfg!(feature = "budget") {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`divisible` resources require the `budget` feature (embassy-supervisor \
         feature `budget`) — the slot is a `Budget<K>` whose shares the \
         supervisor releases when a holder stops",
    ))
}

fn require_heap_state<T: quote::ToTokens>(tok: &T) -> SynResult<()> {
    if cfg!(feature = "heap-state") {
        return Ok(());
    }
    Err(syn::Error::new_spanned(
        tok,
        "`state:` requires the `heap-state` feature \
         (embassy-supervisor feature `heap-state`) — per-activation \
         boxed state, reclaimed on task exit",
    ))
}

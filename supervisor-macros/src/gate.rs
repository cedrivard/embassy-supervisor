use embassy_supervisor_syntax::{
    AdoptedFn, Dep, Gated, GraphSpec, Item, NodeItem, PoolItem, ResourceDecl, SignalDecl,
    StateInit, kw,
};
use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::{Attribute, Result as SynResult};

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

#[cfg(test)]
impl Features {
    /// Every feature on.
    pub(crate) const fn all() -> Self {
        Self::uniform(true)
    }

    /// Every feature off.
    pub(crate) const fn none() -> Self {
        Self::uniform(false)
    }

    const fn uniform(on: bool) -> Self {
        Self {
            readiness: on,
            liveness: on,
            liveness_monitor: on,
            enabled_if: on,
            restart_intensity: on,
            bound_deps: on,
            dataflow: on,
            veto: on,
            budget: on,
            coupling: on,
            coupling_observe: on,
            data_deps: on,
            graph_ref: on,
            pool: on,
            metadata_names: on,
            trace: on,
            trace_hooks: on,
            trace_self: on,
            fault_inject: on,
            local_resources: on,
            heap_state: on,
        }
    }
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

/// Check `spec`'s feature-gated constructs against `f`. A construct with no
/// `#[cfg]` on it or on its item is rejected here; a gated one is dropped
/// from `spec`, and its rejection comes back as `compile_error!`s under the
/// same cfgs, for the caller to emit.
pub fn gate(spec: &mut GraphSpec, f: &Features) -> SynResult<TokenStream2> {
    // The graph-level defaults cannot be cfg-gated.
    if !f.coupling_observe {
        for (default, label) in [
            (&spec.observe_writes, "`observe writes:`"),
            (&spec.observe_reads, "`observe reads:`"),
        ] {
            if let Some((kw, _)) = default {
                return Err(syn::Error::new_spanned(kw, observe_needs(label)));
            }
        }
    }
    let mut deferred = TokenStream2::new();
    for item in &mut spec.items {
        match item {
            Item::Node(n) => gate_node(&mut deferred, n, f)?,
            Item::Pool(p) => gate_pool(&mut deferred, p, f)?,
            Item::Executor(_) => {}
        }
    }
    Ok(deferred)
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
pub(crate) const READY_ON_WRITE_NEEDS: &str = "`ready_on_write` requires the \
     `coupling-observe` and `readiness` features (embassy-supervisor features of the \
     same names) — readiness is asserted by the monitor sweep seeing an `observed beat` \
     write advance";

/// Not a builder the emitter defers: [`gate_node`] drops the clause instead.
const READY_ON_WRITE_MONITOR_NEEDS: &str = "`ready_on_write` requires the \
     `liveness-monitor` feature: the monitor sweep is what asserts readiness";

const READY_NEEDS: &str = "the `ready` dep marker requires the `readiness` feature \
     (embassy-supervisor feature `readiness`) — bring-up then \
     awaits the dep's set_ready() before spawning this node";
const BOUND_NEEDS: &str = "the `bound` dep marker requires the `bound-deps` feature \
     (embassy-supervisor feature `bound-deps`) — readiness \
     then propagates across this edge: the dep's clear_ready() \
     stops this node, its set_ready() brings it back";
const VETO_NEEDS: &str = "the `veto` entry marker requires the `veto` feature \
     (embassy-supervisor feature `veto`) — it gives this writer one contributor slot \
     of a `VetoGate<N>`, asserted while any writer holds it";
const DISCOVER_NEEDS: &str = "`discover` requires the `dataflow` feature \
     (embassy-supervisor feature `dataflow`) — it binds the \
     coupling tables the task fn's `#[dataflow]` attribute derives, \
     in place of `reads:`/`writes:` declarations, which may then \
     only add markers";
const DATAFLOW_NEEDS: &str = "`dataflow:` requires the `dataflow` feature \
     (embassy-supervisor feature `dataflow`) — it adopts the \
     named fns' `#[dataflow]` tables, so their accesses attribute to \
     this item";
const LOCAL_NEEDS: &str = "`local` resources emit an `unsafe impl Sync` — opt in by \
     enabling embassy-supervisor's `local-resources` feature";
const BUDGET_NEEDS: &str = "`divisible` resources require the `budget` feature \
     (embassy-supervisor feature `budget`) — the slot is a `Budget<K>` whose shares \
     the supervisor releases when a holder stops";
const STATE_NEEDS: &str = "`state:` requires the `heap-state` feature \
     (embassy-supervisor feature `heap-state`), which boxes the state \
     per activation and drops it when the activation ends";

fn coupling_needs(clause: &str) -> String {
    format!(
        "`{clause}:` requires the `coupling` feature (embassy-supervisor \
         feature `coupling`) — it declares the signals this node exchanges \
         with the rest of the graph"
    )
}

/// The `coupling-observe` requirement, naming the construct the author wrote:
/// the `observed` entry marker or a graph-level `observe` default.
fn observe_needs(what: &str) -> String {
    format!(
        "{what} requires the `coupling-observe` feature (embassy-supervisor feature \
         `coupling-observe`): it names an accessor that answers whether the signal \
         moved, which an `observed beat` write turns into the node's heartbeat"
    )
}

/// The `#[cfg(..)]` attributes among `attrs`: the ones that compile a
/// construct out.
fn cfgs(attrs: &[Attribute]) -> Vec<&Attribute> {
    attrs.iter().filter(|a| a.path().is_ident("cfg")).collect()
}

/// Reject now when nothing gates the construct; otherwise defer to a
/// `compile_error!` under the same cfgs. The caller then drops the construct
/// from the spec: where a cfg is false rustc strips it anyway, and where it
/// is true the deferred error fires.
fn unavailable(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    own_cfg: &[Attribute],
    tok: &impl ToTokens,
    msg: &str,
) -> SynResult<()> {
    let (item_cfg, own_cfg) = (cfgs(item_cfg), cfgs(own_cfg));
    if item_cfg.is_empty() && own_cfg.is_empty() {
        return Err(syn::Error::new_spanned(tok, msg));
    }
    let span = tok.span();
    let msg = syn::LitStr::new(msg, span);
    deferred.extend(quote::quote_spanned!(span=>
        #(#item_cfg)* #(#own_cfg)* ::core::compile_error!(#msg);
    ));
    Ok(())
}

/// `true` when the feature is on. Otherwise [`unavailable`], and `false` once
/// the error is deferred: the caller drops the construct.
fn available(
    deferred: &mut TokenStream2,
    on: bool,
    item_cfg: &[Attribute],
    own_cfg: &[Attribute],
    tok: &impl ToTokens,
    msg: &str,
) -> SynResult<bool> {
    if !on {
        unavailable(deferred, item_cfg, own_cfg, tok, msg)?;
    }
    Ok(on)
}

fn gate_node(deferred: &mut TokenStream2, n: &mut NodeItem, f: &Features) -> SynResult<()> {
    let item_cfg = &n.cfg;
    gate_deps(deferred, item_cfg, &mut n.deps, f)?;
    gate_coupling(deferred, item_cfg, &mut n.reads, &mut n.writes, f)?;
    gate_discover(deferred, item_cfg, &mut n.discover, f)?;
    gate_dataflow(deferred, item_cfg, &mut n.dataflow, f)?;
    gate_resource_list(deferred, item_cfg, &mut n.resources, f)?;
    gate_state(deferred, item_cfg, &mut n.state, f)?;
    // The emitter defers these clauses itself (`builder_clause_tokens`), so
    // only an ungated one is rejected here.
    if let Some(row) = &n.ready_on_write {
        require_feature_ungated(
            item_cfg,
            &row.cfg,
            &row.kw,
            f.coupling_observe && f.readiness,
            READY_ON_WRITE_NEEDS,
        )?;
    }
    // The builder compiles without the monitor, but only the monitor sweep
    // asserts the readiness: refused here, or deferred and dropped when gated.
    if let Some(row) = &n.ready_on_write
        && !available(
            deferred,
            f.liveness_monitor,
            item_cfg,
            &row.cfg,
            &row.kw,
            READY_ON_WRITE_MONITOR_NEEDS,
        )?
    {
        n.ready_on_write = None;
    }
    if let Some(bt) = &n.beat_timeout {
        require_feature_ungated(
            item_cfg,
            &bt.cfg,
            &bt.kw,
            f.liveness_monitor,
            BEAT_TIMEOUT_NEEDS,
        )?;
    }
    if let Some(bw) = &n.beat_window {
        require_feature_ungated(
            item_cfg,
            &bw.cfg,
            &bw.kw,
            f.liveness_monitor,
            BEAT_WINDOW_NEEDS,
        )?;
    }
    if let Some(e) = &n.enabled_if {
        require_feature_ungated(item_cfg, &e.cfg, &e.kw, f.enabled_if, ENABLED_IF_NEEDS)?;
    }
    if let Some(rb) = &n.restart_budget {
        require_feature_ungated(
            item_cfg,
            &rb.cfg,
            &rb.kw,
            f.restart_intensity,
            RESTART_BUDGET_NEEDS,
        )?;
    }
    Ok(())
}

/// Reject a clause whose feature is off when neither it nor its item is
/// `#[cfg]`-gated; a gated one is left to the emitter's deferred error.
fn require_feature_ungated<T: ToTokens>(
    item_cfg: &[Attribute],
    cfg: &[Attribute],
    tok: &T,
    feature_on: bool,
    msg: &str,
) -> SynResult<()> {
    if !feature_on && cfgs(item_cfg).is_empty() && cfgs(cfg).is_empty() {
        return Err(syn::Error::new_spanned(tok, msg));
    }
    Ok(())
}

fn gate_pool(deferred: &mut TokenStream2, p: &mut PoolItem, f: &Features) -> SynResult<()> {
    let item_cfg = &p.cfg;
    gate_deps(deferred, item_cfg, &mut p.deps, f)?;
    gate_coupling(deferred, item_cfg, &mut p.reads, &mut p.writes, f)?;
    gate_discover(deferred, item_cfg, &mut p.discover, f)?;
    gate_dataflow(deferred, item_cfg, &mut p.dataflow, f)?;
    gate_resource_list(deferred, item_cfg, &mut p.resources, f)?;
    gate_state(deferred, item_cfg, &mut p.state, f)?;
    Ok(())
}

fn gate_resource_list(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    resources: &mut Vec<ResourceDecl>,
    f: &Features,
) -> SynResult<()> {
    for r in std::mem::take(resources) {
        if let Some(l) = &r.local
            && !available(
                deferred,
                f.local_resources,
                item_cfg,
                &r.cfg,
                l,
                LOCAL_NEEDS,
            )?
        {
            continue;
        }
        if let Some(d) = &r.divisible
            && !available(deferred, f.budget, item_cfg, &r.cfg, d, BUDGET_NEEDS)?
        {
            continue;
        }
        resources.push(r);
    }
    Ok(())
}

fn gate_deps(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    deps: &mut Vec<Dep>,
    f: &Features,
) -> SynResult<()> {
    for d in std::mem::take(deps) {
        if let Some(m) = &d.ready
            && !available(deferred, f.readiness, item_cfg, &d.cfg, m, READY_NEEDS)?
        {
            continue;
        }
        if let Some(m) = &d.bound
            && !available(deferred, f.bound_deps, item_cfg, &d.cfg, m, BOUND_NEEDS)?
        {
            continue;
        }
        deps.push(d);
    }
    Ok(())
}

fn gate_coupling(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    reads: &mut Vec<SignalDecl>,
    writes: &mut Vec<SignalDecl>,
    f: &Features,
) -> SynResult<()> {
    for (clause, list) in [("reads", reads), ("writes", writes)] {
        let Some(first) = list.first() else {
            continue;
        };
        // The list has no cfg of its own: only its item's can gate it.
        if !available(
            deferred,
            f.coupling,
            item_cfg,
            &[],
            &first.path,
            &coupling_needs(clause),
        )? {
            list.clear();
            continue;
        }
        for s in std::mem::take(list) {
            if let Some(o) = &s.observed
                && !available(
                    deferred,
                    f.coupling_observe,
                    item_cfg,
                    &s.cfg,
                    o,
                    &observe_needs("the `observed` entry marker"),
                )?
            {
                continue;
            }
            if let Some(v) = &s.veto
                && !available(deferred, f.veto, item_cfg, &s.cfg, v, VETO_NEEDS)?
            {
                continue;
            }
            list.push(s);
        }
    }
    Ok(())
}

fn gate_discover(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    discover: &mut Option<Gated<kw::discover>>,
    f: &Features,
) -> SynResult<()> {
    if let Some(k) = discover.as_ref()
        && !available(
            deferred,
            f.dataflow,
            item_cfg,
            &k.cfg,
            &k.kw,
            DISCOVER_NEEDS,
        )?
    {
        *discover = None;
    }
    Ok(())
}

fn gate_dataflow(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    dataflow: &mut Vec<AdoptedFn>,
    f: &Features,
) -> SynResult<()> {
    for a in std::mem::take(dataflow) {
        if available(
            deferred,
            f.dataflow,
            item_cfg,
            &a.cfg,
            &a.path,
            DATAFLOW_NEEDS,
        )? {
            dataflow.push(a);
        }
    }
    Ok(())
}

fn gate_state(
    deferred: &mut TokenStream2,
    item_cfg: &[Attribute],
    state: &mut Option<(kw::state, syn::Type, StateInit)>,
    f: &Features,
) -> SynResult<()> {
    // `state:` has no cfg of its own: only its item's can gate it.
    if let Some((k, _, _)) = state.as_ref()
        && !available(deferred, f.heap_state, item_cfg, &[], k, STATE_NEEDS)?
    {
        *state = None;
    }
    Ok(())
}

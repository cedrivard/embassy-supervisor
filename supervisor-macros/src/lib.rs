mod gate;

use embassy_supervisor_syntax::{
    AdoptedFn, Dep, GraphSpec, Item, NodeItem, PoolItem, ResourceDecl, ResourceKind, SignalDecl,
    StateInit, TaskSource, VerbTable, item_executor, item_ident_cfg, item_resources, kw,
    name_string, node_param, normalize_fragment_crate, rewrite_verb_calls, substitute_dollar_crate,
};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Ident, LitInt, Meta, Path, Result as SynResult, Token};

const LOCAL_SLOT_TYPE: &str = "__SvLocalResourceSlot";

struct HelperIdents {
    local_slot: Ident,
    try_box: Ident,
    try_box_zeroed: Ident,
    alloc_alias: Ident,
    nodes: Ident,
    graph_ref: Ident,
}

impl HelperIdents {
    fn new(graph_name: Option<&Ident>) -> Self {
        match graph_name {
            None => Self {
                local_slot: format_ident!("{LOCAL_SLOT_TYPE}"),
                try_box: format_ident!("__sv_try_box"),
                try_box_zeroed: format_ident!("__sv_try_box_zeroed"),
                alloc_alias: format_ident!("__sv_alloc"),
                nodes: format_ident!("NODES"),
                graph_ref: format_ident!("GRAPH_REF"),
            },
            Some(n) => {
                let lower = n.to_string().to_lowercase();
                Self {
                    local_slot: format_ident!("{LOCAL_SLOT_TYPE}{}", n),
                    try_box: format_ident!("__sv_try_box_{lower}"),
                    try_box_zeroed: format_ident!("__sv_try_box_zeroed_{lower}"),
                    alloc_alias: format_ident!("__sv_alloc_{lower}"),
                    nodes: format_ident!("__SV_NODES_{}", n),
                    graph_ref: format_ident!("__SV_GRAPH_REF_{}", n),
                }
            }
        }
    }
}

fn substitute_it(expr: &Expr, target: &TokenStream2) -> TokenStream2 {
    fn walk(tokens: TokenStream2, target: &TokenStream2) -> TokenStream2 {
        tokens
            .into_iter()
            .map(|tt| match tt {
                proc_macro2::TokenTree::Ident(ref id) if id == "it" => {
                    let mut group = proc_macro2::Group::new(
                        proc_macro2::Delimiter::Parenthesis,
                        quote!(&#target),
                    );
                    group.set_span(id.span());
                    proc_macro2::TokenTree::Group(group)
                }
                proc_macro2::TokenTree::Group(g) => {
                    let mut new = proc_macro2::Group::new(g.delimiter(), walk(g.stream(), target));
                    new.set_span(g.span());
                    proc_macro2::TokenTree::Group(new)
                }
                other => other,
            })
            .collect()
    }
    walk(quote!(#expr), target)
}

fn inject_call_with(task: &Expr, lead: &[TokenStream2]) -> SynResult<TokenStream2> {
    match task {
        Expr::Path(_) => Ok(quote!(#task(#(#lead),*))),
        Expr::Call(c) => {
            let f = &c.func;
            let mut args: Vec<TokenStream2> = lead.to_vec();
            args.extend(c.args.iter().map(|a| quote!(#a)));
            Ok(quote!(#f(#(#args),*)))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "expected a task-fn path or a partial call like `f(extra_args)`",
        )),
    }
}

fn cfg_aware_len<'a>(cfgs: impl Iterator<Item = &'a Vec<Attribute>> + Clone) -> TokenStream2 {
    if !cfgs.clone().any(|c| cfg_predicate(c).is_some()) {
        let n = cfgs.count();
        return quote!(#n);
    }
    let terms: Vec<TokenStream2> = cfgs
        .map(|c| match cfg_predicate(c) {
            None => quote!(1usize),
            Some(pred) => quote!({
                #[cfg(#pred)]
                {
                    1usize
                }
                #[cfg(not(#pred))]
                {
                    0usize
                }
            }),
        })
        .collect();
    quote!(0usize #(+ #terms)*)
}

struct EmitCtx<'a> {
    cfg: &'a [Attribute],
    cr: &'a TokenStream2,
    owner: &'a Ident,
}

#[derive(Default)]
struct ObserveDefaults {
    writes: Option<Expr>,
    reads: Option<Expr>,
}

fn marker_array_tokens(
    ctx: &EmitCtx<'_>,
    deps: &[Dep],
    pool_names: &std::collections::HashSet<String>,
    select: impl Fn(&Dep) -> bool,
    prefix: &str,
    builder: &str,
) -> (TokenStream2, TokenStream2) {
    let EmitCtx { cfg, cr, owner } = ctx;
    let Some((len, refs)) = marked_dep_tokens(deps, pool_names, select) else {
        return (quote!(), quote!());
    };
    let table = format_ident!("__SV_{}_{}", prefix, owner);
    let builder = format_ident!("{}", builder);
    (
        quote! {
            #(#cfg)*
            static #table: [&'static #cr::TaskNode; #len] = [#(#refs),*];
        },
        quote!( .#builder(&#table) ),
    )
}

/// The `veto` contributor slots of one item's `writes:` list: the base slot
/// per entry (aligned with the list, `None` where the marker is absent) and
/// the member offset a pool table adds to it.
#[derive(Clone, Copy, Default)]
struct VetoSlots<'a> {
    bases: &'a [Option<u8>],
    offset: usize,
}

fn coupling_binding_tokens(
    ctx: &EmitCtx<'_>,
    decls: &[SignalDecl],
    prefix: &str,
    builder: &str,
    default_observe: Option<&Expr>,
    foreign: &[AdoptedFn],
    veto: VetoSlots<'_>,
) -> (TokenStream2, TokenStream2) {
    let EmitCtx { cfg, cr, owner } = ctx;
    let mut defs = quote!();
    let mut defs_refs: Vec<TokenStream2> = Vec::new();
    let mut ref_cfgs: Vec<Vec<Attribute>> = Vec::new();
    if !decls.is_empty() {
        let table = format_ident!("__SV_{}_{}", prefix, owner);
        // Entries may be individually `#[cfg]`-gated, so the length is summed
        // the same way the dep overlays do it.
        let len = cfg_aware_len(decls.iter().map(|d| &d.cfg));
        let entries: Vec<TokenStream2> = decls
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let (entry_cfg, name, target) = (&d.cfg, d.display(), d.target());
                let veto = veto.bases.get(i).copied().flatten().map(|base| {
                    let slot = u8::try_from(usize::from(base) + veto.offset)
                        .expect("veto slot count checked");
                    quote!( .veto(#slot) )
                });
                let observed = d.observed.as_ref().map(|_| {
                    // `it` names the signal inside the accessor, so one
                    // graph-level default serves every entry in the direction.
                    // With neither a `via` nor a default, the signal answers
                    // for itself through the `Observable` facade.
                    let accessor = d
                        .via
                        .as_ref()
                        .or(default_observe)
                        .map(|e| substitute_it(e, &target))
                        .unwrap_or_else(|| quote!( #cr::Observable::change_token(&#target) ));
                    quote!( .observed(#cr::Observer::new(|| #accessor)) )
                });
                let beat = d.beat.as_ref().map(|_| quote!( .beat() ));
                quote!( #(#entry_cfg)* #cr::Coupling::new(#name, &#target) #observed #beat #veto )
            })
            .collect();
        defs.extend(quote! {
            #(#cfg)*
            static #table: [#cr::Coupling; #len] = [#(#entries),*];
        });
        defs_refs.push(quote!(&#table));
        ref_cfgs.push(Vec::new());
    }
    for f in foreign {
        let table = dataflow_static_path(&f.path, prefix, "");
        let entry_cfg = &f.cfg;
        defs_refs.push(quote!(#(#entry_cfg)* &#table));
        ref_cfgs.push(f.cfg.clone());
    }
    let refs = defs_refs;
    if refs.is_empty() {
        return (defs, quote!());
    }
    // Adoptions may be individually cfg-gated (a feature-gated accessor is
    // ordinary), so the array length is summed cfg-aware like the dep overlays.
    let k = cfg_aware_len(ref_cfgs.iter());
    let tbls = format_ident!("__SV_{}_TBLS_{}", prefix, owner);
    let builder = format_ident!("{}", builder);
    defs.extend(quote! {
        #(#cfg)*
        static #tbls: [&'static [#cr::Coupling]; #k] = [#(#refs),*];
    });
    (defs, quote!( .#builder(&#tbls) ))
}

fn marker_assert_tokens(
    ctx: &EmitCtx<'_>,
    owner: &str,
    discover: Option<&[Attribute]>,
    foreign: &[AdoptedFn],
    decls: &[SignalDecl],
    prefix: &str,
) -> TokenStream2 {
    let EmitCtx { cfg, cr, .. } = ctx;
    let (Some(dcfg), false) = (discover, foreign.is_empty()) else {
        return quote!();
    };
    let clause = if prefix == "READS" { "reads" } else { "writes" };
    let mut out = quote!();
    for d in decls {
        let name = d.display();
        let entry_cfg = &d.cfg;
        // Each term carries its adoption's `#[cfg]`, as statements in a
        let tables: Vec<TokenStream2> = foreign
            .iter()
            .map(|f| {
                let t = dataflow_static_path(&f.path, prefix, "");
                let fcfg = &f.cfg;
                quote! {
                    #(#fcfg)*
                    {
                        __sv_found = __sv_found || #cr::__sv_tail_declared(&#t, #name);
                    }
                }
            })
            .collect();
        let msg = format!(
            "node `{owner}`: `{clause}: [{name} ..]` marks a signal no bound \
             `#[dataflow]` table carries. Beside `discover` an entry may only \
             add a marker to a coupling the scan already found, so either the \
             task fn does not access this signal, or it reaches it under a \
             different final path segment"
        );
        out.extend(quote! {
            #(#cfg)*
            #(#dcfg)*
            #(#entry_cfg)*
            const _: () = {
                let mut __sv_found = false;
                #(#tables)*
                assert!(__sv_found, #msg);
            };
        });
    }
    out
}

fn dataflow_static_path(f: &syn::Path, prefix: &str, infix: &str) -> syn::Path {
    let mut p = f.clone();
    let last = p.segments.last_mut().expect("a path has a segment");
    last.ident = format_ident!("__SV_DATAFLOW_{}_{}{}", prefix, infix, last.ident);
    last.arguments = syn::PathArguments::None;
    p
}

fn discover_fn_path(
    discover: Option<&kw::discover>,
    source: Option<&TaskSource>,
) -> SynResult<Option<syn::Path>> {
    let Some(k) = discover else {
        return Ok(None);
    };
    let (TaskSource::Shell(expr) | TaskSource::Spawn(expr)) =
        source.expect("shape-checked: discover has a source");
    fn callee(e: &syn::Expr) -> Option<&syn::ExprPath> {
        match e {
            syn::Expr::Path(p) => Some(p),
            syn::Expr::Call(c) => callee(&c.func),
            _ => None,
        }
    }
    let Some(path) = callee(expr) else {
        return Err(syn::Error::new_spanned(
            k,
            "`discover` derives its tables from the `task:`/`spawn:` fn — \
             name it by path, not a closure",
        ));
    };
    Ok(Some(path.path.clone()))
}

struct GatedBuilder {
    cfg: Vec<Attribute>,
    tokens: TokenStream2,
    available: bool,
    err: &'static str,
}

fn builder_clause_tokens(
    item_cfg: &[Attribute],
    clauses: impl IntoIterator<Item = GatedBuilder>,
) -> (TokenStream2, TokenStream2, TokenStream2) {
    let (mut inline, mut stmts, mut errors) = (quote!(), quote!(), quote!());
    for c in clauses {
        let (clause_cfg, tokens) = (&c.cfg, &c.tokens);
        if !c.available && !clause_cfg.is_empty() {
            let err = c.err;
            errors.extend(quote!( #(#item_cfg)* #(#clause_cfg)* ::core::compile_error!(#err); ));
        } else if clause_cfg.is_empty() {
            inline.extend(tokens.clone());
        } else {
            stmts.extend(quote!( #(#clause_cfg)* let __sv_cfg = __sv_cfg #tokens; ));
        }
    }
    (inline, stmts, errors)
}

fn discover_error_tokens(
    item_cfg: &[Attribute],
    discover: Option<&embassy_supervisor_syntax::Gated<kw::discover>>,
) -> TokenStream2 {
    match discover {
        Some(g) if !g.cfg.is_empty() && !cfg!(feature = "dataflow") => {
            let c = &g.cfg;
            quote!( #(#item_cfg)* #(#c)* ::core::compile_error!(
                "`discover` requires the `dataflow` feature (embassy-supervisor \
                 feature `dataflow`) — it binds the coupling tables the task \
                 fn's `#[dataflow]` attribute derives"
            ); )
        }
        _ => quote!(),
    }
}

fn disabled_tokens(
    disabled: Option<&embassy_supervisor_syntax::Gated<kw::disabled>>,
) -> TokenStream2 {
    match disabled {
        None => quote!(false),
        Some(g) if g.cfg.is_empty() => quote!(true),
        Some(g) => {
            let pred = cfg_predicate(&g.cfg).expect("parse validated cfg attrs");
            quote!({
                #[cfg(#pred)]
                {
                    true
                }
                #[cfg(not(#pred))]
                {
                    false
                }
            })
        }
    }
}

fn cfg_predicate(attrs: &[Attribute]) -> Option<TokenStream2> {
    let preds: Vec<TokenStream2> = attrs
        .iter()
        .filter_map(|a| match &a.meta {
            Meta::List(ml) if ml.path.is_ident("cfg") => Some(ml.tokens.clone()),
            _ => None,
        })
        .collect();
    match preds.len() {
        0 => None,
        1 => Some(preds[0].clone()),
        _ => Some(quote!(all(#(#preds),*))),
    }
}

fn gate_tokens(resources: &[ResourceDecl]) -> (TokenStream2, Vec<TokenStream2>) {
    let gate_refs: Vec<TokenStream2> = resources
        .iter()
        .map(|r| {
            let cfg = &r.cfg;
            let res = &r.ident;
            quote!(#(#cfg)* &#res)
        })
        .collect();
    (cfg_aware_len(resources.iter().map(|r| &r.cfg)), gate_refs)
}

/// Graph-wide facts the per-item emitters consult: resource slots and the
/// contributor slots of `veto` writes.
struct ResourcePlan {
    /// Per resource name: the first declaring entry's `#[cfg]`s, and whether
    /// the name is a pool's per-member slot array (which `provides:` may not
    /// name).
    cfgs: HashMap<String, (Vec<Attribute>, bool)>,
    /// `(owner, resource)` -> the owner's base slot in that `divisible` budget.
    claim_bases: HashMap<(String, String), u8>,
    /// `(owner, signal display)` -> the owner's base contributor slot in that
    /// `VetoGate`.
    veto_bases: HashMap<(String, String), u8>,
}

impl ResourcePlan {
    /// `owner`'s base contributor slot per entry of its `writes:`, aligned with
    /// the list (`None` where the entry carries no `veto`).
    fn veto_slots(&self, owner: &Ident, writes: &[SignalDecl]) -> Vec<Option<u8>> {
        writes
            .iter()
            .map(|d| {
                self.veto_bases
                    .get(&(owner.to_string(), d.display()))
                    .copied()
            })
            .collect()
    }

    /// `owner`'s base slot per entry of its `resources:`, aligned with the list
    /// (`None` for every kind but `divisible`).
    fn claims(&self, owner: &Ident, resources: &[ResourceDecl]) -> Vec<Option<u8>> {
        resources
            .iter()
            .map(|r| {
                self.claim_bases
                    .get(&(owner.to_string(), r.ident.to_string()))
                    .copied()
            })
            .collect()
    }
}

/// The claims table for one holder: `(&BUDGET, slot)` per `divisible` entry,
/// wired with `.with_claims(..)` so a stop releases each slot. `offset` is the
/// pool member index (0 for a node), added to the entry's base slot.
fn claims_tokens(
    arr: &Ident,
    item_cfg: &[Attribute],
    resources: &[ResourceDecl],
    claims: &[Option<u8>],
    offset: usize,
    cr: &TokenStream2,
) -> (TokenStream2, TokenStream2) {
    let entries: Vec<(&Vec<Attribute>, TokenStream2)> = resources
        .iter()
        .zip(claims)
        .filter_map(|(r, base)| {
            let base = (*base)?;
            let cfg = &r.cfg;
            let res = &r.ident;
            let slot = u8::try_from(usize::from(base) + offset).expect("slot count checked");
            Some((cfg, quote!(#(#cfg)* (&#res, #slot))))
        })
        .collect();
    if entries.is_empty() {
        return (quote!(), quote!());
    }
    let len = cfg_aware_len(entries.iter().map(|(c, _)| *c));
    let refs = entries.iter().map(|(_, t)| t);
    (
        quote! {
            #(#item_cfg)*
            static #arr: [(&'static dyn #cr::Divisible, u8); #len] = [#(#refs),*];
        },
        quote!( .with_claims(&#arr) ),
    )
}

fn provides_tokens(
    n: &NodeItem,
    cr: &TokenStream2,
    resource_cfgs: &std::collections::HashMap<String, (Vec<Attribute>, bool)>,
) -> SynResult<(TokenStream2, TokenStream2)> {
    if n.provides.is_empty() {
        return Ok((quote!(), quote!()));
    }
    let mut cfgs: Vec<Vec<Attribute>> = Vec::new();
    let refs = n
        .provides
        .iter()
        .map(|p| {
            let slot = &p.ident;
            let Some((cfg, from_pool)) = resource_cfgs.get(&slot.to_string()) else {
                return Err(syn::Error::new_spanned(
                    slot,
                    format!(
                        "`provides:` names `{slot}`, but no `resources:` entry in \
                         this graph declares a slot by that name — the clause \
                         clears the macro-emitted slot statics on the provider's \
                         shutdown ack, so it can only name one of them"
                    ),
                ));
            };
            if *from_pool {
                return Err(syn::Error::new_spanned(
                    slot,
                    format!(
                        "`provides:` cannot name `{slot}` — it is a pool's \
                         per-member slot array, filled and cleared by the pool's \
                         own scaling; the clause clears a single node's slot \
                         statics on its shutdown ack"
                    ),
                ));
            }
            let entry_cfg = &p.cfg;
            cfgs.push(cfg.iter().chain(entry_cfg.iter()).cloned().collect());
            Ok(quote!(#(#cfg)* #(#entry_cfg)* &#slot))
        })
        .collect::<SynResult<Vec<_>>>()?;
    let len = cfg_aware_len(cfgs.iter());
    let node_cfg = &n.cfg;
    let arr = format_ident!("__SV_PROVIDES_{}", n.ident);
    Ok((
        quote! {
            #(#node_cfg)*
            static #arr: [&'static dyn #cr::ResourceGate; #len] = [#(#refs),*];
        },
        quote!( .with_provides(&#arr) ),
    ))
}

/// `" (from fragment \`X\`)"` when the item was forwarded through a
/// `supervisor_fragment!` relay, else empty — error-message attribution.
fn fragment_suffix(fragment: &Option<String>) -> String {
    match fragment {
        Some(f) => format!(" (from fragment `{f}`)"),
        None => String::new(),
    }
}

/// The `[&'static TaskNode; n]` element and length tokens for the deps
/// carrying one marker, cfg-aware like `gate_tokens`. A dep naming a pool
/// resolves to that pool's floor member (`&POOL[0]`), matching how `deps: [POOL]`
/// resolves for spawn ordering.
fn marked_dep_tokens(
    deps: &[Dep],
    pool_names: &std::collections::HashSet<String>,
    select: impl Fn(&Dep) -> bool,
) -> Option<(TokenStream2, Vec<TokenStream2>)> {
    let marked: Vec<&Dep> = deps.iter().filter(|d| select(d)).collect();
    if marked.is_empty() {
        return None;
    }
    let refs: Vec<TokenStream2> = marked
        .iter()
        .map(|d| {
            let cfg = &d.cfg;
            let ident = &d.ident;
            if pool_names.contains(&ident.to_string()) {
                quote!(#(#cfg)* &#ident[0])
            } else {
                quote!(#(#cfg)* &#ident)
            }
        })
        .collect();
    Some((cfg_aware_len(marked.iter().map(|d| &d.cfg)), refs))
}

/// Extract the policy *type* from a `Type::new(..)` constructor expression. Only used
/// on the derive path (no explicit `policy: <Ty> = ..` annotation); the type is the
/// call's path minus its last segment (`DeferredShrink::new` -> `DeferredShrink`).
fn policy_type(expr: &Expr) -> SynResult<Path> {
    if let Expr::Call(call) = expr
        && let Expr::Path(p) = &*call.func
    {
        let n = p.path.segments.len();
        if n >= 2 {
            let segs: Punctuated<_, Token![::]> =
                p.path.segments.iter().take(n - 1).cloned().collect();
            return Ok(Path {
                leading_colon: p.path.leading_colon,
                segments: segs,
            });
        }
    }
    Err(syn::Error::new_spanned(
        expr,
        "pool `policy:` must be a `Type::new(..)` constructor (e.g. `DeferredShrink::new(..)`), \
         or give the type explicitly: `policy: <Type> = <expr>`",
    ))
}

/// One emitted node slot, in final index order.
struct Slot {
    /// Presence predicate (`None` = unconditional), gates the node slot (`GRAPH.nodes`) entry.
    cfg_pred: Option<TokenStream2>,
    /// `&NODE` or `&POOL[j]`.
    reference: TokenStream2,
    /// Raw deps, resolved to indices in the second pass.
    deps: Vec<Dep>,
    /// The `supervisor_fragment!` the owning item came from, for error
    /// attribution when a dep fails to resolve across the relay.
    fragment: Option<String>,
}

/// The `Option<fn(..)>` spawn expression for a node. `None` (no `spawn:`) is a
/// parked node the app spawns itself. A path or partial call is a task fn taking
/// `&NODE` first (plus any given args); the macro wraps it as
/// `|s| { s.spawn(<task>(&NODE, ..)?); Ok(()) }`. Anything else (a closure, or a
/// ready spawn fn) is emitted verbatim. Every form is cast to `spawn_fn` so it
/// coerces cleanly inside `Option::Some(..)`.
#[allow(clippy::too_many_arguments)]
fn node_spawn(
    ident: &Ident,
    spawn: &Option<Expr>,
    executor: &Option<Ident>,
    resources: &[ResourceDecl],
    // `state:`: fallibly box the init value in the glue, BEFORE the resource
    // takes (a failed alloc strands nothing) — `SpawnError::Busy`, retryable.
    state: Option<(&syn::Type, &StateInit)>,
    spawn_fn: &TokenStream2,
    cr: &TokenStream2,
    helpers: &HelperIdents,
) -> SynResult<TokenStream2> {
    // `resources:` glue prelude: every entry is PROBED here — the fail-closed
    // check that turns "unprovided" into `SpawnError::Busy` out of
    // `Supervisor::start` — and read by the shell itself at first poll
    // (`take()` for lend/consume, the non-destructive `get()` for `shared`): a
    // value moved through the task-fn call would be dropped, unrecoverable,
    // when the claim fails (`Busy` while the previous instance's storage is
    // still releasing), and even a `Copy` handle passed as an argument sits in
    // the task arena for the whole run beside the worker's own copy
    // (rust-lang/rust#62958). Nothing crosses the call.
    let take_prelude: Vec<TokenStream2> = resources
        .iter()
        .map(|r| {
            let cfg = &r.cfg;
            let res = &r.ident;
            quote! {
                #(#cfg)*
                if !#cr::ResourceGate::is_filled(&#res) {
                    return ::core::result::Result::Err(::embassy_executor::SpawnError::Busy);
                }
            }
        })
        .collect();
    let (state_prelude, state_arg) = match state {
        Some((ty, init)) => (state_box_stmt(ty, init, helpers), vec![quote!(__state)]),
        None => (quote!(), vec![]),
    };
    Ok(match (spawn, executor) {
        (None, None) => quote!(::core::option::Option::None),
        // `executor:` needs the macro to perform the spawn, so it composes only
        // with the path / partial-call `spawn:` forms below.
        (None, Some(ex)) => {
            return Err(syn::Error::new_spanned(
                ex,
                "`executor:` requires a `spawn:` (a parked node is spawned by the \
                 application, which picks its own spawner)",
            ));
        }
        // A path or a partial call: a task fn taking `&NODE` first (plus any
        // given args); generate `|s| { s.spawn(<task>(&NODE, ..)?); Ok(()) }`.
        // With `executor: NAME` the glue ignores the supervisor's `Spawner` and
        // spawns through the named `SpawnerSlot` (a `SendSpawner` the app
        // registers at runtime): an unfilled slot fails the spawn with
        // `SpawnError::Busy` — loud misconfiguration, not a missing task. The
        // task future must then be `Send` (enforced by `SendSpawner::spawn`).
        (Some(e @ (Expr::Path(_) | Expr::Call(_))), executor) => {
            let mut lead: Vec<TokenStream2> = vec![quote!(&#ident)];
            lead.extend(state_arg.iter().cloned());
            let call = inject_call_with(e, &lead)?;
            match executor {
                None => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(s));
                    quote!(::core::option::Option::Some(
                        (|s| {
                            #state_prelude
                            #(#take_prelude)*
                            #stmts
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
                Some(ex) => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(__sp));
                    quote!(::core::option::Option::Some(
                        (|_s| {
                            // The supervisor awaits this slot's `ready()` before
                            // invoking the glue (the node carries `.with_executor(&EX)`
                            // and the bring-up bounds the wait), so `get()` is already
                            // filled; `ok_or` is the belt-and-braces unfilled guard.
                            // Resources are taken AFTER the spawner guard, so an
                            // unfilled executor never consumes (and strands) them.
                            let __sp = #ex
                                .get()
                                .ok_or(::embassy_executor::SpawnError::Busy)?;
                            #state_prelude
                            #(#take_prelude)*
                            #stmts
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
            }
        }
        (Some(_), Some(ex)) => {
            return Err(syn::Error::new_spanned(
                ex,
                "`executor:` cannot be combined with a verbatim spawn closure (the \
                 closure owns the spawn; use the named SpawnerSlot inside it instead)",
            ));
        }
        // Anything else (a closure, or a ready spawn fn) is emitted verbatim.
        // NOTE: with the `trace` feature such a node is not auto-mapped — the
        // closure owns the SpawnToken; call `adopt`/`set_task_id` in it yourself.
        (Some(e), None) => quote!(::core::option::Option::Some((#e) as #spawn_fn)),
    })
}

/// `let __state = …?;` for a `state:` clause: the init form boxes the value,
/// the `zeroed` form allocates zero-filled memory with no value built first.
fn state_box_stmt(ty: &syn::Type, init: &StateInit, helpers: &HelperIdents) -> TokenStream2 {
    match init {
        StateInit::Expr(init) => {
            let try_box = &helpers.try_box;
            quote! {
                let __state = #try_box(#init)
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            }
        }
        StateInit::Zeroed(z) => {
            let try_box_zeroed = &helpers.try_box_zeroed;
            // Only the call carries the marker's span, so a missing `Zeroable`
            // impl points at `zeroed`; the binding keeps the glue's hygiene.
            let call = quote::quote_spanned!(z.span=> #try_box_zeroed::<#ty>());
            quote! {
                let __state = #call.ok_or(::embassy_executor::SpawnError::Busy)?;
            }
        }
    }
}

/// The spawn statement(s) for the generated glue. Plain `s.spawn(<call>?)`
/// normally; with the `trace` feature the `SpawnToken` is bound first so its task
/// id can be captured into the node (`set_task_id`) — the id→node mapping the
/// supervisor's `trace` recorders resolve against (in embassy-executor 0.10 the
/// task-fn call returns `Result<SpawnToken, SpawnError>` and `Spawner::spawn`
/// itself is infallible, so the token is available between the two).
///
/// Three shapes, resolved at expansion by the macro crate's own features:
/// * `trace` on → bind the token and `adopt` it (`set_task_id` + name stamp under
///   `metadata-names`).
/// * `trace` off but `metadata-names` on → bind the token and `stamp_name` only:
///   the node name reaches the task Metadata (for rtos-trace/SystemView) with no id
///   capture and no dependency on the `_embassy_trace_*` hooks.
/// * neither → plain infallible spawn.
fn spawn_stmts(call: &TokenStream2, node_ref: &TokenStream2, sp: &TokenStream2) -> TokenStream2 {
    if cfg!(feature = "trace") {
        // `adopt` = set_task_id + (under metadata-names) Metadata name stamp.
        quote! {
            let __token = #call?;
            (#node_ref).adopt(&__token);
            #sp.spawn(__token);
        }
    } else if cfg!(feature = "metadata-names") {
        // Name-only path: stamp the node name into the task Metadata, nothing else.
        quote! {
            let __token = #call?;
            (#node_ref).stamp_name(&__token);
            #sp.spawn(__token);
        }
    } else {
        quote!(#sp.spawn(#call?);)
    }
}

/// Emit the `#[embassy_executor::task]` shell for a `task:` clause: a concrete,
/// non-generic task fn that takes only the node and awaits the user's worker with
/// the node injected first. This is how a **generic** worker becomes spawnable —
/// embassy forbids generic tasks (one static `TaskPool` per concrete future type),
/// so a monomorphized shell is stamped per declaration. Worker args are evaluated
/// inside the shell — at the task's first poll, on the node's own executor — so
/// the DSL never needs the arg types and a cross-core node builds its resources on
/// the core that runs them.
///
/// Returns the shell item and a path `Expr` naming it, which feeds the ordinary
/// `spawn:` path-form glue (executor routing and trace `adopt` compose unchanged).
// One argument per independent codegen input; a bundling struct would only
// rename the coupling.
#[allow(clippy::too_many_arguments)]
fn emit_shell(
    owner: &Ident,
    cfg: &[Attribute],
    worker: &Expr,
    pool_size: usize,
    resources: &[ResourceDecl],
    // Aligned with `resources`: the holder's slot in each `divisible` entry's
    // budget (a pool shell gets the member's `Claimant` as a parameter instead).
    claims: &[Option<u8>],
    exit: Option<&syn::Type>,
    // `state: Type = ..`: the shell owns the glue-boxed state across the worker
    // call (worker sees `&mut Type`) and DROPS it first thing after the worker
    // returns — reclaimed before restores/exit-provide/mark_exited.
    state: Option<(&syn::Type, &StateInit)>,
    // Pool shells restore lend entries to a slot REFERENCE parameter (the
    // member's own array element, passed by the wrapper) instead of a slot
    // named statically — restore-to-same-index by construction.
    pool_member: bool,
    // `cancel`: drive the worker under `run_cancellable` and DON'T lead its
    // arguments with the node — the worker is a plain future that never returns
    // on its own, so the shell owns the shutdown race on its behalf.
    cancel: bool,
    cr: &TokenStream2,
    helpers: &HelperIdents,
) -> SynResult<(TokenStream2, Expr)> {
    if !matches!(worker, Expr::Path(_) | Expr::Call(_)) {
        return Err(syn::Error::new_spanned(
            worker,
            "`task:` names an async worker fn — a path or a partial call like \
             `worker(args)`; for a closure or a ready spawn fn use `spawn:`",
        ));
    }
    let shell = format_ident!("__sv_task_{}", owner.to_string().to_lowercase());
    // `resources:` handling: every entry is read BY THE SHELL at first poll —
    // `take()` for lend/consume, the non-destructive `get()` for `shared` —
    // never through the task-fn call, where a `Busy` storage claim would drop
    // an owned value unrecoverably and where even a `Copy` handle argument
    // would sit in the task arena for the whole run beside the worker's own
    // copy (rust-lang/rust#62958). The glue probed the slot, so a read that
    // still comes up empty means an out-of-band `take()` won the race: the
    // shell exits immediately (`mark_lost_resource` — a warn plus the
    // completion record), reading as a failed activation rather than a panic.
    //
    // Lend entries are lent to the worker as `&mut` and restored to their slot
    // after the worker returns — i.e. after its clean shutdown ack — so a
    // Terminate respawn re-takes the SAME instance instead of re-acquiring
    // hardware. A `Pause` worker parks instead of returning and simply retains
    // them. `consume` and `shared` forward by value with no restore: a consumed
    // slot stays empty until the app re-`provide()`s (fail-closed respawn), a
    // shared slot was never emptied. A `divisible` entry forwards the member's
    // `Claimant` by value; its share is released by the supervisor, not here.
    //
    // Per-entry `#[cfg]` rides on params, reads, worker-call arguments, and
    // restore statements alike, so a cfg'd-out entry disappears from the whole
    // chain (the worker fn must gate its matching parameter the same way).
    let typed = |r: &ResourceDecl| r.ty.clone().expect("a typed resource kind");
    let res_params: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let cfg = &r.cfg;
            match r.kind() {
                // A pool shell is shared by its members and cannot name `RES[I]`
                // itself, so member `I`'s slot element rides in as a reference.
                // (`shared` slots are pool-wide statics, nameable directly.)
                ResourceKind::Lend | ResourceKind::Consume if pool_member => {
                    let ty = typed(r);
                    let slot_param = format_ident!("__r{}_slot", i);
                    Some(quote!(#(#cfg)* #slot_param: &'static #cr::ResourceSlot<#ty>))
                }
                // Likewise the member's slot in a budget: the wrapper builds
                // the `Claimant` and passes it in.
                ResourceKind::Divisible if pool_member => {
                    let var = format_ident!("__r{}", i);
                    Some(quote!(#(#cfg)* #var: #cr::Claimant))
                }
                _ => None, // read from the slot static inside the shell
            }
        })
        .collect();
    let res_takes: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            let res = &r.ident;
            match r.kind() {
                ResourceKind::Shared => Some(quote! {
                    #(#cfg)*
                    let ::core::option::Option::Some(#var) = #res.get() else {
                        __node.mark_lost_resource();
                        return;
                    };
                }),
                ResourceKind::Divisible if pool_member => None,
                ResourceKind::Divisible => {
                    let slot = claims[i].expect("a divisible entry has a slot");
                    Some(quote!(#(#cfg)* let #var = #res.claimant(#slot);))
                }
                kind => {
                    let mutability = if kind == ResourceKind::Consume {
                        quote!()
                    } else {
                        quote!(mut)
                    };
                    let slot = if pool_member {
                        let slot_param = format_ident!("__r{}_slot", i);
                        quote!(#slot_param)
                    } else {
                        quote!(#res)
                    };
                    Some(quote! {
                        #(#cfg)*
                        let ::core::option::Option::Some(#mutability #var) = #slot.take() else {
                            __node.mark_lost_resource();
                            return;
                        };
                    })
                }
            }
        })
        .collect();
    let res_leases: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            if r.kind() == ResourceKind::Lend {
                quote!(#(#cfg)* &mut #var)
            } else {
                quote!(#(#cfg)* #var)
            }
        })
        .collect();
    let restores: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind() == ResourceKind::Lend)
        .map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            if pool_member {
                let slot_param = format_ident!("__r{}_slot", i);
                quote!(#(#cfg)* #slot_param.restore(#var);)
            } else {
                let res = &r.ident;
                quote!(#(#cfg)* #res.restore(#var);)
            }
        })
        .collect();
    let alloc_alias = &helpers.alloc_alias;
    let (state_param, state_lease, state_drop) = match state {
        Some((ty, _)) => (
            quote!(, mut __state: #alloc_alias::boxed::Box<#ty>),
            vec![quote!(&mut *__state)],
            // Reclaim the bulk FIRST: before restores, exit-provide, and the
            // completion record, so has_exited() implies the heap is back.
            quote!(::core::mem::drop(__state);),
        ),
        None => (quote!(), vec![], quote!()),
    };
    // `cancel` workers take no node: the shell holds it and races the worker's
    // future against the shutdown signal itself, which is the whole point of the
    // flag — the worker stays a plain async fn with no supervisor in its
    // signature.
    let mut lead: Vec<TokenStream2> = if cancel {
        Vec::new()
    } else {
        vec![quote!(__node)]
    };
    lead.extend(res_leases);
    lead.extend(state_lease);
    let call = inject_call_with(worker, &lead)?;
    // Unsuffixed literal: `#[task]`'s own parser wants a plain integer.
    let ps = LitInt::new(&pool_size.to_string(), proc_macro2::Span::call_site());
    // A diverging (`-> !`) worker makes the trailing statements unreachable —
    // legitimate (a detached/`Pause` worker retains its resources forever), so
    // silence rustc's `unreachable_code` lint on the generated body. Always
    // emitted: the completion record below is an unconditional trailing
    // statement.
    let allow_unreachable = quote!(#[allow(unreachable_code)]);
    // `exit: Type`: bind the worker's return value and provide() it into the
    // node's exit slot BEFORE mark_exited, so has_exited() implies the value is
    // present. A worker whose return type mismatches the declared `exit:` fails
    // at this provide with a plain rustc type error on the shell.
    // Under `cancel` the worker may not have returned at all — the shell holds a
    // `Result<Output, Aborted>` — so the exit value is provided only on a real
    // completion. An aborted worker leaves `<NODE>_EXIT` empty (and
    // `shutdown_requested()` set), which is how a waiter tells "it finished" from
    // "it was stopped".
    //
    // A DIVERGING worker (`-> !`) makes that provide dead code: its future has
    // no output, so the slot could never be filled and every `wait_take()` on
    // it would hang forever. The blanket allow above would hide that, so the
    // provide re-DENIES `unreachable_code` on itself — the one statement in the
    // shell where unreachability is a declaration error rather than a
    // legitimate parked/detached worker. Spanned on the declared `exit:` type,
    // so rustc points at the clause the user has to remove (a bare diverging
    // worker stays legal: that is what `cancel` is for).
    let exit_ident = format_ident!("{}_EXIT", owner);
    // The worker's return value binding. Created ONCE, with the macro's own
    // call-site span, and interpolated into both the `let` below and the
    // `provide` — writing `__out` literally inside `quote_spanned!(exit.span())`
    // would give the two occurrences different hygiene contexts, and the
    // provide would fail to see the binding whenever the `exit:` clause's span
    // came from a different expansion than the shell's (e.g. a node declared at
    // a `compose_graph!` site alongside `supervisor_fragment!` fragments).
    //
    // The span is the `exit:` type's, matching the `provide` below, so the
    // clause's own diagnostics (the `unreachable_code` deny on a diverging
    // worker) keep pointing at the clause rather than at the whole graph.
    let out_ident = |exit: &syn::Type| Ident::new("__out", exit.span());
    let provide = |exit: &syn::Type| {
        let out_ident = out_ident(exit);
        // Every token of the statement carries the `exit:` type's span, so the
        // lint's own label lands on that clause instead of the whole item.
        let slot = Ident::new(&exit_ident.to_string(), exit.span());
        quote::quote_spanned!(exit.span()=>
            #[deny(unreachable_code)]
            #slot.provide(#out_ident);
        )
    };
    // The `cancel` arms pin the worker into the shell's own frame and hand
    // `run_cancellable` a `Pin<&mut _>`: the shell then stores the worker's state
    // machine ONCE, whatever rustc decides to do with the callee's arguments
    // (rust-lang/rust#62958 doubles a future passed by value into an `async fn`,
    // and this is static task storage, so the doubling was per node, per binary).
    // The `pin!` lives in its own block so the worker is dropped at the same point
    // it always was — before the restores below, which move the lent resources
    // back out.
    let (drive, provide_exit) = match (cancel, exit) {
        (false, Some(ty)) => {
            let provide = provide(ty);
            let out_ident = out_ident(ty);
            (quote!(let #out_ident = #call.await;), provide)
        }
        (false, None) => (quote!(#call.await;), quote!()),
        (true, Some(ty)) => {
            let provide = provide(ty);
            let out_ident = out_ident(ty);
            (
                quote!(let __res = { let __fut = ::core::pin::pin!(#call); __node.run_cancellable(__fut).await };),
                quote!(if let ::core::result::Result::Ok(#out_ident) = __res {
                    #provide
                }),
            )
        }
        (true, None) => (
            quote!({
                let __fut = ::core::pin::pin!(#call);
                let _ = __node.run_cancellable(__fut).await;
            }),
            quote!(),
        ),
    };
    // Record the completion (and ack any pending shutdown handshake): a worker
    // that returns on its own reads as down, not running forever, and a control
    // Activate can respawn it. A per-node shell names its node STATIC here (like
    // the `_EXIT` provide above) so `__node`'s last use is the worker call — a
    // local dead before the await stays out of the task arena. A pool shell is
    // shared by its members, so only the parameter knows the node.
    let mark_exited = if pool_member {
        quote!(__node.mark_exited();)
    } else {
        quote!(#owner.mark_exited();)
    };
    let def = quote! {
        #(#cfg)*
        #[::embassy_executor::task(pool_size = #ps)]
        #allow_unreachable
        async fn #shell(__node: &'static #cr::TaskNode #(, #res_params)* #state_param) {
            #(#res_takes)*
            #drive
            #state_drop
            #(#restores)*
            #provide_exit
            #mark_exited
        }
    };
    let path: Expr = syn::parse_quote!(#shell);
    Ok((def, path))
}

/// Emit a `node`: its `pub static #ident: TaskNode` definition and its `Slot`. The
/// caller assigns the slot index and records the name, so this touches neither.
/// A `task:` node additionally emits its generated shell ahead of the static.
fn emit_node(
    n: &NodeItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    // Threaded to ready_tokens: a ready dep naming a pool refs its floor member.
    pool_names: &std::collections::HashSet<String>,
    helpers: &HelperIdents,
    observe: &ObserveDefaults,
    // Graph-wide resource facts: `provides:` name resolution and this node's
    // slot in each `divisible` budget.
    plan: &ResourcePlan,
) -> SynResult<(TokenStream2, Slot)> {
    let ident = &n.ident;
    let claims = plan.claims(&n.ident, &n.resources);
    let cfg = &n.cfg;
    let mode = &n.mode;
    let name = name_string(&n.ident);
    let disabled = disabled_tokens(n.disabled.as_ref());
    let (shell_def, spawn_expr) = match &n.source {
        Some(TaskSource::Shell(worker)) => {
            let ps = match &n.pool_size {
                Some(l) => l.base10_parse::<usize>()?,
                None => 1,
            };
            // A node's take-kind slots hold ONE value, so extra instances
            // could only race the shell's take; a divisible slot is ONE
            // claimant, so extra instances would clobber each other's want
            // and share one grant. Reject the combination instead of letting
            // the loser exit as a lost resource.
            if ps > 1
                && n.resources
                    .iter()
                    .any(|r| !matches!(r.kind(), ResourceKind::Shared))
            {
                return Err(syn::Error::new_spanned(
                    n.pool_size.as_ref().unwrap(),
                    "`pool_size > 1` cannot combine with lend/consume/divisible \
                     `resources:` (the slot holds one value, or one claimant; use \
                     `shared`, or an `ElasticPool` with per-member slots)",
                ));
            }
            let (def, path) = emit_shell(
                ident,
                cfg,
                worker,
                ps,
                &n.resources,
                &claims,
                n.exit.as_ref(),
                n.state.as_ref().map(|(_, ty, init)| (ty, init)),
                false,
                n.cancel,
                cr,
                helpers,
            )?;
            (def, Some(path))
        }
        Some(TaskSource::Spawn(e)) => (quote!(), Some(e.clone())),
        None => (quote!(), None),
    };
    let spawn = node_spawn(
        ident,
        &spawn_expr,
        &n.executor,
        &n.resources,
        n.state.as_ref().map(|(_, ty, init)| (ty, init)),
        spawn_fn,
        cr,
        helpers,
    )?;
    // `executor: NAME` routes the node through that SpawnerSlot; the supervisor
    // awaits the slot before spawning (see `TaskNode::with_executor`).
    let with_exec = match &n.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    // `resources: [NAME: Type, ..]` — one `pub static NAME: ResourceSlot<Type>`
    // per entry (main moves the resource in with `NAME.provide(..)`), plus a
    // type-erased gate array wired into the node so the supervisor can await
    // provisioning/restore before each (re)spawn (see `TaskNode::with_resources`).
    // The unsized coercion `&NAME` -> `&dyn ResourceGate` happens in the static
    // initializer, where it is allowed.
    let (res_defs, with_res) = if n.resources.is_empty() {
        (quote!(), quote!())
    } else {
        let gates_ident = format_ident!("__SV_GATES_{}", ident);
        // `shared` slots and `divisible` budgets are emitted once per graph in
        // `expand` (several items may declare the same one); only this node's
        // exclusive (take-kind) slots are emitted here.
        let slot_defs = n
            .resources
            .iter()
            .filter(|r| matches!(r.kind(), ResourceKind::Lend | ResourceKind::Consume))
            .map(|r| {
                let ecfg = &r.cfg;
                let res = &r.ident;
                let ty = r.ty.as_ref().expect("a typed resource kind");
                // `local` entries use the graph-site slot type (emitted once per
                // graph in `expand`): same provide/take protocol as `ResourceSlot`
                // but without its `T: Send` bound, for `!Send` driver handles on a
                // single-core system. `consume` changes only shell codegen (by-value
                // arg, no restore) — the slot type is the same either way.
                let slot_ty = if r.local.is_some() {
                    let local = &helpers.local_slot;
                    quote!(#local<#ty>)
                } else {
                    quote!(#cr::ResourceSlot<#ty>)
                };
                let doc = if r.consume.is_some() {
                    format!(
                        "Resource slot for node `{ident}` (generated by `supervisor_graph!`). \
                         Move the resource in with `.provide(..)` before `Supervisor::start`. \
                         `consume`: the worker owns (and may drop) the value, so the slot is \
                         empty after the task exits — re-`provide()` before any respawn."
                    )
                } else {
                    format!(
                        "Resource slot for node `{ident}` (generated by `supervisor_graph!`). \
                         Move the resource in with `.provide(..)` before `Supervisor::start`."
                    )
                };
                quote! {
                    #(#cfg)*
                    #(#ecfg)*
                    #[doc = #doc]
                    pub static #res: #slot_ty = <#slot_ty>::new();
                }
            });
        let (gates_len, gate_refs) = gate_tokens(&n.resources);
        (
            quote! {
                #(#slot_defs)*
                #(#cfg)*
                static #gates_ident: [&'static dyn #cr::ResourceGate; #gates_len] =
                    [#(#gate_refs),*];
            },
            quote!( .with_resources(&#gates_ident) ),
        )
    };
    let (with_clauses, clause_stmts, clause_errors) = builder_clause_tokens(
        cfg,
        [
            n.slot_timeout.as_ref().map(|g| {
                let (c, ms) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_slot_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                    available: true,
                    err: "",
                }
            }),
            n.ack_timeout.as_ref().map(|g| {
                let (c, ms) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_ack_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                    available: true,
                    err: "",
                }
            }),
            n.beat_timeout.as_ref().map(|g| {
                let (c, ms) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_beat_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                    available: cfg!(feature = "liveness-monitor"),
                    err: "`beat_timeout:` requires the `liveness-monitor` feature \
                          (embassy-supervisor feature `liveness-monitor`) — the \
                          supervisor then reports this node once it has been running \
                          that long without a beat()",
                }
            }),
            // `beat_window:` alone is rejected as a shape error.
            n.beat_window.as_ref().map(|g| {
                let (c, w) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_beat_window(#w) ),
                    available: cfg!(feature = "liveness-monitor"),
                    err: "`beat_window:` requires the `liveness-monitor` feature \
                          (embassy-supervisor feature `liveness-monitor`) — it sets \
                          how many consecutive stale sweeps are reported on",
                }
            }),
            n.ready_on_write.as_ref().map(|g| GatedBuilder {
                cfg: g.cfg.clone(),
                tokens: quote!( .with_ready_on_write() ),
                available: cfg!(all(feature = "coupling-observe", feature = "readiness")),
                err: "`ready_on_write` requires the `coupling-observe` and \
                      `readiness` features (embassy-supervisor features of the \
                      same names) — readiness is asserted by the monitor sweep \
                      seeing an `observed beat` write advance",
            }),
        ]
        .into_iter()
        .flatten(),
    );
    // The node's view of its own graph: what a data-driven dependency resolves
    // its producer through. Const, and a cycle only in the address sense — the
    // graph names the nodes, each node names the graph.
    let graph_ref = &helpers.graph_ref;
    let with_graph = cfg!(feature = "data-deps").then(|| quote!( .with_graph(&#graph_ref) ));
    // `deps: [X ready, ..]` — the ready-marked subset becomes a per-node
    // `[&'static TaskNode; n]` array wired via `.with_ready_deps`: bring-up
    // awaits each one's set_ready() (bounded by slot_timeout) after the
    let ctx = EmitCtx {
        cfg,
        cr,
        owner: ident,
    };
    let marker = |select: fn(&Dep) -> bool, prefix, builder| {
        marker_array_tokens(&ctx, &n.deps, pool_names, select, prefix, builder)
    };
    let (ready_def, with_ready) = marker(|d| d.ready.is_some(), "READY", "with_ready_deps");
    let (bound_def, with_bound) = marker(|d| d.bound.is_some(), "BOUND", "with_bound_deps");
    let mut foreign: Vec<AdoptedFn> = Vec::new();
    foreign.extend(
        discover_fn_path(n.discover.as_ref().map(|g| &g.kw), n.source.as_ref())?
            .filter(|_| cfg!(feature = "dataflow"))
            .map(|path| AdoptedFn {
                cfg: n
                    .discover
                    .as_ref()
                    .map(|g| g.cfg.clone())
                    .unwrap_or_default(),
                path,
            }),
    );
    foreign.extend(n.dataflow.iter().cloned());
    let (reads_def, with_reads) = coupling_binding_tokens(
        &ctx,
        &n.reads,
        "READS",
        "with_reads",
        observe.reads.as_ref(),
        &foreign,
        VetoSlots::default(),
    );
    let marker_asserts = {
        let d = n.discover.as_ref().map(|g| g.cfg.as_slice());
        let r = marker_assert_tokens(&ctx, &name_string(ident), d, &foreign, &n.reads, "READS");
        let w = marker_assert_tokens(&ctx, &name_string(ident), d, &foreign, &n.writes, "WRITES");
        quote!( #r #w )
    };
    let veto_bases = plan.veto_slots(&n.ident, &n.writes);
    let (writes_def, with_writes) = coupling_binding_tokens(
        &ctx,
        &n.writes,
        "WRITES",
        "with_writes",
        observe.writes.as_ref(),
        &foreign,
        VetoSlots {
            bases: &veto_bases,
            offset: 0,
        },
    );
    let exit_def = match &n.exit {
        Some(ty) => {
            let exit_ident = format_ident!("{}_EXIT", ident);
            let doc = format!(
                "Exit-value slot for node `{ident}` (generated by `supervisor_graph!`). \
                 The generated shell `provide()`s the worker's return value here just \
                 before recording the exit; read it with `.wait_take()` (or `.take()` \
                 after `has_exited()`). Overwritten by the next completion."
            );
            quote! {
                #(#cfg)*
                #[doc = #doc]
                pub static #exit_ident: #cr::ResourceSlot<#ty> =
                    #cr::ResourceSlot::new();
            }
        }
        None => quote!(),
    };
    let node_doc = format!(
        "Supervised node `{ident}` (`{mode}`), generated by `supervisor_graph!`. \
         Pass it to the supervisor's per-node verbs (`start_node`, `stop_node`, \
         `resume_node`, `activate`/`deactivate`); the worker gets the same \
         `&'static TaskNode` for the task-side protocol."
    );
    let cfg_ident = format_ident!("__SV_CFG_{}", ident);
    let (prov_def, with_provides) = provides_tokens(n, cr, &plan.cfgs)?;
    let (claims_def, with_claims) = claims_tokens(
        &format_ident!("__SV_CLAIMS_{}", ident),
        cfg,
        &n.resources,
        &claims,
        0,
        cr,
    );
    let cfg_chain = quote! {
        #cr::NodeCfg::new(#name, #cr::Mode::#mode, #spawn)
            #with_exec #with_res #with_provides #with_claims #with_clauses #with_ready
            #with_bound #with_reads #with_writes
            #with_graph
    };
    let cfg_init = if clause_stmts.is_empty() {
        cfg_chain
    } else {
        quote! {{
            let __sv_cfg = #cfg_chain;
            #clause_stmts
            __sv_cfg
        }}
    };
    let discover_errors = discover_error_tokens(cfg, n.discover.as_ref());
    let def = quote! {
        #clause_errors
        #discover_errors
        #res_defs
        #prov_def
        #claims_def
        #exit_def
        #ready_def
        #bound_def
        #reads_def
        #writes_def
        #marker_asserts
        #shell_def
        #(#cfg)*
        #[doc(hidden)]
        static #cfg_ident: #cr::NodeCfg = #cfg_init;
        #(#cfg)*
        #[doc = #node_doc]
        pub static #ident: #cr::TaskNode = #cr::TaskNode::new(&#cfg_ident, #disabled);
    };
    let slot = Slot {
        cfg_pred: cfg_predicate(cfg),
        reference: quote!(&#ident),
        deps: n.deps.clone(),
        fragment: n.fragment.clone(),
    };
    Ok((def, slot))
}

fn emit_pool(
    p: &PoolItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    pool_names: &std::collections::HashSet<String>,
    helpers: &HelperIdents,
    observe: &ObserveDefaults,
    // Graph-wide resource facts: the pool's base slot in each `divisible`
    // budget (member `j` holds base + j).
    plan: &ResourcePlan,
) -> SynResult<(Vec<TokenStream2>, TokenStream2, Vec<Slot>)> {
    let ident = &p.ident;
    let claims = plan.claims(&p.ident, &p.resources);
    let cfg = &p.cfg;
    let lname = name_string(&p.ident);
    let pool_static = format_ident!("{}_POOL", ident);
    let k = p.modes.len();

    let lit_bounds = match (&p.min, &p.max) {
        (Expr::Lit(lmin), Expr::Lit(lmax)) => match (&lmin.lit, &lmax.lit) {
            (syn::Lit::Int(imin), syn::Lit::Int(imax)) => {
                Some((imin.base10_parse::<u8>()?, imax.base10_parse::<u8>()?))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some((min_v, max_v)) = lit_bounds {
        if min_v > max_v {
            return Err(syn::Error::new_spanned(
                &p.min,
                format!("pool `min:` ({min_v}) must not exceed `max:` ({max_v})"),
            ));
        }
        if usize::from(max_v) > k {
            return Err(syn::Error::new_spanned(
                &p.max,
                format!("pool `max:` ({max_v}) exceeds the declared member count ({k})"),
            ));
        }
    }

    if !p.resources.is_empty() && matches!(p.source, TaskSource::Spawn(_)) {
        return Err(syn::Error::new_spanned(
            &p.resources[0].ident,
            "pool `resources:` requires `task:` — the values are handed to the \
             generated shell as arguments (and lend entries restored by it); a \
             `spawn:` task fn manages its own arguments",
        ));
    }
    if let Some((_, ty, _)) = &p.state
        && matches!(p.source, TaskSource::Spawn(_))
    {
        return Err(syn::Error::new_spanned(
            ty,
            "pool `state:` requires `task:` — the generated shell owns the boxed \
             state across the worker call; a `spawn:` task fn can Box its own",
        ));
    }

    let (shell_def, member_expr) = match &p.source {
        TaskSource::Spawn(e) => (quote!(), e.clone()),
        TaskSource::Shell(worker) => emit_shell(
            ident,
            cfg,
            worker,
            k,
            &p.resources,
            &claims,
            None,
            p.state.as_ref().map(|(_, ty, init)| (ty, init)),
            true,
            p.cancel,
            cr,
            helpers,
        )?,
    };
    let res_args: Vec<TokenStream2> = p
        .resources
        .iter()
        .zip(&claims)
        .filter_map(|(r, base)| {
            let ecfg = &r.cfg;
            let res = &r.ident;
            match r.kind() {
                ResourceKind::Lend | ResourceKind::Consume => Some(quote!(#(#ecfg)* &#res[I])),
                ResourceKind::Divisible => {
                    let base = base.expect("a divisible entry has a slot");
                    Some(quote!(#(#ecfg)* #res.claimant((#base as usize + I) as u8)))
                }
                ResourceKind::Shared => None,
            }
        })
        .collect();
    let (state_prelude, state_arg) = match &p.state {
        Some((_, ty, init)) => (state_box_stmt(ty, init, helpers), vec![quote!(__state)]),
        None => (quote!(), vec![]),
    };
    let mut lead: Vec<TokenStream2> = vec![quote!(&#ident[I])];
    lead.extend(res_args);
    lead.extend(state_arg);
    let call = inject_call_with(&member_expr, &lead)?;
    let (param, prelude, sp_tokens) = match &p.executor {
        None => (quote!(s), quote!(), quote!(s)),
        Some(ex) => (
            quote!(_s),
            quote! {
                let __sp = #ex
                    .get()
                    .ok_or(::embassy_executor::SpawnError::Busy)?;
            },
            quote!(__sp),
        ),
    };
    let get_prelude: Vec<TokenStream2> = p
        .resources
        .iter()
        .map(|r| {
            let ecfg = &r.cfg;
            let res = &r.ident;
            let slot = match r.kind() {
                ResourceKind::Lend | ResourceKind::Consume => quote!(#res[I]),
                ResourceKind::Shared | ResourceKind::Divisible => quote!(#res),
            };
            quote! {
                #(#ecfg)*
                if !#cr::ResourceGate::is_filled(&#slot) {
                    return ::core::result::Result::Err(::embassy_executor::SpawnError::Busy);
                }
            }
        })
        .collect();
    let pool_spawn_stmts = spawn_stmts(&call, &quote!(&#ident[I]), &sp_tokens);
    let wrapper = format_ident!("spawn_{}", ident.to_string().to_lowercase());
    let mut defs: Vec<TokenStream2> = Vec::new();
    defs.push(shell_def);
    defs.push(quote! {
        #(#cfg)*
        fn #wrapper<const I: usize>(
            #param: ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError> {
            #prelude
            #state_prelude
            #(#get_prelude)*
            #pool_spawn_stmts
            ::core::result::Result::Ok(())
        }
    });
    let member_spawn: Vec<TokenStream2> = (0..k).map(|j| quote!(#wrapper::<#j>)).collect();

    let member_with_exec = match &p.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    for r in p
        .resources
        .iter()
        .filter(|r| matches!(r.kind(), ResourceKind::Lend | ResourceKind::Consume))
    {
        let ecfg = &r.cfg;
        let res = &r.ident;
        let ty = r.ty.as_ref().expect("a typed resource kind");
        let doc = format!(
            "Per-member resource slots for pool `{ident}` (generated by \
             `supervisor_graph!`): member `I` takes/restores element `I`. \
             Provide at least the floor members' elements before \
             `Supervisor::start`; a member whose element is empty fail-closes \
             its (re)spawn with `SpawnError::Busy`."
        );
        defs.push(quote! {
            #(#cfg)*
            #(#ecfg)*
            #[doc = #doc]
            pub static #res: [#cr::ResourceSlot<#ty>; #k] =
                [const { #cr::ResourceSlot::new() }; #k];
        });
    }
    let member_with_res: Vec<TokenStream2> = if p.resources.is_empty() {
        (0..k).map(|_| quote!()).collect()
    } else {
        let gates_len = cfg_aware_len(p.resources.iter().map(|r| &r.cfg));
        (0..k)
            .map(|j| {
                let gates_ident = format_ident!("__SV_GATES_{}_{}", ident, j);
                let gate_refs: Vec<TokenStream2> = p
                    .resources
                    .iter()
                    .map(|r| {
                        let ecfg = &r.cfg;
                        let res = &r.ident;
                        match r.kind() {
                            ResourceKind::Lend | ResourceKind::Consume => {
                                quote!(#(#ecfg)* &#res[#j])
                            }
                            ResourceKind::Shared | ResourceKind::Divisible => {
                                quote!(#(#ecfg)* &#res)
                            }
                        }
                    })
                    .collect();
                defs.push(quote! {
                    #(#cfg)*
                    static #gates_ident: [&'static dyn #cr::ResourceGate; #gates_len] =
                        [#(#gate_refs),*];
                });
                quote!( .with_resources(&#gates_ident) )
            })
            .collect()
    };
    let member_with_claims: Vec<TokenStream2> = (0..k)
        .map(|j| {
            let (def, with) = claims_tokens(
                &format_ident!("__SV_CLAIMS_{}_{}", ident, j),
                cfg,
                &p.resources,
                &claims,
                j,
                cr,
            );
            defs.push(def);
            with
        })
        .collect();
    // `deps: [X ready, ..]` — ONE shared ready-dep array for the whole pool
    // (markers apply to every member; growth also checks it synchronously).
    // The three dep-marker overlays and the two coupling tables: one table per
    // POOL, shared by every member. A pool is a single declaration instantiated
    // K times, so its members gate on the same deps and exchange the same
    // signals — and sharing the statics keeps the flash cost independent of the
    // member count.
    let ctx = EmitCtx {
        cfg,
        cr,
        owner: ident,
    };
    let marker = |select: fn(&Dep) -> bool, prefix, builder| {
        marker_array_tokens(&ctx, &p.deps, pool_names, select, prefix, builder)
    };
    let (ready_def, member_with_ready) = marker(|d| d.ready.is_some(), "READY", "with_ready_deps");
    let (bound_def, member_with_bound) = marker(|d| d.bound.is_some(), "BOUND", "with_bound_deps");
    let mut foreign: Vec<AdoptedFn> = Vec::new();
    foreign.extend(
        discover_fn_path(p.discover.as_ref().map(|g| &g.kw), Some(&p.source))?
            .filter(|_| cfg!(feature = "dataflow"))
            .map(|path| AdoptedFn {
                cfg: p
                    .discover
                    .as_ref()
                    .map(|g| g.cfg.clone())
                    .unwrap_or_default(),
                path,
            }),
    );
    foreign.extend(p.dataflow.iter().cloned());
    let (reads_def, member_with_reads) = coupling_binding_tokens(
        &ctx,
        &p.reads,
        "READS",
        "with_reads",
        observe.reads.as_ref(),
        &foreign,
        VetoSlots::default(),
    );
    // A `veto` write gives every member its own contributor slot, so such a
    // pool emits one writes table per member (the flash cost is the opt-in's);
    // any other pool shares one table across its members, as before.
    let veto_bases = plan.veto_slots(&p.ident, &p.writes);
    let (writes_def, member_with_writes): (TokenStream2, Vec<TokenStream2>) =
        if veto_bases.iter().any(Option::is_some) {
            let mut def = quote!();
            let withs = (0..k)
                .map(|j| {
                    let member_ident = format_ident!("{}_{}", ident, j);
                    let member_ctx = EmitCtx {
                        cfg,
                        cr,
                        owner: &member_ident,
                    };
                    let (d, w) = coupling_binding_tokens(
                        &member_ctx,
                        &p.writes,
                        "WRITES",
                        "with_writes",
                        observe.writes.as_ref(),
                        &foreign,
                        VetoSlots {
                            bases: &veto_bases,
                            offset: j,
                        },
                    );
                    def.extend(d);
                    w
                })
                .collect();
            (def, withs)
        } else {
            let (d, w) = coupling_binding_tokens(
                &ctx,
                &p.writes,
                "WRITES",
                "with_writes",
                observe.writes.as_ref(),
                &foreign,
                VetoSlots::default(),
            );
            (d, (0..k).map(|_| w.clone()).collect())
        };
    // Empty token streams for absent clauses, so pushing unconditionally is a
    // no-op rather than a special case.
    {
        let d = p.discover.as_ref().map(|g| g.cfg.as_slice());
        let owner = name_string(&p.ident);
        defs.push(marker_assert_tokens(
            &ctx, &owner, d, &foreign, &p.reads, "READS",
        ));
        defs.push(marker_assert_tokens(
            &ctx, &owner, d, &foreign, &p.writes, "WRITES",
        ));
    }
    defs.extend([ready_def, bound_def, reads_def, writes_def]);

    let (member_with_clauses, member_clause_stmts, member_clause_errors) = builder_clause_tokens(
        cfg,
        [
            p.slot_timeout.as_ref().map(|g| {
                let (c, ms) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_slot_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                    available: true,
                    err: "",
                }
            }),
            p.ack_timeout.as_ref().map(|g| {
                let (c, ms) = (&g.cfg, &g.value);
                GatedBuilder {
                    cfg: c.clone(),
                    tokens: quote!( .with_ack_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                    available: true,
                    err: "",
                }
            }),
        ]
        .into_iter()
        .flatten(),
    );
    defs.push(member_clause_errors);
    defs.push(discover_error_tokens(cfg, p.discover.as_ref()));
    let member_graph_ref = &helpers.graph_ref;
    let member_with_graph =
        cfg!(feature = "data-deps").then(|| quote!( .with_graph(&#member_graph_ref) ));
    let member_cfg_ident = format_ident!("__SV_CFG_{}", ident);
    let member_cfgs = p
        .modes
        .iter()
        .zip(&member_spawn)
        .enumerate()
        .map(|(j, (mode, sp))| {
            let nm = format!("{lname}{j}");
            let with_res = &member_with_res[j];
            let with_claims = &member_with_claims[j];
            let member_with_writes = &member_with_writes[j];
            let chain = quote! {
                #cr::NodeCfg::new(
                    #nm, #cr::Mode::#mode,
                    ::core::option::Option::Some((#sp) as #spawn_fn),
                ) #member_with_exec #with_res #with_claims #member_with_clauses
                  #member_with_ready
                  #member_with_bound
                  #member_with_reads #member_with_writes #member_with_graph
            };
            if member_clause_stmts.is_empty() {
                chain
            } else {
                quote! {{
                    let __sv_cfg = #chain;
                    #member_clause_stmts
                    __sv_cfg
                }}
            }
        });
    let members =
        (0..p.modes.len()).map(|j| quote!( #cr::TaskNode::new(&#member_cfg_ident[#j], false) ));
    defs.push(quote! {
        #(#cfg)*
        #[doc(hidden)]
        static #member_cfg_ident: [#cr::NodeCfg; #k] = [ #(#member_cfgs),* ];
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s members, one `TaskNode` per slot \
            (index = member index). Index it for the per-node verbs; the pool itself is \
            `", stringify!(#ident), "_POOL`.")]
        pub static #ident: [#cr::TaskNode; #k] = [ #(#members),* ];
    });

    let min_const = format_ident!("{}_MIN", ident);
    let max_const = format_ident!("{}_MAX", ident);
    let members_const = format_ident!("{}_MEMBERS", ident);
    let (min_tokens, max_tokens, bound_asserts) = match lit_bounds {
        Some((min_v, max_v)) => {
            let (min_u, max_u) = (usize::from(min_v), usize::from(max_v));
            (quote!(#min_u), quote!(#max_u), quote!())
        }
        None => {
            let (min_e, max_e) = (&p.min, &p.max);
            (
                quote!({ #min_e }),
                quote!({ #max_e }),
                quote! {
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #min_const <= #max_const,
                        "pool `min:` must not exceed `max:`",
                    );
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #max_const <= #members_const,
                        "pool `max:` exceeds the declared member count",
                    );
                    #(#cfg)*
                    const _: () = ::core::assert!(
                        #max_const <= 255,
                        "pool `max:` exceeds 255 (ElasticPool bounds are u8)",
                    );
                },
            )
        }
    };
    defs.push(quote! {
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s `min:` floor (validated at expansion or by const assert).")]
        pub const #min_const: usize = #min_tokens;
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s `max:` scaling ceiling — the most members ever running concurrently.")]
        pub const #max_const: usize = #max_tokens;
        #(#cfg)*
        #[doc = concat!("Pool `", stringify!(#ident), "`'s declared member count (the `[TaskNode; K]` array length).")]
        pub const #members_const: usize = #k;
        #bound_asserts
    });

    let member_refs = (0..k).map(|j| quote!(&#ident[#j]));
    let policy = &p.policy;
    let policy_ty = match &p.policy_ty {
        Some(ty) => quote!(#ty),
        None => {
            let path = policy_type(policy)?;
            quote!(#path)
        }
    };
    defs.push(quote! {
        #(#cfg)*
        #[doc = concat!("The `ElasticPool` over the `", stringify!(#ident), "` members: \
            the `min:`/`max:` bounds and the scaling policy `Supervisor::run_pools` \
            drives. Also reachable through `GRAPH.pools`.")]
        pub static #pool_static: #cr::ElasticPool<#policy_ty> = #cr::ElasticPool {
            nodes: &[ #(#member_refs),* ],
            min: #min_const as u8,
            max: #max_const as u8,
            policy: #policy,
        };
    });

    let pool_entry = quote!( #(#cfg)* &#pool_static );

    let pred = cfg_predicate(cfg);
    let slots = (0..k)
        .map(|j| Slot {
            cfg_pred: pred.clone(),
            reference: quote!(&#ident[#j]),
            deps: p.deps.clone(),
            fragment: p.fragment.clone(),
        })
        .collect();

    Ok((defs, pool_entry, slots))
}

fn slot_tables(
    slots: &[Slot],
    names: &HashMap<String, usize>,
) -> SynResult<(Vec<TokenStream2>, Vec<TokenStream2>)> {
    let mut all_entries: Vec<TokenStream2> = Vec::new();
    let mut deps_entries: Vec<TokenStream2> = Vec::new();
    for slot in slots {
        let reference = &slot.reference;
        all_entries.push(match &slot.cfg_pred {
            None => quote!(::core::option::Option::Some(#reference)),
            Some(pred) => quote!({
                #[cfg(#pred)]
                { ::core::option::Option::Some(#reference) }
                #[cfg(not(#pred))]
                { ::core::option::Option::None }
            }),
        });

        let mut dep_toks: Vec<TokenStream2> = Vec::new();
        let mut seen: Vec<(u8, String)> = Vec::new();
        for d in &slot.deps {
            let idx = match names.get(&d.ident.to_string()) {
                Some(&i) => i as u8,
                None => {
                    return Err(syn::Error::new_spanned(
                        &d.ident,
                        format!(
                            "unknown dependency `{}` — not a declared node or pool{}",
                            d.ident,
                            fragment_suffix(&slot.fragment),
                        ),
                    ));
                }
            };
            let cfg = &d.cfg;
            let cfg_key = quote!( #(#cfg)* ).to_string();
            if seen.iter().any(|(i, k)| *i == idx && *k == cfg_key) {
                return Err(syn::Error::new_spanned(
                    &d.ident,
                    format!("duplicate dependency `{}`", d.ident),
                ));
            }
            seen.push((idx, cfg_key));
            dep_toks.push(quote!( #(#cfg)* #idx ));
        }
        deps_entries.push(quote!( &[ #(#dep_toks),* ] ));
    }
    Ok((all_entries, deps_entries))
}

fn expand(graph: GraphSpec) -> SynResult<TokenStream2> {
    gate::gate(&graph)?;
    let cr = quote!(::embassy_supervisor);
    let helpers = HelperIdents::new(graph.name.as_ref());
    let observe = ObserveDefaults {
        writes: graph.observe_writes.as_ref().map(|(_, e)| e.clone()),
        reads: graph.observe_reads.as_ref().map(|(_, e)| e.clone()),
    };
    let spawn_fn = quote!(
        fn(
            ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError>
    );

    let mut defs: Vec<TokenStream2> = Vec::new();
    let mut pool_entries: Vec<TokenStream2> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();

    let has_state = |want: fn(&StateInit) -> bool| {
        graph.items.iter().any(|item| match item {
            Item::Node(n) => n.state.as_ref().is_some_and(|(_, _, i)| want(i)),
            Item::Pool(p) => p.state.as_ref().is_some_and(|(_, _, i)| want(i)),
            Item::Executor(_) => false,
        })
    };
    let any_init_state = has_state(|i| matches!(i, StateInit::Expr(_)));
    let any_zeroed_state = has_state(|i| matches!(i, StateInit::Zeroed(_)));
    let alloc_alias = &helpers.alloc_alias;
    if any_init_state || any_zeroed_state {
        defs.push(quote! {
            extern crate alloc as #alloc_alias;
        });
    }
    if any_init_state {
        let try_box = &helpers.try_box;
        defs.push(quote! {
            #[doc(hidden)]
            fn #try_box<T>(init: T) -> ::core::option::Option<#alloc_alias::boxed::Box<T>> {
                let layout = ::core::alloc::Layout::new::<T>();
                if layout.size() == 0 {
                    ::core::mem::forget(init);
                    return ::core::option::Option::Some(unsafe {
                        #alloc_alias::boxed::Box::from_raw(
                            ::core::ptr::NonNull::<T>::dangling().as_ptr(),
                        )
                    });
                }
                unsafe {
                    let p = #alloc_alias::alloc::alloc(layout) as *mut T;
                    if p.is_null() {
                        return ::core::option::Option::None;
                    }
                    ::core::ptr::write(p, init);
                    ::core::option::Option::Some(#alloc_alias::boxed::Box::from_raw(p))
                }
            }
        });
    }
    if any_zeroed_state {
        let try_box_zeroed = &helpers.try_box_zeroed;
        defs.push(quote! {
            #[doc(hidden)]
            fn #try_box_zeroed<T: #cr::Zeroable>() -> ::core::option::Option<#alloc_alias::boxed::Box<T>> {
                let layout = ::core::alloc::Layout::new::<T>();
                if layout.size() == 0 {
                    return ::core::option::Option::Some(unsafe {
                        #alloc_alias::boxed::Box::from_raw(
                            ::core::ptr::NonNull::<T>::dangling().as_ptr(),
                        )
                    });
                }
                unsafe {
                    let p = #alloc_alias::alloc::alloc_zeroed(layout) as *mut T;
                    if p.is_null() {
                        return ::core::option::Option::None;
                    }
                    ::core::option::Option::Some(#alloc_alias::boxed::Box::from_raw(p))
                }
            }
        });
    }

    let any_local = graph
        .items
        .iter()
        .any(|item| item_resources(item).iter().any(|r| r.local.is_some()));
    if any_local {
        let local = helpers.local_slot.clone();
        let cell = quote!(::core::cell::Cell<::core::option::Option<T>>);
        let raw = quote!(#cr::_export::CriticalSectionRawMutex);
        let signal = quote!(#cr::_export::Signal<#raw, ()>);
        defs.push(quote! {
            #[allow(dead_code)]
            pub struct #local<T> {
                slot: #cr::_export::BlockingMutex<#raw, #cell>,
                filled: #signal,
            }
            unsafe impl<T> ::core::marker::Sync for #local<T> {}
            #[allow(dead_code)]
            impl<T> #local<T> {
                pub const fn new() -> Self {
                    Self {
                        slot: #cr::_export::BlockingMutex::new(
                            ::core::cell::Cell::new(::core::option::Option::None),
                        ),
                        filled: #cr::_export::Signal::new(),
                    }
                }
                pub fn provide(&self, value: T) {
                    self.slot.lock(|c| c.set(::core::option::Option::Some(value)));
                    self.filled.signal(());
                    #cr::__sv_gate_event();
                }
                pub fn take(&self) -> ::core::option::Option<T> {
                    self.slot.lock(::core::cell::Cell::take)
                }
                pub fn restore(&self, value: T) {
                    self.provide(value);
                }
            }
            #[allow(dead_code)]
            impl<T: ::core::marker::Copy> #local<T> {
                pub fn get(&self) -> ::core::option::Option<T> {
                    self.slot.lock(|c| {
                        let v = c.take();
                        c.set(v);
                        v
                    })
                }
            }
            impl<T> ::core::default::Default for #local<T> {
                fn default() -> Self {
                    Self::new()
                }
            }
            impl<T> #cr::ResourceGate for #local<T> {
                fn is_filled(&self) -> bool {
                    self.slot.lock(|c| {
                        let v = c.take();
                        let filled = v.is_some();
                        c.set(v);
                        filled
                    })
                }
                fn filled_signal(&self) -> &#signal {
                    &self.filled
                }
                fn clear(&self) {
                    let stale = self.slot.lock(::core::cell::Cell::take);
                    drop(stale);
                    self.filled.reset();
                }
            }
        });
    }

    let helpers = HelperIdents::new(graph.name.as_ref());
    let executor_names: Vec<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Executor(x) => Some(x.ident.to_string()),
            _ => None,
        })
        .collect();
    let pool_names: std::collections::HashSet<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Pool(p) => Some(p.ident.to_string()),
            _ => None,
        })
        .collect();

    /// A graph-wide slot — a `shared` slot or a `divisible` budget — that
    /// several items may declare and the graph emits once.
    struct GraphSlotPlan<'a> {
        /// First declaration — supplies the emitted static's ident (span), type,
        decl: &'a ResourceDecl,
        /// Kinds+type token string every re-declaration must match.
        sig: String,
        /// One entry per declaring site: `None` = unconditional (the slot is
        /// then unconditional too), `Some(pred)` = that site's combined
        preds: Vec<Option<TokenStream2>>,
        owners: Vec<String>,
        /// The first declarer's `executor:` (`None` = the supervisor's own),
        /// which a `serialized` slot holds every other declarer to.
        executor: Option<String>,
        /// `divisible`: claimant slots handed out so far (a node takes one, a
        /// pool one per member), which sizes the emitted `Budget<K>`.
        slots: usize,
    }
    let mut shared_plans: Vec<(String, GraphSlotPlan)> = Vec::new();
    // (owner, resource) -> the owner's base slot in that budget.
    let mut claim_bases: HashMap<(String, String), u8> = HashMap::new();
    {
        let mut taken: HashSet<String> = HashSet::new();
        for item in &graph.items {
            let Some((owner, item_cfg)) = item_ident_cfg(item) else {
                continue;
            };
            let item_pred = cfg_predicate(item_cfg);
            let member_count = match item {
                Item::Pool(p) => p.modes.len(),
                _ => 1,
            };
            let executor_text = item_executor(item).map(ToString::to_string);
            for r in item_resources(item) {
                let key = r.ident.to_string();
                if executor_names.contains(&key) {
                    return Err(syn::Error::new_spanned(
                        &r.ident,
                        format!(
                            "resource name `{}` shadows an `executor {};` slot — \
                             both are statics at the declaration site",
                            r.ident, r.ident
                        ),
                    ));
                }
                let pred = match (item_pred.clone(), cfg_predicate(&r.cfg)) {
                    (None, None) => None,
                    (Some(p), None) | (None, Some(p)) => Some(p),
                    (Some(a), Some(b)) => Some(quote!(all(#a, #b))),
                };
                if matches!(r.kind(), ResourceKind::Shared | ResourceKind::Divisible) {
                    if taken.contains(&key) {
                        return Err(syn::Error::new_spanned(
                            &r.ident,
                            format!(
                                "`{}` is already a take-kind resource elsewhere in \
                                 the graph — a name is either one exclusive slot or \
                                 one `shared`/`divisible` slot, not both",
                                r.ident
                            ),
                        ));
                    }
                    let sig = if r.kind() == ResourceKind::Divisible {
                        "divisible".to_string()
                    } else {
                        r.shared_signature()
                    };
                    let plan = match shared_plans.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, plan)) => {
                            if plan.sig != sig {
                                return Err(syn::Error::new_spanned(
                                    &r.ident,
                                    format!(
                                        "shared resource `{}` re-declared with a \
                                         different shape: `{}` here vs `{}` on \
                                         `{}` — every declaration of a shared slot \
                                         must repeat the same kind markers and type",
                                        r.ident, sig, plan.sig, plan.owners[0]
                                    ),
                                ));
                            }
                            // `serialized`: every holder on one executor, so no
                            // higher-tier waiter can be starved by a lower-tier
                            // holder — priority ceiling by construction, since
                            // embassy can neither boost a holder nor migrate a
                            // task. Syntactic: `#[cfg]`s are not consulted.
                            if let Some(marker) = &r.serialized
                                && executor_text != plan.executor
                            {
                                let tier = |e: &Option<String>| match e {
                                    Some(x) => format!("`{x}`"),
                                    None => "the supervisor's executor".to_string(),
                                };
                                return Err(syn::Error::new_spanned(
                                    marker,
                                    format!(
                                        "`{}` is `serialized`: every holder must run on one \
                                         executor so no higher-tier waiter can be starved by \
                                         a lower-tier holder (priority ceiling by \
                                         construction), but `{}` runs on {} and `{}` on {}",
                                        r.ident,
                                        plan.owners[0],
                                        tier(&plan.executor),
                                        owner,
                                        tier(&executor_text),
                                    ),
                                ));
                            }
                            plan.preds.push(pred);
                            plan.owners.push(owner.to_string());
                            plan
                        }
                        None => {
                            shared_plans.push((
                                key.clone(),
                                GraphSlotPlan {
                                    decl: r,
                                    sig,
                                    preds: vec![pred],
                                    owners: vec![owner.to_string()],
                                    executor: executor_text.clone(),
                                    slots: 0,
                                },
                            ));
                            &mut shared_plans.last_mut().expect("just pushed").1
                        }
                    };
                    if r.kind() == ResourceKind::Divisible {
                        // Counted syntactically: a `#[cfg]`'d-out declarer still
                        // takes its slots, so the budget can only be oversized.
                        let base = u8::try_from(plan.slots)
                            .ok()
                            .filter(|_| plan.slots + member_count <= usize::from(u8::MAX) + 1);
                        let Some(base) = base else {
                            return Err(syn::Error::new_spanned(
                                &r.ident,
                                format!(
                                    "divisible resource `{}` has more than 256 claimant \
                                     slots across the graph — slots are `u8`",
                                    r.ident
                                ),
                            ));
                        };
                        claim_bases.insert((owner.to_string(), key), base);
                        plan.slots += member_count;
                    }
                } else {
                    if !taken.insert(key.clone()) || shared_plans.iter().any(|(k, _)| *k == key) {
                        return Err(syn::Error::new_spanned(
                            &r.ident,
                            format!(
                                "duplicate resource name `{}` — resource slots are \
                                 statics and must be unique across the graph (only \
                                 `shared` entries may repeat a name)",
                                r.ident
                            ),
                        ));
                    }
                }
            }
        }
    }
    for (_, plan) in &shared_plans {
        let res = &plan.decl.ident;
        let cfg_attr = if plan.preds.iter().any(|p| p.is_none()) {
            quote!()
        } else {
            let preds = plan.preds.iter().flatten();
            quote!(#[cfg(any(#(#preds),*))])
        };
        if plan.decl.kind() == ResourceKind::Divisible {
            let k = plan.slots;
            let doc = format!(
                "Divisible budget declared by `{}` (generated by `supervisor_graph!`): \
                 {k} claimant slot(s), one per declaring node or pool member, in \
                 declaration order. `provide()` the capacity before the holders \
                 start (or from an allocator node that names it in `provides:`), \
                 and `rebalance()` it with a `BudgetPolicy` when the wants move.",
                plan.owners.join("`, `"),
            );
            defs.push(quote! {
                #cfg_attr
                #[doc = #doc]
                pub static #res: #cr::Budget<#k> = #cr::Budget::new();
            });
            continue;
        }
        let ty = plan.decl.ty.as_ref().expect("a typed resource kind");
        let slot_ty = if plan.decl.local.is_some() {
            let local = &helpers.local_slot;
            quote!(#local<#ty>)
        } else {
            quote!(#cr::ResourceSlot<#ty>)
        };
        let doc = format!(
            "Shared (fan-out) resource slot declared by `{}` (generated by \
             `supervisor_graph!`). `provide()` the `Copy` handle before \
             `Supervisor::start`; every consumer's glue copies it out with \
             `get()`, so the slot STAYS FILLED — re-`provide()` only to replace \
             the handle (e.g. after rebuilding the underlying driver).",
            plan.owners.join("`, `"),
        );
        defs.push(quote! {
            #cfg_attr
            #[doc = #doc]
            pub static #res: #slot_ty = <#slot_ty>::new();
        });
    }

    let mut resource_cfgs: HashMap<String, (Vec<Attribute>, bool)> = HashMap::new();
    for item in &graph.items {
        let from_pool = matches!(item, Item::Pool(_));
        for r in item_resources(item) {
            let per_member = matches!(r.kind(), ResourceKind::Lend | ResourceKind::Consume);
            resource_cfgs
                .entry(r.ident.to_string())
                .or_insert_with(|| (r.cfg.clone(), from_pool && per_member));
        }
    }
    // `veto` contributor slots: per gate (by its display text), writers are
    // numbered in item order — a node takes one slot, a pool one per member —
    // and the gate is checked once for the total. Counted syntactically, so a
    // `#[cfg]`'d-out writer still reserves its slot: the check can only be
    // stricter than the build. The text is load-bearing here (the slot is a
    // bit of the static it resolves to), so one gate named two ways — `TRIP`
    // beside `crate::TRIP` — is rejected rather than numbered twice.
    struct VetoPlan {
        key: String,
        target: TokenStream2,
        total: usize,
        /// One entry per writer, `None` = unconditional: gates the check the
        /// way `GraphSlotPlan::preds` gates a shared static, so a gate whose
        /// writers all sit behind a `#[cfg]` is not named in a build without it.
        preds: Vec<Option<TokenStream2>>,
    }
    let mut veto_bases: HashMap<(String, String), u8> = HashMap::new();
    let mut veto_plans: Vec<VetoPlan> = Vec::new();
    // Last path segment (plus index) -> the first spelling seen for it.
    let mut veto_stems: HashMap<String, String> = HashMap::new();
    for item in &graph.items {
        let Some((owner, item_cfg)) = item_ident_cfg(item) else {
            continue;
        };
        let item_pred = cfg_predicate(item_cfg);
        let member_count = match item {
            Item::Pool(p) => p.modes.len(),
            _ => 1,
        };
        let writes = match item {
            Item::Node(n) => &n.writes[..],
            Item::Pool(p) => &p.writes[..],
            Item::Executor(_) => &[][..],
        };
        for d in writes.iter().filter(|d| d.veto.is_some()) {
            let key = d.display();
            let stem = match (d.path.segments.last(), key.find('[')) {
                (Some(last), Some(at)) => format!("{}{}", last.ident, &key[at..]),
                (Some(last), None) => last.ident.to_string(),
                (None, _) => key.clone(),
            };
            match veto_stems.get(&stem) {
                Some(first) if *first != key => {
                    return Err(syn::Error::new_spanned(
                        &d.path,
                        format!(
                            "`{first}` and `{key}` both name a `veto` gate ending in \
                             `{stem}`: contributor slots are numbered per spelling, so \
                             one static named two ways would hand two writers the same \
                             bit — spell the gate one way across the graph, or alias one \
                             of two distinct statics with `use .. as`"
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    veto_stems.insert(stem, key.clone());
                }
            }
            let pred = match (item_pred.clone(), cfg_predicate(&d.cfg)) {
                (None, None) => None,
                (Some(p), None) | (None, Some(p)) => Some(p),
                (Some(a), Some(b)) => Some(quote!(all(#a, #b))),
            };
            let plan = match veto_plans.iter_mut().find(|p| p.key == key) {
                Some(plan) => plan,
                None => {
                    veto_plans.push(VetoPlan {
                        key: key.clone(),
                        target: d.target(),
                        total: 0,
                        preds: Vec::new(),
                    });
                    veto_plans.last_mut().expect("just pushed")
                }
            };
            if plan.total + member_count > 32 {
                return Err(syn::Error::new_spanned(
                    &d.path,
                    format!(
                        "`{key}` has more than 32 `veto` writers across the graph — a \
                         `VetoGate` holds at most 32 contributors"
                    ),
                ));
            }
            veto_bases.insert((owner.to_string(), key), plan.total as u8);
            plan.total += member_count;
            plan.preds.push(pred);
        }
    }
    for plan in &veto_plans {
        let (target, total) = (&plan.target, plan.total);
        let cfg_attr = if plan.preds.iter().any(|p| p.is_none()) {
            quote!()
        } else {
            let preds = plan.preds.iter().flatten();
            quote!(#[cfg(any(#(#preds),*))])
        };
        defs.push(quote! {
            #cfg_attr
            const _: () = #cr::__sv_check_veto(&#target, #total);
        });
    }
    let plan = ResourcePlan {
        cfgs: resource_cfgs,
        claim_bases,
        veto_bases,
    };

    for item in &graph.items {
        match item {
            Item::Node(n) => {
                if let Some(ex) = &n.executor
                    && !executor_names.contains(&ex.to_string())
                {
                    return Err(syn::Error::new_spanned(
                        ex,
                        format!(
                            "unknown executor `{ex}`; declare it in the graph with \
                             `executor {ex};` (declared: [{}])",
                            executor_names.join(", ")
                        ),
                    ));
                }
                if names.insert(n.ident.to_string(), slots.len()).is_some() {
                    return Err(syn::Error::new_spanned(
                        &n.ident,
                        format!(
                            "duplicate node/pool name `{}`{}",
                            n.ident,
                            fragment_suffix(&n.fragment),
                        ),
                    ));
                }
                let (def, slot) =
                    emit_node(n, &cr, &spawn_fn, &pool_names, &helpers, &observe, &plan)?;
                defs.push(def);
                slots.push(slot);
            }
            Item::Executor(x) => {
                let (cfg, ident) = (&x.cfg, &x.ident);
                defs.push(quote! {
                    #(#cfg)*
                    pub static #ident: #cr::SpawnerSlot = #cr::SpawnerSlot::new();
                });
            }
            Item::Pool(p) => {
                if cfg!(feature = "pool") {
                    if let Some(ex) = &p.executor
                        && !executor_names.contains(&ex.to_string())
                    {
                        return Err(syn::Error::new_spanned(
                            ex,
                            format!(
                                "unknown executor `{ex}`; declare it in the graph with \
                                 `executor {ex};` (declared: [{}])",
                                executor_names.join(", ")
                            ),
                        ));
                    }
                    let (pool_defs, pool_entry, pool_slots) =
                        emit_pool(p, &cr, &spawn_fn, &pool_names, &helpers, &observe, &plan)?;
                    if names.insert(p.ident.to_string(), slots.len()).is_some() {
                        return Err(syn::Error::new_spanned(
                            &p.ident,
                            format!(
                                "duplicate node/pool name `{}`{}",
                                p.ident,
                                fragment_suffix(&p.fragment),
                            ),
                        ));
                    }
                    defs.extend(pool_defs);
                    pool_entries.push(pool_entry);
                    slots.extend(pool_slots);
                } else {
                    return Err(syn::Error::new_spanned(
                        &p.ident,
                        "a `pool` requires enabling embassy-supervisor's `pool` feature",
                    ));
                }
            }
        }
    }

    let m = slots.len();
    if m > 256 {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "supervisor_graph!: {m} node slots declared, but at most 256 are supported \
                 (including pool members) — graph indices are `u8`"
            ),
        ));
    }
    let (all_entries, deps_entries) = slot_tables(&slots, &names)?;

    let shape_bits: u32 = {
        const READY_DEPS: u32 = 1 << 0;
        const EXEC_SLOTS: u32 = 1 << 1;
        const RESOURCES: u32 = 1 << 2;
        const PAUSE: u32 = 1 << 3;
        const ON_DEMAND: u32 = 1 << 4;
        const BEATS: u32 = 1 << 5;
        const OBSERVED: u32 = 1 << 6;
        const BOUND_DEPS: u32 = 1 << 7;
        const POOLS: u32 = 1 << 8;
        const CLAIMS: u32 = 1 << 9;
        let claims_bit = |resources: &[ResourceDecl]| {
            if resources
                .iter()
                .any(|r| r.kind() == ResourceKind::Divisible)
            {
                CLAIMS
            } else {
                0
            }
        };
        let mode_bit = |m: &Ident| match m.to_string().as_str() {
            "Pause" => PAUSE,
            "OnDemand" => ON_DEMAND,
            _ => 0,
        };
        let dep_bits = |deps: &[Dep]| {
            let mut b = 0;
            for d in deps {
                if d.ready.is_some() || d.bound.is_some() {
                    b |= READY_DEPS;
                }
                if d.bound.is_some() {
                    b |= BOUND_DEPS;
                }
            }
            b
        };
        let observed_bit = |reads: &[SignalDecl], writes: &[SignalDecl]| {
            if reads.iter().chain(writes).any(|s| s.observed.is_some()) {
                OBSERVED
            } else {
                0
            }
        };
        let mut bits = 0;
        for item in &graph.items {
            match item {
                Item::Node(n) => {
                    bits |= mode_bit(&n.mode);
                    bits |= dep_bits(&n.deps);
                    bits |= observed_bit(&n.reads, &n.writes);
                    if n.executor.is_some() {
                        bits |= EXEC_SLOTS;
                    }
                    if !n.resources.is_empty() {
                        bits |= RESOURCES;
                    }
                    bits |= claims_bit(&n.resources);
                    if n.beat_timeout.is_some() {
                        bits |= BEATS;
                    }
                    if n.ready_on_write.is_some() {
                        bits |= OBSERVED;
                    }
                }
                Item::Pool(p) => {
                    bits |= POOLS;
                    for m in &p.modes {
                        bits |= mode_bit(m);
                    }
                    bits |= dep_bits(&p.deps);
                    bits |= observed_bit(&p.reads, &p.writes);
                    if p.executor.is_some() {
                        bits |= EXEC_SLOTS;
                    }
                    if !p.resources.is_empty() {
                        bits |= RESOURCES;
                    }
                    bits |= claims_bit(&p.resources);
                }
                Item::Executor(_) => {}
            }
        }
        bits
    };
    let shape_lit = proc_macro2::Literal::u32_suffixed(shape_bits);
    let flat = slots.iter().all(|s| s.deps.is_empty());

    let pools_field = if cfg!(feature = "pool") {
        quote!( pools: &[ #(#pool_entries),* ], )
    } else {
        quote!()
    };

    let trace_hooks = if cfg!(feature = "trace-hooks") && graph.name.is_none() {
        quote! {
            #[unsafe(no_mangle)]
            fn _embassy_trace_poll_start(executor_id: u32) {
                #cr::trace::on_poll_start(executor_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_new(_executor_id: u32, _task_id: u32) {}
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_end(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_end(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_exec_begin(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_exec_begin(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_exec_end(executor_id: u32, task_id: u32) {
                #cr::trace::on_task_exec_end(executor_id, task_id);
            }
            #[unsafe(no_mangle)]
            fn _embassy_trace_task_ready_begin(_executor_id: u32, _task_id: u32) {}
            #[unsafe(no_mangle)]
            fn _embassy_trace_executor_idle(executor_id: u32) {
                #cr::trace::on_executor_idle(executor_id);
            }
        }
    } else {
        quote!()
    };

    let graph_ident = graph
        .name
        .clone()
        .unwrap_or_else(|| Ident::new("GRAPH", proc_macro2::Span::call_site()));
    let nodes_ident = helpers.nodes.clone();
    let deps_ident = match &graph.name {
        Some(n) => format_ident!("__SV_DEPS_{}", n),
        None => Ident::new("DEPS", proc_macro2::Span::call_site()),
    };
    let graph_ref_ident = helpers.graph_ref.clone();
    let (graph_ref_def, graph_ref_field) = if cfg!(feature = "graph-ref") {
        (
            quote!( static #graph_ref_ident: #cr::GraphRef = #cr::GraphRef::new(&#nodes_ident); ),
            quote!( graph_ref: &#graph_ref_ident, ),
        )
    } else {
        (quote!(), quote!())
    };
    let topo_alias = format_ident!("{}_TOPOLOGY", graph_ident);
    let (topo_ty, topo_val, deps_def) = if flat {
        (
            quote!( #cr::Flat<#shape_lit> ),
            quote!( #cr::Flat::new() ),
            quote!(),
        )
    } else {
        (
            quote!( #cr::Ordered<#m, #shape_lit> ),
            quote!( #cr::Ordered::new(&#deps_ident) ),
            quote!( const #deps_ident: [&'static [u8]; #m] = [ #(#deps_entries),* ]; ),
        )
    };
    Ok(quote! {
        #(#defs)*

        // Private backing tables — the application uses the graph static. The
        // topology (dep table + order, or `Flat`) and pools are inlined into
        // its literal below; the node count is `.nodes.len()`.
        static #nodes_ident: [::core::option::Option<&'static #cr::TaskNode>; #m] = [ #(#all_entries),* ];
        #deps_def
        #graph_ref_def

        #[allow(non_camel_case_types)]
        pub type #topo_alias = #topo_ty;

        pub static #graph_ident: #cr::Graph<#m, #topo_alias> = #cr::Graph {
            nodes: &#nodes_ident,
            topo: #topo_val,
            #pools_field
            #graph_ref_field
        };

        #trace_hooks
    })
}

#[proc_macro_attribute]
pub fn dataflow(args: TokenStream, item: TokenStream) -> TokenStream {
    dataflow_expand(args.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[proc_macro_attribute]
pub fn dataflow_bundle(args: TokenStream, item: TokenStream) -> TokenStream {
    bundle_expand(args.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn bundle_expand(args: TokenStream2, item: TokenStream2) -> SynResult<TokenStream2> {
    use quote::ToTokens;

    let mut m: syn::ItemMod = syn::parse2(item)?;
    if !cfg!(feature = "dataflow") {
        return Err(syn::Error::new_spanned(
            &m.ident,
            "`#[dataflow_bundle]` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`), like the `#[dataflow]` \
             fns it bundles",
        ));
    }
    let name: Ident = if args.is_empty() {
        format_ident!("BUNDLE")
    } else {
        syn::parse2(args)?
    };
    let Some((_, items)) = &m.content else {
        return Err(syn::Error::new_spanned(
            &m.ident,
            "`#[dataflow_bundle]` needs an inline module (`mod x { .. }`) — \
             the member fns' bodies are its input, and a `mod x;` declaration \
             does not carry them",
        ));
    };

    let cr = quote!(::embassy_supervisor);
    let mut reads: Vec<DerivedEntry> = Vec::new();
    let mut writes: Vec<DerivedEntry> = Vec::new();
    let mut members = 0usize;
    for it in items {
        let syn::Item::Fn(f) = it else { continue };
        let Some(attr) = f
            .attrs
            .iter()
            .find(|a| embassy_supervisor_syntax::is_dataflow_attr(a))
        else {
            continue;
        };
        members += 1;
        let verbs: VerbTable = match &attr.meta {
            syn::Meta::Path(_) => VerbTable::builtin(),
            syn::Meta::List(l) => syn::parse2(l.tokens.clone())?,
            syn::Meta::NameValue(_) => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[dataflow]` takes verb registrations, not a value",
                ));
            }
        };
        let Some(param) = node_param(&f.sig) else {
            return Err(syn::Error::new_spanned(
                &f.sig.ident,
                "`#[dataflow]` needs a `&'static TaskNode` parameter — the \
                 verbs it derives from are called through it",
            ));
        };
        let fn_cfgs: Vec<TokenStream2> = f
            .attrs
            .iter()
            .filter_map(|a| match &a.meta {
                syn::Meta::List(l) if l.path.is_ident("cfg") => Some(l.tokens.clone()),
                _ => None,
            })
            .collect();
        rewrite_verb_calls(
            f.block.to_token_stream(),
            &param.to_string(),
            &verbs,
            &mut |call| {
                let mut cfgs = fn_cfgs.clone();
                cfgs.extend(call.cfgs.iter().cloned());
                record_derived(
                    if call.write { &mut writes } else { &mut reads },
                    &call,
                    cfgs,
                );
                Ok(None)
            },
        )?;
    }
    if members == 0 {
        return Err(syn::Error::new_spanned(
            &m.ident,
            "`#[dataflow_bundle]` found no `#[dataflow]` fn at the module's \
             top level — nothing to bundle",
        ));
    }

    let reads_ident = format_ident!("__SV_DATAFLOW_READS_{}", name);
    let writes_ident = format_ident!("__SV_DATAFLOW_WRITES_{}", name);
    let read_entries: Vec<TokenStream2> =
        reads.iter().map(|e| derived_entry_tokens(&cr, e)).collect();
    let write_entries: Vec<TokenStream2> = writes
        .iter()
        .map(|e| derived_entry_tokens(&cr, e))
        .collect();
    let nr = derived_prefix_expr(&reads);
    let nw = derived_prefix_expr(&writes);
    let statics = quote! {
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub static #reads_ident: [#cr::Coupling; #nr] = [#(#read_entries),*];
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub static #writes_ident: [#cr::Coupling; #nw] = [#(#write_entries),*];
    };
    m.content
        .as_mut()
        .expect("checked inline above")
        .1
        .push(syn::Item::Verbatim(statics));
    Ok(m.to_token_stream())
}

fn derived_predicate(alts: &[Vec<TokenStream2>]) -> Option<TokenStream2> {
    if alts.iter().any(|a| a.is_empty()) {
        return None;
    }
    let one = |alt: &Vec<TokenStream2>| -> TokenStream2 {
        match alt.as_slice() {
            [p] => p.clone(),
            many => quote!(all(#(#many),*)),
        }
    };
    match alts {
        [] => None,
        [alt] => Some(one(alt)),
        many => {
            let terms: Vec<TokenStream2> = many.iter().map(one).collect();
            Some(quote!(any(#(#terms),*)))
        }
    }
}

fn derived_prefix_expr<E: DerivedAlts>(entries: &[E]) -> TokenStream2 {
    let attrs: Vec<Vec<Attribute>> = entries
        .iter()
        .map(|e| match derived_predicate(e.alts()) {
            None => Vec::new(),
            Some(pred) => vec![syn::parse_quote!(#[cfg(#pred)])],
        })
        .collect();
    cfg_aware_len(attrs.iter())
}

trait DerivedAlts {
    fn alts(&self) -> &[Vec<TokenStream2>];
}

struct DerivedEntry {
    path: String,
    target: syn::Expr,
    alts: Vec<Vec<TokenStream2>>,
}
impl DerivedAlts for DerivedEntry {
    fn alts(&self) -> &[Vec<TokenStream2>] {
        &self.alts
    }
}

fn record_derived(
    list: &mut Vec<DerivedEntry>,
    call: &embassy_supervisor_syntax::VerbCall,
    cfgs: Vec<TokenStream2>,
) -> usize {
    match list.iter().position(|e| e.path == call.path) {
        Some(k) => {
            let text = |a: &[TokenStream2]| {
                a.iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            if !list[k].alts.iter().any(|a| text(a) == text(&cfgs)) {
                list[k].alts.push(cfgs);
            }
            k
        }
        None => {
            list.push(DerivedEntry {
                path: call.path.clone(),
                target: call.target.clone(),
                alts: vec![cfgs],
            });
            list.len() - 1
        }
    }
}

fn derived_entry_tokens(cr: &TokenStream2, e: &DerivedEntry) -> TokenStream2 {
    let (path, target) = (&e.path, &e.target);
    let plain = quote!( #cr::Coupling::new(#path, & #target) );
    match derived_predicate(&e.alts) {
        None => plain,
        Some(pred) => quote!( #[cfg(#pred)] #plain ),
    }
}

fn dataflow_expand(args: TokenStream2, item: TokenStream2) -> SynResult<TokenStream2> {
    use quote::ToTokens;

    let mut f: syn::ItemFn = syn::parse2(item)?;
    if !cfg!(feature = "dataflow") {
        return Err(syn::Error::new_spanned(
            &f.sig.ident,
            "`#[dataflow]` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`) — it derives this \
             fn's coupling tables from the supervisor verbs it calls",
        ));
    }
    let verbs: VerbTable = syn::parse2(args)?;
    let Some(param) = node_param(&f.sig) else {
        return Err(syn::Error::new_spanned(
            &f.sig.ident,
            "`#[dataflow]` needs a `&'static TaskNode` parameter — the verbs \
             it derives from are called through it",
        ));
    };
    let cr = quote!(::embassy_supervisor);
    let reads_ident = format_ident!("__SV_DATAFLOW_READS_{}", f.sig.ident);
    let writes_ident = format_ident!("__SV_DATAFLOW_WRITES_{}", f.sig.ident);

    let mut reads: Vec<DerivedEntry> = Vec::new();
    let mut writes: Vec<DerivedEntry> = Vec::new();
    rewrite_verb_calls(
        f.block.to_token_stream(),
        &param.to_string(),
        &verbs,
        &mut |call| {
            if !cfg!(feature = "liveness")
                && matches!(call.verb.as_str(), "beat_put" | "beat_writer")
                && call.cfgs.is_empty()
            {
                return Err(syn::Error::new_spanned(
                    &call.target,
                    format!(
                        "`{}` carries the node's sign of life, which requires \
                         the `liveness` feature (embassy-supervisor feature \
                         `liveness`). Without it this would be a heartbeat the \
                         build silently does not make: use `{}` for the write \
                         alone, or enable the feature",
                        call.verb,
                        call.verb.trim_start_matches("beat_"),
                    ),
                ));
            }
            // The verbs a feature adds to `TaskNode`: name the feature here
            // rather than leave rustc to report a missing method.
            let needs = match call.verb.as_str() {
                "open" | "lease" if !cfg!(feature = "data-deps") => Some("`data-deps` feature"),
                "veto" if !cfg!(feature = "veto") => Some("`veto` feature"),
                "retire" if !cfg!(all(feature = "data-deps", feature = "readiness")) => {
                    Some("`data-deps` and `readiness` features")
                }
                _ => None,
            };
            if let Some(needs) = needs
                && call.cfgs.is_empty()
            {
                return Err(syn::Error::new_spanned(
                    &call.target,
                    format!(
                        "`{}` requires the {needs} (embassy-supervisor features of \
                         the same names), which add the verb to `TaskNode`",
                        call.verb,
                    ),
                ));
            }
            record_derived(
                if call.write { &mut writes } else { &mut reads },
                &call,
                call.cfgs.clone(),
            );
            Ok(None)
        },
    )?;
    let body = rewrite_verb_calls(
        f.block.to_token_stream(),
        &param.to_string(),
        &verbs,
        &mut |call| {
            let (list, table) = if call.write {
                (&writes, &writes_ident)
            } else {
                (&reads, &reads_ident)
            };
            let k = list
                .iter()
                .position(|e| e.path == call.path)
                .expect("collected in the first pass over this same body");
            let prefix = derived_prefix_expr(&list[..k]);
            let target = &call.target;
            Ok(Some(
                quote!( #cr::Sig { entry: &#table[#prefix], target: & #target } ),
            ))
        },
    )?;
    f.block = syn::parse2(body)?;

    let read_entries: Vec<TokenStream2> =
        reads.iter().map(|e| derived_entry_tokens(&cr, e)).collect();
    let write_entries: Vec<TokenStream2> = writes
        .iter()
        .map(|e| derived_entry_tokens(&cr, e))
        .collect();
    let nr = derived_prefix_expr(&reads);
    let nw = derived_prefix_expr(&writes);
    Ok(quote! {
        #f

        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub static #reads_ident: [#cr::Coupling; #nr] = [#(#read_entries),*];
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        pub static #writes_ident: [#cr::Coupling; #nw] = [#(#write_entries),*];
    })
}

#[proc_macro]
pub fn supervisor_graph(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as GraphSpec);
    expand(graph)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[proc_macro]
pub fn supervisor_fragment(input: TokenStream) -> TokenStream {
    fragment_expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn fragment_expand(input: TokenStream2) -> SynResult<TokenStream2> {
    struct FragmentSpec {
        name: Ident,
        items: TokenStream2,
    }
    impl Parse for FragmentSpec {
        fn parse(input: ParseStream) -> SynResult<Self> {
            input.parse::<kw::name>()?;
            input.parse::<Token![:]>()?;
            let name: Ident = input.parse()?;
            input.parse::<Token![;]>()?;
            let items: TokenStream2 = input.parse()?;
            Ok(FragmentSpec { name, items })
        }
    }
    let spec: FragmentSpec = syn::parse2(input)?;
    let name = &spec.name;

    validate_dollars(spec.items.clone())?;
    let items = normalize_fragment_crate(spec.items);

    let substituted = substitute_dollar_crate(items.clone(), &quote!(__sv_fragment_crate));
    gate::gate(&syn::parse2::<GraphSpec>(substituted)?)?;

    let items = &items;
    let dollar = proc_macro2::Punct::new('$', proc_macro2::Spacing::Alone);
    let doc = format!(
        "A `supervisor_fragment!` relay (generated). Use from a compose site:\n\
         `embassy_supervisor::compose_graph! {{ fragments: [{name}], graph: {{ .. }} }}`\n\
         Not for direct invocation."
    );
    Ok(quote! {
        #[doc = #doc]
        #[macro_export]
        macro_rules! #name {
            (@emit #dollar cb:path, [#dollar(#dollar rest:tt)*], {#dollar(#dollar acc:tt)*}, {#dollar(#dollar g:tt)*}) => {
                #dollar cb! { @next [#dollar(#dollar rest)*],
                    {#dollar(#dollar acc)* @fragment #name; #items @endfragment;},
                    {#dollar(#dollar g)*} }
            };
        }
    })
}

fn validate_dollars(stream: TokenStream2) -> SynResult<()> {
    use proc_macro2::TokenTree;
    let mut iter = stream.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Group(g) => validate_dollars(g.stream())?,
            TokenTree::Punct(p) if p.as_char() == '$' => match iter.peek() {
                Some(TokenTree::Ident(i)) if i == "crate" => {}
                _ => {
                    return Err(syn::Error::new(
                        p.span(),
                        "only `$crate` is permitted in a fragment — any other `$` \
                         would be read as a metavariable by the relay macro",
                    ));
                }
            },
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_gated(src: &str) -> SynResult<GraphSpec> {
        let spec = syn::parse_str::<GraphSpec>(src)?;
        gate::gate(&spec)?;
        Ok(spec)
    }

    #[test]
    fn ready_marker_requires_feature() {
        let res = parse_gated(
            "node NET = Terminate, deps: [];\n\
             node HTTP = Terminate, deps: [NET ready];",
        );
        if cfg!(feature = "readiness") {
            assert!(res.is_ok(), "marker accepted with the feature");
        } else {
            match res {
                Ok(_) => panic!("marker accepted without the feature"),
                Err(err) => assert!(
                    err.to_string().contains("requires the `readiness` feature"),
                    "unexpected error: {err}"
                ),
            }
        }
    }

    #[test]
    fn beat_timeout_requires_feature() {
        let res = parse_gated("node A = Terminate, deps: [], beat_timeout: 100;");
        if cfg!(feature = "liveness-monitor") {
            assert!(res.is_ok(), "clause accepted with the feature");
        } else {
            match res {
                Ok(_) => panic!("clause accepted without the feature"),
                Err(err) => assert!(
                    err.to_string()
                        .contains("requires the `liveness-monitor` feature"),
                    "unexpected error: {err}"
                ),
            }
        }
    }

    #[test]
    fn cfg_gated_clauses_defer_to_rustc() {
        for (src, feature_on, builder) in [
            (
                "node A = Terminate, deps: [], \
                 #[cfg(feature = \"x\")] beat_timeout: 100, \
                 #[cfg(feature = \"x\")] beat_window: 3;",
                cfg!(feature = "liveness-monitor"),
                "with_beat_timeout",
            ),
            (
                "node A = Terminate, deps: [], task: w, \
                 writes: [crate::S observed beat via it.get()], \
                 #[cfg(feature = \"x\")] beat_timeout: 100, \
                 #[cfg(feature = \"x\")] ready_on_write;",
                cfg!(all(
                    feature = "liveness-monitor",
                    feature = "coupling-observe",
                    feature = "readiness"
                )),
                "with_ready_on_write",
            ),
            (
                "node A = Terminate, deps: [], task: w, #[cfg(feature = \"x\")] discover;",
                cfg!(feature = "dataflow"),
                "__SV_DATAFLOW_",
            ),
        ] {
            let spec = parse_gated(src).expect("a gated clause passes the gate");
            let out = expand(spec).expect("expansion succeeds").to_string();
            if feature_on {
                assert!(out.contains(builder), "{src}\n{out}");
                assert!(!out.contains("compile_error"), "{src}\n{out}");
            } else {
                assert!(out.contains("compile_error"), "{src}\n{out}");
            }
        }
    }

    #[test]
    fn cfg_gated_featureless_clauses_emit_both_ways() {
        let spec = parse_gated(
            "node A = Terminate, deps: [], \
             #[cfg(feature = \"x\")] slot_timeout: 100, \
             #[cfg(feature = \"x\")] ack_timeout: 200, \
             #[cfg(feature = \"x\")] disabled;\n\
             node B = Terminate, deps: [], slot_timeout: 300;",
        )
        .expect("gated featureless clauses pass the gate");
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(out.contains("with_slot_timeout"), "{out}");
        assert!(out.contains("with_ack_timeout"), "{out}");
        // The gated `disabled` becomes a cfg-block bool, not a bare literal.
        assert!(out.contains("# [cfg (feature = \"x\")] { true }"), "{out}");
        assert!(!out.contains("compile_error"), "{out}");

        // Pool timeouts ride the same emitter.
        if cfg!(feature = "pool") {
            let spec = parse_gated(
                "pool P = [Terminate], deps: [], task: w, \
                 policy: embassy_supervisor::DeferredShrink::new(d()), \
                 min: 1, max: 1, #[cfg(feature = \"x\")] slot_timeout: 100;",
            )
            .expect("gated pool timeout passes the gate");
            let out = expand(spec).expect("expansion succeeds").to_string();
            assert!(out.contains("with_slot_timeout"), "{out}");
            assert!(!out.contains("compile_error"), "{out}");
        }
    }

    #[test]
    fn beat_verb_requires_feature() {
        let expand = |verb: &str| {
            dataflow_expand(
                quote!(),
                format!("fn f(node: &'static TaskNode) {{ node.{verb}(&crate::OUT, 1); }}")
                    .parse()
                    .unwrap(),
            )
        };
        for verb in ["beat_put", "beat_writer"] {
            let res = expand(verb);
            if !cfg!(feature = "dataflow") {
                continue;
            }
            if cfg!(feature = "liveness") {
                assert!(res.is_ok(), "`{verb}` accepted with the feature");
            } else {
                match res {
                    Ok(_) => panic!("`{verb}` accepted without the feature"),
                    Err(err) => {
                        let msg = err.to_string();
                        assert!(
                            msg.contains("requires the `liveness` feature"),
                            "`{verb}`: {msg}"
                        );
                        assert!(
                            msg.contains(verb.trim_start_matches("beat_")),
                            "the message names the plain verb to fall back to: {msg}"
                        );
                    }
                }
            }
        }
        if cfg!(feature = "dataflow") {
            assert!(expand("put").is_ok(), "`put` needs no liveness");
            let gated = dataflow_expand(
                quote!(),
                "fn f(node: &'static TaskNode) { \
                 #[cfg(feature = \"x\")] node.beat_put(&crate::OUT, 1); }"
                    .parse()
                    .unwrap(),
            );
            assert!(gated.is_ok(), "a cfg-gated beat verb defers to rustc");
        }
    }

    #[test]
    fn coupling_clauses_require_feature() {
        let res =
            parse_gated("node A = Terminate, deps: [], reads: [crate::SIG], writes: [crate::OUT];");
        if cfg!(feature = "coupling") {
            assert!(res.is_ok(), "clauses accepted with the feature");
        } else {
            match res {
                Ok(_) => panic!("clauses accepted without the feature"),
                Err(err) => assert!(
                    err.to_string().contains("requires the `coupling` feature"),
                    "unexpected error: {err}"
                ),
            }
        }
    }

    #[test]
    fn bound_marker_requires_feature() {
        let res = parse_gated(
            "node A = Terminate, deps: [];\nnode B = Terminate, deps: [A ready bound];",
        );
        if cfg!(all(feature = "bound-deps", feature = "readiness")) {
            assert!(res.is_ok(), "marker accepted with the features");
        } else if !cfg!(feature = "bound-deps") {
            match res {
                Ok(_) => panic!("marker accepted without the feature"),
                Err(err) => assert!(
                    err.to_string()
                        .contains("requires the `bound-deps` feature"),
                    "unexpected error: {err}"
                ),
            }
        }
    }

    fn gate_rejects(src: &str, feature: bool, needle: &str) {
        let res = parse_gated(src);
        if feature {
            assert!(res.is_ok(), "rejected with the feature: {:?}", res.err());
        } else {
            match res {
                Ok(_) => panic!("accepted without the feature"),
                Err(err) => assert!(err.to_string().contains(needle), "unexpected error: {err}"),
            }
        }
    }

    #[test]
    fn observed_marker_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, writes: [crate::S observed via it.get()];",
            cfg!(all(feature = "coupling-observe", feature = "coupling")),
            "requires the `coupling-observe` feature",
        );
    }

    #[test]
    fn discover_clause_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, discover;",
            cfg!(feature = "dataflow"),
            "requires the `dataflow` feature",
        );
    }

    #[test]
    fn observe_default_requires_feature() {
        gate_rejects(
            "observe writes: it.get();\nnode A = Terminate, deps: [], task: f;",
            cfg!(feature = "coupling-observe"),
            "requires the `coupling-observe` feature",
        );
    }

    #[test]
    fn local_resource_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, resources: [R: local Thing];",
            cfg!(feature = "local-resources"),
            "`local-resources` feature",
        );
    }

    #[test]
    fn state_clause_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, state: Buf = Buf::new();",
            cfg!(feature = "heap-state"),
            "requires the `heap-state` feature",
        );
    }

    #[test]
    fn zeroed_state_clause_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, state: zeroed Buf;",
            cfg!(feature = "heap-state"),
            "requires the `heap-state` feature",
        );
    }

    #[test]
    #[cfg(feature = "heap-state")]
    fn zeroed_state_parses_as_marker() {
        let spec = parse_gated("node A = Terminate, deps: [], task: f, state: zeroed Buf;")
            .expect("marker form");
        let Item::Node(n) = &spec.items[0] else {
            panic!("node")
        };
        let Some((_, ty, StateInit::Zeroed(_))) = &n.state else {
            panic!("zeroed state")
        };
        assert_eq!(quote!(#ty).to_string(), "Buf");
        let spec = parse_gated("node A = Terminate, deps: [], task: f, state: zeroed = Z;")
            .expect("type named zeroed");
        let Item::Node(n) = &spec.items[0] else {
            panic!("node")
        };
        let Some((_, ty, StateInit::Expr(_))) = &n.state else {
            panic!("init state")
        };
        assert_eq!(quote!(#ty).to_string(), "zeroed");
    }

    #[test]
    fn deps_clause_is_optional() {
        assert!(parse_gated("node A = Terminate, task: f;").is_ok());
        assert!(parse_gated("node A = Terminate, task: f, deps: [];").is_ok());
        match parse_gated("node A = Terminate, deps: [], task: f, deps: [];") {
            Ok(_) => panic!("duplicate deps accepted"),
            Err(e) => assert!(e.to_string().contains("duplicate `deps:`"), "{e}"),
        }
    }

    #[test]
    fn provides_must_name_a_declared_slot() {
        let spec = syn::parse_str::<GraphSpec>(
            "node P = Terminate, deps: [], task: f, provides: [NOPE];\n\
             node C = Terminate, deps: [P], task: g, resources: [SLOT: shared u32];",
        )
        .unwrap();
        match expand(spec) {
            Ok(_) => panic!("unknown slot accepted"),
            Err(e) => assert!(
                e.to_string().contains("no `resources:` entry"),
                "unexpected error: {e}"
            ),
        }
    }

    #[test]
    fn beat_window_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, beat_timeout: 10, beat_window: 3;",
            cfg!(feature = "liveness-monitor"),
            "requires the `liveness-monitor` feature",
        );
    }

    #[test]
    fn ready_on_write_requires_both_features() {
        let src = "node A = Terminate, deps: [], task: f, beat_timeout: 10, \
                   ready_on_write, writes: [crate::S observed beat via it.get()];";
        let all = cfg!(all(
            feature = "coupling-observe",
            feature = "readiness",
            feature = "coupling",
            feature = "liveness-monitor"
        ));
        let needle = if cfg!(feature = "coupling") && !cfg!(feature = "coupling-observe") {
            "requires the `coupling-observe` feature"
        } else if !cfg!(feature = "coupling") {
            "requires the `coupling` feature"
        } else if !cfg!(feature = "liveness-monitor") {
            "requires the `liveness-monitor` feature"
        } else {
            "requires the `readiness` feature"
        };
        gate_rejects(src, all, needle);
    }

    #[cfg(feature = "dataflow")]
    #[test]
    fn dataflow_indices_bake_after_full_collection() {
        let item = quote::quote! {
            async fn f(node: &'static TaskNode) {
                #[cfg(feature = "x")]
                node.put(&crate::A, 1u8);
                node.put(&crate::B, 2u8);
                node.put(&crate::A, 3u8);
            }
        };
        let out = dataflow_expand(TokenStream2::new(), item)
            .expect("expands")
            .to_string();
        // The third call makes `A` unconditional, so the only `feature = "x"`
        // left is the statement's own attribute; a second occurrence is a call
        assert_eq!(
            out.matches("feature = \"x\"").count(),
            1,
            "stale predicate survives: {out}"
        );
    }

    #[test]
    fn divisible_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, resources: [R: divisible];",
            cfg!(feature = "budget"),
            "`budget` feature",
        );
    }

    #[cfg(all(feature = "budget", feature = "pool"))]
    #[test]
    fn divisible_emits_one_budget_sized_by_its_holders() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, resources: [P: divisible];\n\
             pool W = [Terminate, Terminate, Terminate], deps: [], task: g, \
             resources: [P: divisible], policy: DeferredShrink::new(d), min: 1, max: 3;\n\
             node B = Terminate, deps: [], task: f, resources: [P: divisible];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(out.contains("pub static P : "), "{out}");
        assert!(
            out.contains("Budget < 5usize >"),
            "one slot for A, three for W, one for B: {out}"
        );
        assert!(out.contains("P . claimant (0u8)"), "A takes slot 0: {out}");
        assert!(
            out.contains("P . claimant ((1u8 as usize + I) as u8)"),
            "W's members take 1..=3: {out}"
        );
        assert!(out.contains("P . claimant (4u8)"), "B takes slot 4: {out}");
        assert!(
            out.contains("__SV_CLAIMS_W_2"),
            "one claims table per member: {out}"
        );
        assert!(
            out.contains("(& P , 3u8)"),
            "member 2 releases slot 3: {out}"
        );
        assert_eq!(out.matches(". with_claims (").count(), 5, "{out}");
        assert!(!out.contains(". restore ("), "nothing to restore: {out}");
    }

    #[cfg(feature = "budget")]
    #[test]
    fn divisible_slots_are_counted_syntactically() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, resources: [#[cfg(any())] P: divisible];\n\
             node B = Terminate, deps: [], task: f, resources: [P: divisible];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(
            out.contains("Budget < 2usize >"),
            "a cfg'd-out holder still takes its slot: {out}"
        );
    }

    #[cfg(feature = "budget")]
    #[test]
    fn a_budget_may_be_provided_by_a_node() {
        let spec = syn::parse_str::<GraphSpec>(
            "node ALLOC = Terminate, deps: [], task: f, provides: [P];\n\
             node A = Terminate, deps: [ALLOC], task: g, resources: [P: divisible];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(out.contains("__SV_PROVIDES_ALLOC"), "{out}");
    }

    #[cfg(feature = "budget")]
    #[test]
    fn a_budget_name_cannot_double_as_a_take_kind_slot() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, resources: [P: divisible];\n\
             node B = Terminate, deps: [], task: f, resources: [P: u32];",
        )
        .unwrap();
        match expand(spec) {
            Ok(_) => panic!("accepted"),
            Err(e) => assert!(e.to_string().contains("duplicate resource name"), "{e}"),
        }
    }

    #[test]
    fn veto_requires_feature() {
        gate_rejects(
            "node A = Terminate, deps: [], task: f, writes: [crate::TRIP veto];",
            cfg!(feature = "veto"),
            "`veto` feature",
        );
    }

    #[cfg(all(feature = "veto", feature = "pool"))]
    #[test]
    fn veto_writers_are_numbered_in_item_order_across_nodes_and_pools() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, writes: [crate::TRIP veto];\n\
             pool P = [Terminate, Terminate], deps: [], task: g, writes: [crate::TRIP veto], \
             policy: DeferredShrink::new(d), min: 1, max: 2;\n\
             node B = Terminate, deps: [], task: f, writes: [crate::TRIP veto observed beat];\n\
             node R = Terminate, deps: [], task: h, reads: [crate::TRIP];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(out.contains(". veto (0u8)"), "A: {out}");
        assert!(
            out.contains("__SV_WRITES_P_0") && out.contains("__SV_WRITES_P_1"),
            "a table per member: {out}"
        );
        assert!(
            out.contains(". veto (1u8)") && out.contains(". veto (2u8)"),
            "P's members: {out}"
        );
        assert!(
            out.contains(". beat () . veto (3u8)"),
            "B, beside its other markers: {out}"
        );
        assert!(
            out.contains("__sv_check_veto (& crate :: TRIP , 4usize)"),
            "one check per gate: {out}"
        );
        assert_eq!(out.matches("__sv_check_veto").count(), 1, "{out}");
    }

    #[cfg(feature = "veto")]
    #[test]
    fn a_pool_without_veto_keeps_one_writes_table() {
        let spec = syn::parse_str::<GraphSpec>(
            "pool P = [Terminate, Terminate], deps: [], task: g, writes: [crate::OUT], \
             policy: DeferredShrink::new(d), min: 1, max: 2;",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(out.contains("__SV_WRITES_P :"), "{out}");
        assert!(!out.contains("__SV_WRITES_P_0"), "{out}");
    }

    #[cfg(feature = "veto")]
    #[test]
    fn more_than_32_veto_writers_are_rejected() {
        let mut src = String::new();
        for i in 0..33 {
            src.push_str(&format!(
                "node N{i} = Terminate, deps: [], task: f, writes: [crate::TRIP veto];\n"
            ));
        }
        let spec = syn::parse_str::<GraphSpec>(&src).unwrap();
        match expand(spec) {
            Ok(_) => panic!("accepted"),
            Err(e) => assert!(e.to_string().contains("more than 32"), "{e}"),
        }
    }

    #[cfg(feature = "budget")]
    #[test]
    fn pool_size_cannot_share_a_claimant_slot() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, pool_size: 2, resources: [P: divisible];",
        )
        .unwrap();
        match expand(spec) {
            Ok(_) => panic!("accepted"),
            Err(e) => assert!(e.to_string().contains("lend/consume/divisible"), "{e}"),
        }
    }

    #[cfg(feature = "veto")]
    #[test]
    fn a_veto_gate_spelled_two_ways_is_rejected() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, writes: [crate::TRIP veto];\n\
             node B = Terminate, deps: [], task: f, writes: [TRIP veto];",
        )
        .unwrap();
        match expand(spec) {
            Ok(_) => panic!("accepted"),
            Err(e) => {
                let e = e.to_string();
                assert!(e.contains("`crate::TRIP` and `TRIP`"), "{e}");
                assert!(e.contains("numbered per spelling"), "{e}");
            }
        }
        // Different statics that happen to share an ident are two gates.
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, writes: [crate::a::TRIP veto];\n\
             node B = Terminate, deps: [], task: f, writes: [crate::b::TRIP veto];",
        )
        .unwrap();
        match expand(spec) {
            Ok(_) => panic!("accepted"),
            Err(e) => assert!(e.to_string().contains("`use .. as`"), "{e}"),
        }
        // An indexed gate is keyed by its index too.
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, writes: [crate::TRIP[0] veto];\n\
             node B = Terminate, deps: [], task: f, writes: [crate::TRIP[1] veto];",
        )
        .unwrap();
        let out = expand(spec).expect("two elements, two gates").to_string();
        assert_eq!(out.matches("__sv_check_veto").count(), 2, "{out}");
        assert!(
            out.contains(". veto (0u8)") && !out.contains(". veto (1u8)"),
            "{out}"
        );
    }

    #[cfg(feature = "veto")]
    #[test]
    fn the_veto_check_is_gated_like_its_writers() {
        let spec = syn::parse_str::<GraphSpec>(
            "#[cfg(feature = \"x\")] node A = Terminate, deps: [], task: f, \
             writes: [crate::TRIP veto];\n\
             node B = Terminate, deps: [], task: f, \
             writes: [#[cfg(feature = \"y\")] crate::TRIP veto];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(
            out.contains("# [cfg (any (feature = \"x\" , feature = \"y\"))] const _ : () = "),
            "the check carries the union of its writers' cfgs: {out}"
        );
        let spec = syn::parse_str::<GraphSpec>(
            "#[cfg(feature = \"x\")] node A = Terminate, deps: [], task: f, \
             writes: [crate::TRIP veto];\n\
             node B = Terminate, deps: [], task: f, writes: [crate::TRIP veto];",
        )
        .unwrap();
        let out = expand(spec).expect("expansion succeeds").to_string();
        assert!(
            out.contains("const _ : () = ") && !out.contains("))] const _ : () = "),
            "one unconditional writer keeps the check bare: {out}"
        );
    }

    #[cfg(feature = "dataflow")]
    #[test]
    fn feature_verbs_name_their_feature_in_dataflow_bodies() {
        for (body, feature, needle) in [
            (
                quote::quote! { let _ = node.open(&crate::EST).await; },
                cfg!(feature = "data-deps"),
                "`data-deps` feature",
            ),
            (
                quote::quote! { let _ = node.lease(&crate::LNK); },
                cfg!(feature = "data-deps"),
                "`data-deps` feature",
            ),
            (
                quote::quote! { node.veto(&crate::TRIP); },
                cfg!(feature = "veto"),
                "`veto` feature",
            ),
            (
                quote::quote! { node.retire(&crate::EST, d).await; },
                cfg!(all(feature = "data-deps", feature = "readiness")),
                "`data-deps` and `readiness` features",
            ),
        ] {
            let item = quote::quote! {
                async fn f(node: &'static TaskNode) { #body }
            };
            let res = dataflow_expand(TokenStream2::new(), item);
            if feature {
                assert!(res.is_ok(), "rejected with the feature: {:?}", res.err());
            } else {
                match res {
                    Ok(_) => panic!("accepted without the feature"),
                    Err(e) => assert!(e.to_string().contains(needle), "{e}"),
                }
            }
        }
    }

    #[test]
    fn a_serialized_slot_holds_its_holders_to_one_executor() {
        let ok = syn::parse_str::<GraphSpec>(
            "executor HIGH;\n\
             node A = Terminate, deps: [], executor: HIGH, task: f, \
             resources: [BUS: shared serialized Bus];\n\
             node B = Terminate, deps: [], executor: HIGH, task: f, \
             resources: [BUS: shared serialized Bus];",
        )
        .unwrap();
        assert!(expand(ok).is_ok(), "one tier: accepted");
        let root = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];\n\
             node B = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];",
        )
        .unwrap();
        assert!(
            expand(root).is_ok(),
            "both on the supervisor's executor: accepted"
        );
        for (src, a, b) in [
            (
                "executor HIGH;\n\
                 node A = Terminate, deps: [], task: f, resources: [BUS: shared serialized Bus];\n\
                 node B = Terminate, deps: [], executor: HIGH, task: f, \
                 resources: [BUS: shared serialized Bus];",
                "`A` runs on the supervisor's executor",
                "`B` on `HIGH`",
            ),
            (
                "executor HIGH; executor LOW;\n\
                 node A = Terminate, deps: [], executor: HIGH, task: f, \
                 resources: [BUS: shared serialized Bus];\n\
                 node B = Terminate, deps: [], executor: LOW, task: f, \
                 resources: [BUS: shared serialized Bus];",
                "`A` runs on `HIGH`",
                "`B` on `LOW`",
            ),
        ] {
            let spec = syn::parse_str::<GraphSpec>(src).unwrap();
            match expand(spec) {
                Ok(_) => panic!("accepted across tiers: {src}"),
                Err(e) => {
                    let msg = e.to_string();
                    assert!(msg.contains("priority ceiling"), "{msg}");
                    assert!(msg.contains(a) && msg.contains(b), "{msg}");
                }
            }
        }
        let plain = syn::parse_str::<GraphSpec>(
            "executor HIGH;\n\
             node A = Terminate, deps: [], task: f, resources: [BUS: shared Bus];\n\
             node B = Terminate, deps: [], executor: HIGH, task: f, resources: [BUS: shared Bus];",
        )
        .unwrap();
        assert!(
            expand(plain).is_ok(),
            "without the marker a shared slot may span tiers"
        );
    }
}

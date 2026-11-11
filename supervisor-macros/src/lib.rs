//! Proc-macros behind `embassy-supervisor`: use them through that crate's
//! re-exports.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod gate;

use embassy_supervisor_syntax::{
    AdoptedFn, Dep, DerivedEntry, GraphSpec, Item, NodeItem, PoolItem, ResourceDecl, ResourceKind,
    SignalDecl, StateInit, TaskSource, VerbTable, check_fragment_body, derived_predicate,
    item_executor, item_ident_cfg, item_resources, kw, name_string, node_param,
    normalize_fragment_crate, record_derived, rewrite_verb_calls, substitute_dollar_crate,
};
use gate::{ENABLED, Features};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Ident, LitInt, Meta, Path, Result as SynResult, Token};

const LOCAL_SLOT_TYPE: &str = "__SvLocalResourceSlot";
const LOCAL_GATE_TYPE: &str = "__SvLocalResourceGate";
/// The sentence a `local` slot static's generated rustdoc ends with.
const LOCAL_SLOT_DOC: &str = " `local`: touch it only from the executor its declarations \
     run on; `provide`, `take`, `restore` and `get` are `unsafe` for that reason.";

struct HelperIdents {
    local_slot: Ident,
    local_gate: Ident,
    try_box: Ident,
    try_box_zeroed: Ident,
    alloc_alias: Ident,
    nodes: Ident,
    graph_ref: Ident,
    self_handle: Ident,
    fence: Ident,
}

impl HelperIdents {
    fn new(graph_name: Option<&Ident>) -> Self {
        match graph_name {
            None => Self {
                local_slot: format_ident!("{LOCAL_SLOT_TYPE}"),
                local_gate: format_ident!("{LOCAL_GATE_TYPE}"),
                try_box: format_ident!("__sv_try_box"),
                try_box_zeroed: format_ident!("__sv_try_box_zeroed"),
                alloc_alias: format_ident!("__sv_alloc"),
                nodes: format_ident!("__SV_NODES"),
                graph_ref: format_ident!("__SV_GRAPH_REF"),
                self_handle: format_ident!("__SV_SELF_HANDLE"),
                fence: format_ident!("__sv_fence"),
            },
            Some(n) => {
                let lower = n.to_string().to_lowercase();
                Self {
                    local_slot: format_ident!("{LOCAL_SLOT_TYPE}{}", n),
                    local_gate: format_ident!("{LOCAL_GATE_TYPE}{}", n),
                    try_box: format_ident!("__sv_try_box_{lower}"),
                    try_box_zeroed: format_ident!("__sv_try_box_zeroed_{lower}"),
                    alloc_alias: format_ident!("__sv_alloc_{lower}"),
                    nodes: format_ident!("__SV_NODES_{}", n),
                    graph_ref: format_ident!("__SV_GRAPH_REF_{}", n),
                    self_handle: format_ident!("__SV_SELF_HANDLE_{}", n),
                    fence: format_ident!("__sv_fence_{lower}"),
                }
            }
        }
    }
}

/// `expr` with every `it` that names a value (a single-segment path
/// expression) replaced by `(&target)`. A field or method named `it`, a path
/// segment `it`, and the body of a closure that binds its own `it` are left
/// alone. A macro call's arguments are rewritten when they parse as
/// expressions, and left as written otherwise.
fn substitute_it(expr: &Expr, target: &TokenStream2) -> TokenStream2 {
    use syn::visit_mut::{self, VisitMut};

    fn binds_it(p: &syn::Pat) -> bool {
        match p {
            syn::Pat::Ident(i) => i.ident == "it",
            syn::Pat::Type(t) => binds_it(&t.pat),
            _ => false,
        }
    }
    struct It<'a>(&'a TokenStream2);
    impl VisitMut for It<'_> {
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            if let Expr::Path(p) = e
                && p.qself.is_none()
                && p.path.get_ident().is_some_and(|i| i == "it")
            {
                let (span, target) = (p.path.segments[0].ident.span(), self.0);
                *e = Expr::Verbatim(quote::quote_spanned!(span=> (&#target)));
                return;
            }
            visit_mut::visit_expr_mut(self, e);
        }
        fn visit_expr_closure_mut(&mut self, c: &mut syn::ExprClosure) {
            if !c.inputs.iter().any(binds_it) {
                visit_mut::visit_expr_closure_mut(self, c);
            }
        }
        fn visit_macro_mut(&mut self, m: &mut syn::Macro) {
            if let Ok(mut args) = m.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            {
                for a in &mut args {
                    self.visit_expr_mut(a);
                }
                m.tokens = quote!(#args);
            }
        }
    }
    let mut e = expr.clone();
    It(target).visit_expr_mut(&mut e);
    quote!(#e)
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
    tag: &'a str,
}

#[derive(Default)]
struct ObserveDefaults {
    writes: Option<Expr>,
    reads: Option<Expr>,
}

fn marker_array_tokens(
    ctx: &EmitCtx<'_>,
    deps: &[Dep],
    targets: &HashMap<String, DepTarget<'_>>,
    select: impl Fn(&Dep) -> bool,
    prefix: &str,
    builder: &str,
) -> (TokenStream2, TokenStream2) {
    let EmitCtx { cfg, cr, tag } = ctx;
    let Some((len, refs)) = marked_dep_tokens(deps, targets, select) else {
        return (quote!(), quote!());
    };
    let table = format_ident!("__SV_{}_{}", prefix, tag);
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
    let EmitCtx { cfg, cr, tag } = ctx;
    let mut defs = quote!();
    let mut defs_refs: Vec<TokenStream2> = Vec::new();
    let mut ref_cfgs: Vec<Vec<Attribute>> = Vec::new();
    if !decls.is_empty() {
        let table = format_ident!("__SV_{}_{}", prefix, tag);
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
                    // Spanned on the entry: a signal without `Observable` is
                    // reported where it is declared, not on the whole graph.
                    let accessor = d
                        .via
                        .as_ref()
                        .or(default_observe)
                        .map(|e| substitute_it(e, &target))
                        .unwrap_or_else(|| {
                            quote::quote_spanned!(d.path.span()=>
                                #cr::Observable::change_token(&#target)
                            )
                        });
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
    let tbls = format_ident!("__SV_TBLS_{}_{}", prefix, tag);
    let builder = format_ident!("{}", builder);
    defs.extend(quote! {
        #(#cfg)*
        static #tbls: [&'static [#cr::Coupling]; #k] = [#(#refs),*];
    });
    (defs, quote!( .#builder(&#tbls) ))
}

/// The const asserts that every marker-only entry beside `discover` names a
/// signal a bound `#[dataflow]` table carries. `owner` names the declaring
/// item for the message (``node `A` `` or ``pool `P` ``).
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
        // Each term is a statement gated by its adoption's `#[cfg]`, so a
        // compiled-out adoption adds nothing.
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
            "{owner}: `{clause}: [{name} ..]` marks a signal no bound \
             `#[dataflow]` table carries. Beside `discover` an entry may only \
             add a marker to a coupling the scan already found, so either the \
             task fn does not access this signal, or it reaches it under a \
             different final path segment"
        );
        // Only the assert carries the entry's span, so a failure points at
        // the marker to drop; the flag keeps the glue's hygiene.
        let found = Ident::new("__sv_found", proc_macro2::Span::call_site());
        let check = quote::quote_spanned!(d.path.span()=> ::core::assert!(#found, #msg););
        out.extend(quote! {
            #(#cfg)*
            #(#dcfg)*
            #(#entry_cfg)*
            const _: () = {
                let mut __sv_found = false;
                #(#tables)*
                #check
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
    /// The clause keyword's span, which a deferred `compile_error!` reports at.
    span: proc_macro2::Span,
}

fn builder_clause_tokens(
    item_cfg: &[Attribute],
    clauses: impl IntoIterator<Item = GatedBuilder>,
) -> (TokenStream2, TokenStream2, TokenStream2) {
    let (mut inline, mut stmts, mut errors) = (quote!(), quote!(), quote!());
    for c in clauses {
        let (clause_cfg, tokens) = (&c.cfg, &c.tokens);
        if !c.available {
            // The gate let it through, so the item or the clause is gated:
            // the error fires where both hold, and no builder call is emitted.
            let err = syn::LitStr::new(c.err, c.span);
            errors.extend(quote::quote_spanned!(c.span=>
                #(#item_cfg)* #(#clause_cfg)* ::core::compile_error!(#err);
            ));
        } else if clause_cfg.is_empty() {
            inline.extend(tokens.clone());
        } else {
            stmts.extend(quote!( #(#clause_cfg)* let __sv_cfg = __sv_cfg #tokens; ));
        }
    }
    (inline, stmts, errors)
}

/// The `disabled` marker as a `.with_disabled_at_boot(..)` call on the node's
/// config; nothing for a node that does not declare it.
fn disabled_tokens(
    disabled: Option<&embassy_supervisor_syntax::Gated<kw::disabled>>,
) -> TokenStream2 {
    let value = match disabled {
        None => return quote!(),
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
    };
    quote!( .with_disabled_at_boot(#value) )
}

/// The `slot_timeout:`/`ack_timeout:` builder calls, in that order: nodes and
/// pools both take them, and neither needs a feature.
fn timeout_clauses(
    slot: Option<&embassy_supervisor_syntax::Gated<kw::slot_timeout, LitInt>>,
    ack: Option<&embassy_supervisor_syntax::Gated<kw::ack_timeout, LitInt>>,
    cr: &TokenStream2,
) -> [Option<GatedBuilder>; 2] {
    let clause = |cfg: &[Attribute], builder: &str, ms: &LitInt, span| {
        let builder = format_ident!("{builder}");
        GatedBuilder {
            cfg: cfg.to_vec(),
            tokens: quote!( .#builder(#cr::_export::Duration::from_millis(#ms)) ),
            available: true,
            err: "",
            span,
        }
    };
    [
        slot.map(|g| clause(&g.cfg, "with_slot_timeout", &g.value, g.kw.span)),
        ack.map(|g| clause(&g.cfg, "with_ack_timeout", &g.value, g.kw.span)),
    ]
}

/// `.with_trace_name("<name>\0")` under `metadata-names`: the NUL-terminated
/// copy for the executor metadata (SystemView reads C strings).
fn trace_name_tokens(f: &Features, name: &str) -> Option<TokenStream2> {
    f.metadata_names.then(|| {
        let nul = format!("{name}\0");
        quote!( .with_trace_name(#nul) )
    })
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
            let gate = gate_static(r);
            quote!(#(#cfg)* &#gate)
        })
        .collect();
    (cfg_aware_len(resources.iter().map(|r| &r.cfg)), gate_refs)
}

/// The static a gate table names for resource `r`: its slot, or for a
/// `local` slot the slot's gate view, which keeps its value accessors
/// `unsafe`.
fn gate_static(r: &ResourceDecl) -> Ident {
    if r.local.is_some() {
        local_gate_static(&r.ident)
    } else {
        r.ident.clone()
    }
}

/// `__SV_LGATE_<RES>`: the gate view of `local` slot `RES`.
fn local_gate_static(res: &Ident) -> Ident {
    format_ident!("__SV_LGATE_{}", res)
}

/// Graph-wide facts the per-item emitters consult: resource slots and the
/// contributor slots of `veto` writes.
struct ResourcePlan {
    /// Per resource name: the `#[cfg]` predicate its emitted static is present
    /// under (`None` = unconditional), and whether the name is a pool's
    /// per-member slot array (which `provides:` may not name).
    slots: HashMap<String, (Option<TokenStream2>, bool)>,
    /// The names of the `local` slots, whose gate tables name their gate view.
    local: HashSet<String>,
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
    tag: &str,
    cr: &TokenStream2,
    plan: &ResourcePlan,
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
            let Some((presence, from_pool)) = plan.slots.get(&slot.to_string()) else {
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
            // The entry exists where the slot static does and its own gate holds.
            let mut cfg: Vec<Attribute> = presence
                .iter()
                .map(|pred| syn::parse_quote!(#[cfg(#pred)]))
                .collect();
            cfg.extend(p.cfg.iter().cloned());
            let gate = if plan.local.contains(&slot.to_string()) {
                local_gate_static(slot)
            } else {
                slot.clone()
            };
            let r = quote!(#(#cfg)* &#gate);
            cfgs.push(cfg);
            Ok(r)
        })
        .collect::<SynResult<Vec<_>>>()?;
    let len = cfg_aware_len(cfgs.iter());
    let node_cfg = &n.cfg;
    let arr = format_ident!("__SV_PROVIDES_{}", tag);
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

/// What a `deps:` name resolves to, for the marked-dep arrays.
struct DepTarget<'a> {
    /// The target item's `#[cfg]`s: an entry naming a compiled-out item drops
    /// out of the array with it, so the array never names a missing static.
    cfg: &'a [Attribute],
    /// A pool stands in by its floor member (`&POOL[0]`).
    pool: bool,
}

/// The `[&'static TaskNode; n]` element and length tokens for the deps
/// carrying one marker, cfg-aware like `gate_tokens`. A dep naming a pool
/// resolves to that pool's floor member (`&POOL[0]`), matching how `deps: [POOL]`
/// resolves for spawn ordering.
fn marked_dep_tokens(
    deps: &[Dep],
    targets: &HashMap<String, DepTarget<'_>>,
    select: impl Fn(&Dep) -> bool,
) -> Option<(TokenStream2, Vec<TokenStream2>)> {
    let marked: Vec<&Dep> = deps.iter().filter(|d| select(d)).collect();
    if marked.is_empty() {
        return None;
    }
    let mut cfgs: Vec<Vec<Attribute>> = Vec::with_capacity(marked.len());
    let refs: Vec<TokenStream2> = marked
        .iter()
        .map(|d| {
            let ident = &d.ident;
            let target = targets.get(&ident.to_string());
            // Only the target's `#[cfg]`s: its other attributes (a doc comment,
            // a lint level) belong to its own statics, not to an array element.
            let target_cfg = target
                .map_or(&[][..], |t| t.cfg)
                .iter()
                .filter(|a| a.path().is_ident("cfg"));
            let cfg: Vec<Attribute> = d.cfg.iter().chain(target_cfg).cloned().collect();
            let reference = if target.is_some_and(|t| t.pool) {
                quote!(&#ident[0])
            } else {
                quote!(&#ident)
            };
            let r = quote!(#(#cfg)* #reference);
            cfgs.push(cfg);
            r
        })
        .collect();
    Some((cfg_aware_len(cfgs.iter()), refs))
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
    // Private helpers are named after this tag (see `expand`).
    tag: &str,
    spawn: &Option<Expr>,
    // `task:` node: its shell is resident, so the glue spawns it once and
    // restarts it in place after that (see `emit_shell`).
    resident: bool,
    executor: &Option<Ident>,
    resources: &[ResourceDecl],
    // `state:`: fallibly box the init value in the glue, before the resource
    // probes (a failed alloc strands nothing): `SpawnError::Busy`, retryable.
    state: Option<(&syn::Type, &StateInit)>,
    spawn_fn: &TokenStream2,
    helpers: &HelperIdents,
    f: &Features,
) -> SynResult<TokenStream2> {
    // `resources:` glue prelude: every entry is PROBED here, through the
    // node's gate table (the fail-closed check that turns "unprovided" into
    // `SpawnError::Busy` out of `Supervisor::start`), and read by the shell
    // itself at the start of each activation (`take()` for lend/consume, the
    // non-destructive `get()` for `shared`): a value moved through the
    // task-fn call would be dropped, unrecoverable, when the claim fails, and
    // even a `Copy` handle passed as an argument sits in the task arena for
    // the whole run beside the worker's own copy (rust-lang/rust#62958).
    // Nothing crosses the call.
    let take_prelude = gates_probe(resources, &quote!(&#ident));
    // `state:` requires `task:`, whose shell is resident: the glue boxes the
    // state and hands it over through the node's slot.
    let (state_prelude, state_arg, state_handoff) = match state {
        Some((ty, init)) => {
            let slot = format_ident!("__SV_STATE_{}", tag);
            (
                state_box_stmt(ty, init, helpers),
                vec![quote!(&#slot)],
                quote!(#slot.provide(__state);),
            )
        }
        None => (quote!(), vec![], quote!()),
    };
    // The body after the executor guard. A resident shell is spawned by the
    // first activation only; every later one restarts it (`__sv_start`).
    // `__sv_resident` fails `Busy` while the shell is still finishing its
    // previous activation — before anything is boxed or probed, so a refused
    // restart costs nothing.
    let launch = |stmts: TokenStream2| {
        if resident {
            quote! {
                let __resident = #ident.__sv_resident()?;
                #state_prelude
                #take_prelude
                #state_handoff
                if __resident {
                    #ident.__sv_start();
                } else {
                    #stmts
                    #ident.__sv_mark_resident();
                }
            }
        } else {
            quote! {
                #state_prelude
                #take_prelude
                #stmts
            }
        }
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
        // A path or partial call: wrap the task fn as spawn glue.
        // With `executor:` the glue spawns through the named `SpawnerSlot`; an
        // unfilled slot fails with `SpawnError::Busy`.
        // `SendSpawner::spawn` requires `Send` on the arguments, not the future,
        // so any `task:` worker can route through any executor.
        (Some(e @ (Expr::Path(_) | Expr::Call(_))), executor) => {
            let mut lead: Vec<TokenStream2> = vec![quote!(&#ident)];
            lead.extend(state_arg.iter().cloned());
            let call = inject_call_with(e, &lead)?;
            match executor {
                None => {
                    // Mixed-site: a `spawn:` argument cannot name the spawner.
                    let s = Ident::new("s", proc_macro2::Span::mixed_site());
                    let body = launch(spawn_stmts(&call, &quote!(&#ident), &quote!(#s), f));
                    quote!(::core::option::Option::Some(
                        (|#s| {
                            #body
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
                Some(ex) => {
                    let body = launch(spawn_stmts(&call, &quote!(&#ident), &quote!(__sp), f));
                    quote!(::core::option::Option::Some(
                        (|_s| {
                            // The supervisor waits for this slot to fill before
                            // invoking the glue (the node carries `.with_executor(&EX)`
                            // and the bring-up bounds the wait), so `get()` is already
                            // filled; `ok_or` is the belt-and-braces unfilled guard.
                            // Resources are taken AFTER the spawner guard, so an
                            // unfilled executor never consumes (and strands) them.
                            let __sp = #ex
                                .get()
                                .ok_or(::embassy_executor::SpawnError::Busy)?;
                            #body
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

/// The glue's resource probe: one out-of-line call over the node's gate
/// table (`TaskNode::__sv_gates_filled`), which already carries each entry's
/// `#[cfg]`. Nothing for an item without `resources:`.
fn gates_probe(resources: &[ResourceDecl], node_ref: &TokenStream2) -> Option<TokenStream2> {
    (!resources.is_empty()).then(|| {
        quote! {
            if !(#node_ref).__sv_gates_filled() {
                return ::core::result::Result::Err(::embassy_executor::SpawnError::Busy);
            }
        }
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
/// * `trace` on → bind the token and `adopt` it (`set_task_id` + NUL-terminated
///   name stamp under `metadata-names`).
/// * `trace` off but `metadata-names` on → bind the token and `stamp_name` only:
///   the node name reaches the task Metadata (for rtos-trace/SystemView) with no id
///   capture and no dependency on the `_embassy_trace_*` hooks.
/// * neither → plain infallible spawn.
fn spawn_stmts(
    call: &TokenStream2,
    node_ref: &TokenStream2,
    sp: &TokenStream2,
    f: &Features,
) -> TokenStream2 {
    if f.trace {
        // `adopt` = set_task_id + (under metadata-names) Metadata name stamp.
        quote! {
            let __token = #call?;
            (#node_ref).adopt(&__token);
            #sp.spawn(__token);
        }
    } else if f.metadata_names {
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

/// The slot a shell takes resource entry `i` from and restores it to: a pool
/// member's own element rides in as the `__r{i}_slot` parameter, a node names
/// its slot static.
fn take_slot(pool_member: bool, i: usize, res: &Ident) -> TokenStream2 {
    if pool_member {
        let slot_param = format_ident!("__r{}_slot", i);
        quote!(#slot_param)
    } else {
        quote!(#res)
    }
}

/// Emit the `#[embassy_executor::task]` shell for a `task:` clause: a concrete,
/// non-generic task fn that takes only the node and awaits the user's worker with
/// the node injected first. This is how a **generic** worker becomes spawnable:
/// embassy forbids generic tasks (one static `TaskPool` per concrete future type),
/// so a monomorphized shell is stamped per declaration. Worker args are evaluated
/// inside the shell, at the start of each activation, on the node's own executor,
/// so the DSL never needs the arg types and a cross-core node builds its resources
/// on the core that runs them.
///
/// Returns the shell item and a path `Expr` naming it, which feeds the ordinary
/// `spawn:` path-form glue (executor routing and trace `adopt` compose unchanged).
// One argument per independent codegen input; a bundling struct would only
// rename the coupling.
#[allow(clippy::too_many_arguments)]
fn emit_shell(
    owner: &Ident,
    // Private helpers are named after this tag (see `expand`).
    tag: &str,
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
    f: &Features,
) -> SynResult<(TokenStream2, Expr)> {
    if !matches!(worker, Expr::Path(_) | Expr::Call(_)) {
        return Err(syn::Error::new_spanned(
            worker,
            "`task:` names an async worker fn — a path or a partial call like \
             `worker(args)`; for a closure or a ready spawn fn use `spawn:`",
        ));
    }
    let shell = format_ident!("__sv_task_{}", tag.to_lowercase());
    // `resources:` handling: the shell reads every entry at the start of each
    // activation (`take()` for lend/consume, the non-destructive `get()` for
    // `shared`), never through the task-fn call, where a `Busy` storage claim
    // would drop an owned value and even a `Copy` handle argument would sit in
    // the task arena beside the worker's own copy (rust-lang/rust#62958). The
    // glue probed every slot, so a read that comes up empty means an
    // out-of-band `take()` won the race: the shell restores the entries it
    // already took and exits (`__sv_lost_resource`: a warn plus the completion
    // record), a failed activation rather than a panic.
    //
    // Lend entries are lent to the worker as `&mut` and restored to their slot
    // after the worker returns (after its clean shutdown ack), so a Terminate
    // respawn re-takes the same instance instead of re-acquiring hardware; a
    // `Pause` worker parks instead and retains them. `consume` and `shared`
    // forward by value with no restore: a consumed slot stays empty until the
    // app re-`provide()`s (fail-closed respawn), a shared slot was never
    // emptied. A `divisible` entry forwards the member's `Claimant` by value;
    // the supervisor releases its share.
    //
    // Per-entry `#[cfg]` rides on params, reads, worker-call arguments and
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
                    let slot_param = take_slot(pool_member, i, &r.ident);
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
    // A `local` slot's value accessors are `unsafe`: their contract is the
    // slot's executor, which the shell runs on (checked at expansion). `read`
    // is the `let`-`else` initializer for a take or a copy.
    let read = |r: &ResourceDecl, call: TokenStream2| -> TokenStream2 {
        if r.local.is_some() {
            // SAFETY: the shell runs on the slot's executor (checked at
            // expansion). Parenthesized: a `let`-`else` initializer cannot
            // end in `}`.
            quote!((unsafe { #call }))
        } else {
            call
        }
    };
    // The statement that puts entry `i` back in its slot. Only the take kinds
    // move a value out of a slot.
    let restore = |i: usize, r: &ResourceDecl| -> Option<TokenStream2> {
        if !matches!(r.kind(), ResourceKind::Lend | ResourceKind::Consume) {
            return None;
        }
        let cfg = &r.cfg;
        let var = format_ident!("__r{}", i);
        let slot = take_slot(pool_member, i, &r.ident);
        Some(if r.local.is_some() {
            // SAFETY: the shell runs on the slot's executor (checked at
            // expansion).
            quote!(#(#cfg)* unsafe { #slot.restore(#var); })
        } else {
            quote!(#(#cfg)* #slot.restore(#var);)
        })
    };
    // A lost read reclaims the `state:` box too, after the restores and before
    // the exit record, so has_exited() implies the heap is back on this path
    // as well.
    let lost_reclaim = if state.is_some() {
        quote!(::core::mem::drop(__state);)
    } else {
        quote!()
    };
    let res_takes: Vec<TokenStream2> = resources
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let cfg = &r.cfg;
            let var = format_ident!("__r{}", i);
            let res = &r.ident;
            // A read that comes up empty hands back what this activation took
            // so far, before the exit record, so a respawn finds those filled;
            // in reverse order, so each lost arm is a suffix of the next and
            // the compiler can merge them.
            let undo: Vec<TokenStream2> = resources[..i]
                .iter()
                .enumerate()
                .rev()
                .filter_map(|(j, r)| restore(j, r))
                .collect();
            let lost = quote! {
                #(#undo)*
                #lost_reclaim
                __node.__sv_lost_resource();
                break '__sv_act;
            };
            match r.kind() {
                ResourceKind::Shared => {
                    let get = read(r, quote!(#res.get()));
                    Some(quote! {
                        #(#cfg)*
                        let ::core::option::Option::Some(#var) = #get else { #lost };
                    })
                }
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
                    let slot = take_slot(pool_member, i, res);
                    let take = read(r, quote!(#slot.take()));
                    Some(quote! {
                        #(#cfg)*
                        let ::core::option::Option::Some(#mutability #var) = #take else {
                            #lost
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
        .filter_map(|(i, r)| restore(i, r))
        .collect();
    let alloc_alias = &helpers.alloc_alias;
    // `state:` crosses from the spawn glue (which boxes it, fallibly, per
    // activation) into the resident shell through a handoff slot: a node's
    // own static, or member `I`'s element of the pool's array, passed in by
    // the wrapper. A `ResourceSlot` is `Sync` only for `Send` contents, so the
    // state type must be `Send`.
    let state_slot = format_ident!("__SV_STATE_{}", tag);
    let (state_param, state_def, state_take, state_lease, state_drop) = match state {
        Some((ty, _)) => {
            let def = if pool_member {
                quote! {
                    #(#cfg)*
                    static #state_slot: [#cr::ResourceSlot<#alloc_alias::boxed::Box<#ty>>; #pool_size] =
                        [const { #cr::ResourceSlot::new() }; #pool_size];
                }
            } else {
                quote! {
                    #(#cfg)*
                    static #state_slot: #cr::ResourceSlot<#alloc_alias::boxed::Box<#ty>> =
                        #cr::ResourceSlot::new();
                }
            };
            (
                quote!(, __state_slot: &'static #cr::ResourceSlot<#alloc_alias::boxed::Box<#ty>>),
                def,
                quote! {
                    let ::core::option::Option::Some(mut __state) = __state_slot.take() else {
                        __node.__sv_lost_resource();
                        break '__sv_act;
                    };
                },
                vec![quote!(&mut *__state)],
                // Reclaim the bulk FIRST: before restores, exit-provide, and the
                // completion record, so has_exited() implies the heap is back.
                quote!(::core::mem::drop(__state);),
            )
        }
        None => (quote!(), quote!(), quote!(), vec![], quote!()),
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
    // Pin the worker in the shell frame so its state is stored once. This
    // avoids doubling it as a callee argument, which matters in static task
    // storage.
    // With `fault-inject` the pinned worker is wrapped in `Injected`: stall,
    // hog and crash apply transparently. The wrapper still resolves to an
    // `Option`, so diverging-`exit:` remains a lint error as usual.
    let (drive, provide_exit) = if f.fault_inject {
        match (cancel, exit) {
            (false, Some(ty)) => {
                let provide = provide(ty);
                let out_ident = out_ident(ty);
                (
                    quote!(let __res = { let __fut = ::core::pin::pin!(#call); #cr::Injected::new(__node, __fut).await };),
                    quote!(if let ::core::option::Option::Some(#out_ident) = __res {
                        #provide
                    }),
                )
            }
            (false, None) => (
                quote!({
                    let __fut = ::core::pin::pin!(#call);
                    let _ = #cr::Injected::new(__node, __fut).await;
                }),
                quote!(),
            ),
            (true, Some(ty)) => {
                let provide = provide(ty);
                let out_ident = out_ident(ty);
                (
                    quote!(let __res = { let __fut = ::core::pin::pin!(#call); __node.run_cancellable(#cr::Injected::new(__node, __fut)).await };),
                    quote!(if let ::core::result::Result::Ok(::core::option::Option::Some(#out_ident)) = __res {
                        #provide
                    }),
                )
            }
            (true, None) => (
                quote!({
                    let __fut = ::core::pin::pin!(#call);
                    let _ = __node.run_cancellable(#cr::Injected::new(__node, __fut)).await;
                }),
                quote!(),
            ),
        }
    } else {
        match (cancel, exit) {
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
        }
    };
    // Record the completion (and ack any pending shutdown handshake): a worker
    // that returns on its own reads as down, not running forever, and a control
    // Activate can start it again. A per-node shell names its node STATIC here
    // (like the `_EXIT` provide above). A pool shell is shared by its members,
    // so only the parameter knows the node.
    let (mark_exited, park) = if pool_member {
        (
            quote!(__node.mark_exited();),
            quote!(__node.__sv_park().await;),
        )
    } else {
        (
            quote!(#owner.mark_exited();),
            quote!(#owner.__sv_park().await;),
        )
    };
    // Resident: spawned once, the shell runs one activation per start and
    // parks in between, so a restart never waits for its task slot to free
    // (which happens only after the final poll returns, out of the
    // supervisor's sight). Parking is the shell's last act of an activation,
    // after the restores and the exit record, so a parked shell has nothing
    // left that could touch the next activation's state.
    let def = quote! {
        #state_def
        #(#cfg)*
        #[::embassy_executor::task(pool_size = #ps)]
        #allow_unreachable
        #[allow(unused_labels)]
        async fn #shell(__node: &'static #cr::TaskNode #(, #res_params)* #state_param) {
            loop {
                '__sv_act: {
                    #state_take
                    #(#res_takes)*
                    #drive
                    #state_drop
                    #(#restores)*
                    #provide_exit
                    #mark_exited
                }
                #park
            }
        }
    };
    let path: Expr = syn::parse_quote!(#shell);
    Ok((def, path))
}

/// Emit a `node`: its `pub static #ident: TaskNode` definition and its `Slot`. The
/// caller assigns the slot index and records the name, so this touches neither.
/// A `task:` node additionally emits its generated shell ahead of the static.
// One argument per independent codegen input; a bundling struct would only
// rename the coupling.
#[allow(clippy::too_many_arguments)]
fn emit_node(
    n: &NodeItem,
    // Private helpers are named after this tag (see `expand`).
    tag: &str,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    // What each `deps:` name resolves to, for the marked-dep arrays.
    targets: &HashMap<String, DepTarget<'_>>,
    helpers: &HelperIdents,
    observe: &ObserveDefaults,
    // Graph-wide resource facts: `provides:` name resolution and this node's
    // slot in each `divisible` budget.
    plan: &ResourcePlan,
    f: &Features,
) -> SynResult<(TokenStream2, Slot)> {
    let ident = &n.ident;
    let claims = plan.claims(&n.ident, &n.resources);
    let cfg = &n.cfg;
    let mode = &n.mode;
    let name = name_string(&n.ident);
    let with_disabled = disabled_tokens(n.disabled.as_ref());
    let (shell_def, spawn_expr) = match &n.source {
        Some(TaskSource::Shell(worker)) => {
            let (def, path) = emit_shell(
                ident,
                tag,
                cfg,
                worker,
                1,
                &n.resources,
                &claims,
                n.exit.as_ref(),
                n.state.as_ref().map(|(_, ty, init)| (ty, init)),
                false,
                n.cancel,
                cr,
                helpers,
                f,
            )?;
            (def, Some(path))
        }
        Some(TaskSource::Spawn(e)) => (quote!(), Some(e.clone())),
        None => (quote!(), None),
    };
    let spawn = node_spawn(
        ident,
        tag,
        &spawn_expr,
        matches!(n.source, Some(TaskSource::Shell(_))),
        &n.executor,
        &n.resources,
        n.state.as_ref().map(|(_, ty, init)| (ty, init)),
        spawn_fn,
        helpers,
        f,
    )?;
    // `executor: NAME` routes the node through that SpawnerSlot; the supervisor
    // awaits the slot before spawning (see `TaskNode::with_executor`).
    let mut with_exec = match &n.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    // With `fault-inject`, shelled `task:` nodes can be stalled, crashed or hogged.
    // Hand-written `spawn:` tasks only support wedge.
    if f.fault_inject && matches!(n.source, Some(TaskSource::Shell(_))) {
        with_exec.extend(quote!( .with_shell() ));
    }
    // `resources: [NAME: Type, ..]` — one `pub static NAME: ResourceSlot<Type>`
    // per entry (main moves the resource in with `NAME.provide(..)`), plus a
    // type-erased gate array wired into the node so the supervisor can await
    // provisioning/restore before each (re)spawn (see `TaskNode::with_resources`).
    // The unsized coercion `&NAME` -> `&dyn ResourceGate` happens in the static
    // initializer, where it is allowed.
    let (res_defs, with_res) = if n.resources.is_empty() {
        (quote!(), quote!())
    } else {
        let gates_ident = format_ident!("__SV_GATES_{}", tag);
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
                // but without its `T: Send` bound, for `!Send` driver handles local
                // to one executor, and a gate view the gate tables name in its
                // place. `consume` changes only shell codegen (by-value arg, no
                // restore): the slot type is the same either way.
                let (slot_ty, gate_def) = if r.local.is_some() {
                    let (local, local_gate) = (&helpers.local_slot, &helpers.local_gate);
                    let lgate = local_gate_static(res);
                    (
                        quote!(#local<#ty>),
                        quote! {
                            #(#cfg)*
                            #(#ecfg)*
                            #[doc(hidden)]
                            #[allow(non_upper_case_globals)]
                            pub static #lgate: #local_gate<#ty> = #local_gate(&#res);
                        },
                    )
                } else {
                    (quote!(#cr::ResourceSlot<#ty>), quote!())
                };
                let mut doc = if r.consume.is_some() {
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
                if r.local.is_some() {
                    doc.push_str(LOCAL_SLOT_DOC);
                }
                quote! {
                    #(#cfg)*
                    #(#ecfg)*
                    #[doc = #doc]
                    pub static #res: #slot_ty = <#slot_ty>::new();
                    #gate_def
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
        timeout_clauses(n.slot_timeout.as_ref(), n.ack_timeout.as_ref(), cr)
            .into_iter()
            .chain([
                n.beat_timeout.as_ref().map(|g| {
                    let (c, ms) = (&g.cfg, &g.value);
                    GatedBuilder {
                        cfg: c.clone(),
                        tokens: quote!( .with_beat_timeout(#cr::_export::Duration::from_millis(#ms)) ),
                        available: f.liveness_monitor,
                        err: gate::BEAT_TIMEOUT_NEEDS,
                        span: g.kw.span,
                    }
                }),
                // `beat_window:` alone is rejected as a shape error.
                n.beat_window.as_ref().map(|g| {
                    let (c, w) = (&g.cfg, &g.value);
                    GatedBuilder {
                        cfg: c.clone(),
                        tokens: quote!( .with_beat_window(#w) ),
                        available: f.liveness_monitor,
                        err: gate::BEAT_WINDOW_NEEDS,
                        span: g.kw.span,
                    }
                }),
                n.enabled_if.as_ref().map(|g| {
                    let (c, pred) = (&g.cfg, &g.value);
                    GatedBuilder {
                        cfg: c.clone(),
                        tokens: quote!( .with_enabled_if(#pred) ),
                        available: f.enabled_if,
                        err: gate::ENABLED_IF_NEEDS,
                        span: g.kw.span,
                    }
                }),
                n.restart_budget.as_ref().map(|g| {
                    let (c, (max, window)) = (&g.cfg, &g.value);
                    let max = LitInt::new(&format!("{}u16", max.base10_digits()), max.span());
                    GatedBuilder {
                        cfg: c.clone(),
                        tokens: quote!(
                            .with_restart_budget(#max, #cr::_export::Duration::from_millis(#window))
                        ),
                        available: f.restart_intensity,
                        err: gate::RESTART_BUDGET_NEEDS,
                        span: g.kw.span,
                    }
                }),
                n.ready_on_write.as_ref().map(|g| GatedBuilder {
                    cfg: g.cfg.clone(),
                    tokens: quote!( .with_ready_on_write() ),
                    available: f.coupling_observe && f.readiness,
                    err: gate::READY_ON_WRITE_NEEDS,
                    span: g.kw.span(),
                }),
            ])
            .flatten(),
    );
    // The node's view of its own graph: what a data-driven dependency resolves
    // its producer through. Const, and a cycle only in the address sense — the
    // graph names the nodes, each node names the graph.
    let graph_ref = &helpers.graph_ref;
    let with_graph = f.data_deps.then(|| quote!( .with_graph(&#graph_ref) ));
    // `deps: [X ready, ..]` / `[X ready bound]`: the marked subsets become
    // `[&'static TaskNode; n]` arrays wired via `.with_ready_deps` /
    // `.with_bound_deps`; each entry carries its own and its target's `#[cfg]`s.
    let ctx = EmitCtx { cfg, cr, tag };
    let marker = |select: fn(&Dep) -> bool, prefix, builder| {
        marker_array_tokens(&ctx, &n.deps, targets, select, prefix, builder)
    };
    let (ready_def, with_ready) = marker(|d| d.ready.is_some(), "READY", "with_ready_deps");
    let (bound_def, with_bound) = marker(|d| d.bound.is_some(), "BOUND", "with_bound_deps");
    let mut foreign: Vec<AdoptedFn> = Vec::new();
    foreign.extend(
        discover_fn_path(n.discover.as_ref().map(|g| &g.kw), n.source.as_ref())?
            .filter(|_| f.dataflow)
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
        let owner = format!("node `{ident}`");
        let r = marker_assert_tokens(&ctx, &owner, d, &foreign, &n.reads, "READS");
        let w = marker_assert_tokens(&ctx, &owner, d, &foreign, &n.writes, "WRITES");
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
    let cfg_ident = format_ident!("__SV_CFG_{}", tag);
    let handle_ident = format_ident!("__SV_HANDLE_{}", tag);
    let (prov_def, with_provides) = provides_tokens(n, tag, cr, plan)?;
    let (claims_def, with_claims) = claims_tokens(
        &format_ident!("__SV_CLAIMS_{}", tag),
        cfg,
        &n.resources,
        &claims,
        0,
        cr,
    );
    let with_trace_name = trace_name_tokens(f, &name);
    let cfg_chain = quote! {
        #cr::NodeCfg::new(#name, #cr::Mode::#mode, #spawn)
            #with_exec #with_res #with_provides #with_claims #with_clauses #with_disabled
            #with_ready #with_bound #with_reads #with_writes
            #with_graph #with_trace_name
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
    let def = quote! {
        #clause_errors
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
        #[doc(hidden)]
        static #handle_ident: #cr::TaskHandle = #cr::TaskHandle::new();
        #(#cfg)*
        #[doc = #node_doc]
        pub static #ident: #cr::TaskNode = #cr::TaskNode::new(&#cfg_ident, &#handle_ident);
    };
    let slot = Slot {
        cfg_pred: cfg_predicate(cfg),
        reference: quote!(&#ident),
        deps: n.deps.clone(),
        fragment: n.fragment.clone(),
    };
    Ok((def, slot))
}

// One argument per independent codegen input; a bundling struct would only
// rename the coupling.
#[allow(clippy::too_many_arguments)]
fn emit_pool(
    p: &PoolItem,
    // Private helpers are named after this tag (see `expand`).
    tag: &str,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
    // What each `deps:` name resolves to, for the marked-dep arrays.
    targets: &HashMap<String, DepTarget<'_>>,
    helpers: &HelperIdents,
    observe: &ObserveDefaults,
    // Graph-wide resource facts: the pool's base slot in each `divisible`
    // budget (member `j` holds base + j).
    plan: &ResourcePlan,
    f: &Features,
) -> SynResult<(Vec<TokenStream2>, TokenStream2, Vec<Slot>)> {
    let ident = &p.ident;
    let claims = plan.claims(&p.ident, &p.resources);
    let cfg = &p.cfg;
    let lname = name_string(&p.ident);
    let pool_static = format_ident!("{}_POOL", ident);
    let k = p.modes.len();

    // Literal bounds were checked by the parser; an expression is checked by
    // the const asserts below.
    let lit_bounds = match (&p.min, &p.max) {
        (Expr::Lit(lmin), Expr::Lit(lmax)) => match (&lmin.lit, &lmax.lit) {
            (syn::Lit::Int(imin), syn::Lit::Int(imax)) => {
                Some((imin.base10_parse::<u8>()?, imax.base10_parse::<u8>()?))
            }
            _ => None,
        },
        _ => None,
    };

    let (shell_def, member_expr) = match &p.source {
        TaskSource::Spawn(e) => (quote!(), e.clone()),
        TaskSource::Shell(worker) => emit_shell(
            ident,
            tag,
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
            f,
        )?,
    };
    // The glue's own bindings: mixed-site, so a user's `spawn:` arguments can
    // neither name nor be shadowed by them.
    let (s, i) = (
        Ident::new("s", proc_macro2::Span::mixed_site()),
        Ident::new("i", proc_macro2::Span::mixed_site()),
    );
    let res_args: Vec<TokenStream2> = p
        .resources
        .iter()
        .zip(&claims)
        .filter_map(|(r, base)| {
            let ecfg = &r.cfg;
            let res = &r.ident;
            match r.kind() {
                ResourceKind::Lend | ResourceKind::Consume => Some(quote!(#(#ecfg)* &#res[#i])),
                ResourceKind::Divisible => {
                    let base = base.expect("a divisible entry has a slot");
                    Some(quote!(#(#ecfg)* #res.claimant((#base as usize + #i) as u8)))
                }
                ResourceKind::Shared => None,
            }
        })
        .collect();
    // A `task:` pool's shell is resident per member (see `emit_shell`): member
    // `i` spawns its shell instance once and restarts it after that, its
    // `state:` box crossing through element `i` of the handoff array.
    let resident = matches!(p.source, TaskSource::Shell(_));
    let state_slot = format_ident!("__SV_STATE_{}", tag);
    let (state_prelude, state_arg, state_handoff) = match &p.state {
        Some((_, ty, init)) => (
            state_box_stmt(ty, init, helpers),
            vec![quote!(&#state_slot[#i])],
            quote!(#state_slot[#i].provide(__state);),
        ),
        None => (quote!(), vec![], quote!()),
    };
    let mut lead: Vec<TokenStream2> = vec![quote!(&#ident[#i])];
    lead.extend(res_args);
    lead.extend(state_arg);
    let call = inject_call_with(&member_expr, &lead)?;
    let (param, prelude, sp_tokens) = match &p.executor {
        None => (quote!(#s), quote!(), quote!(#s)),
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
    // Member `i`'s gate table (`__SV_GATES_<tag>_<i>`) lists its own element
    // of each take-kind array and the pool-wide `shared`/`divisible` statics;
    // a pool without take-kind entries shares one table (`__SV_GATES_<tag>`).
    let get_prelude = gates_probe(&p.resources, &quote!(&#ident[#i]));
    let pool_spawn_stmts = spawn_stmts(&call, &quote!(&#ident[#i]), &sp_tokens, f);
    let body = if resident {
        quote! {
            let __resident = #ident[#i].__sv_resident()?;
            #state_prelude
            #get_prelude
            #state_handoff
            if __resident {
                #ident[#i].__sv_start();
            } else {
                #pool_spawn_stmts
                #ident[#i].__sv_mark_resident();
            }
        }
    } else {
        quote! {
            #state_prelude
            #get_prelude
            #pool_spawn_stmts
        }
    };
    // One spawn body per pool, indexed by the member at run time. Each
    // member's `NodeCfg.spawn` is a bare `fn(Spawner)`, so every member keeps
    // its own fn item: a trampoline that passes its index in and inlines to a
    // tail call.
    let wrapper = format_ident!("__sv_spawn_{}", tag.to_lowercase());
    let wrapper_at = format_ident!("__sv_spawn_{}_at", tag.to_lowercase());
    let mut defs: Vec<TokenStream2> = Vec::new();
    defs.push(shell_def);
    defs.push(quote! {
        #(#cfg)*
        fn #wrapper_at(
            #param: ::embassy_executor::Spawner,
            #i: usize,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError> {
            #prelude
            #body
            ::core::result::Result::Ok(())
        }
        #(#cfg)*
        #[inline(always)]
        fn #wrapper<const I: usize>(
            #s: ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError> {
            #wrapper_at(#s, I)
        }
    });
    let member_spawn: Vec<TokenStream2> = (0..k).map(|j| quote!(#wrapper::<#j>)).collect();

    let mut member_with_exec = match &p.executor {
        Some(ex) => quote!( .with_executor(&#ex) ),
        None => quote!(),
    };
    if f.fault_inject && matches!(p.source, TaskSource::Shell(_)) {
        member_with_exec.extend(quote!( .with_shell() ));
    }
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
        // A take-kind entry gives each member its own element, hence its own
        // table; with none, every entry is a pool-wide static and one table
        // serves every member.
        let per_member = p
            .resources
            .iter()
            .any(|r| matches!(r.kind(), ResourceKind::Lend | ResourceKind::Consume));
        let tables = if per_member { k } else { 1 };
        let gates_idents: Vec<Ident> = (0..tables)
            .map(|j| {
                let gates_ident = if per_member {
                    format_ident!("__SV_GATES_{}_{}", tag, j)
                } else {
                    format_ident!("__SV_GATES_{}", tag)
                };
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
                                let gate = gate_static(r);
                                quote!(#(#ecfg)* &#gate)
                            }
                        }
                    })
                    .collect();
                defs.push(quote! {
                    #(#cfg)*
                    static #gates_ident: [&'static dyn #cr::ResourceGate; #gates_len] =
                        [#(#gate_refs),*];
                });
                gates_ident
            })
            .collect();
        (0..k)
            .map(|j| {
                let gates_ident = &gates_idents[if per_member { j } else { 0 }];
                quote!( .with_resources(&#gates_ident) )
            })
            .collect()
    };
    let member_with_claims: Vec<TokenStream2> = (0..k)
        .map(|j| {
            let (def, with) = claims_tokens(
                &format_ident!("__SV_CLAIMS_{}_{}", tag, j),
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
    let ctx = EmitCtx { cfg, cr, tag };
    let marker = |select: fn(&Dep) -> bool, prefix, builder| {
        marker_array_tokens(&ctx, &p.deps, targets, select, prefix, builder)
    };
    let (ready_def, member_with_ready) = marker(|d| d.ready.is_some(), "READY", "with_ready_deps");
    let (bound_def, member_with_bound) = marker(|d| d.bound.is_some(), "BOUND", "with_bound_deps");
    let mut foreign: Vec<AdoptedFn> = Vec::new();
    foreign.extend(
        discover_fn_path(p.discover.as_ref().map(|g| &g.kw), Some(&p.source))?
            .filter(|_| f.dataflow)
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
                    let member_tag = format!("{tag}_{j}");
                    let member_ctx = EmitCtx {
                        cfg,
                        cr,
                        tag: &member_tag,
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
        let owner = format!("pool `{ident}`");
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
        timeout_clauses(p.slot_timeout.as_ref(), p.ack_timeout.as_ref(), cr)
            .into_iter()
            .flatten(),
    );
    defs.push(member_clause_errors);
    let member_graph_ref = &helpers.graph_ref;
    let member_with_graph = f
        .data_deps
        .then(|| quote!( .with_graph(&#member_graph_ref) ));
    let member_cfg_ident = format_ident!("__SV_CFG_{}", tag);
    let member_cfgs = p
        .modes
        .iter()
        .zip(&member_spawn)
        .enumerate()
        .map(|(j, (mode, sp))| {
            let nm = format!("{lname}{j}");
            let with_trace_name = trace_name_tokens(f, &nm);
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
                  #member_with_reads #member_with_writes #member_with_graph #with_trace_name
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
    // Pool members are never `disabled`: no `with_disabled_at_boot`.
    let member_handle_ident = format_ident!("__SV_HANDLE_{}", tag);
    let members = (0..k)
        .map(|j| quote!( #cr::TaskNode::new(&#member_cfg_ident[#j], &#member_handle_ident[#j]) ));
    let members_doc = format!(
        "Pool `{ident}`'s members, one `TaskNode` per slot (index = member index). \
         Index it for the per-node verbs; the pool itself is `{ident}_POOL`."
    );
    defs.push(quote! {
        #(#cfg)*
        #[doc(hidden)]
        static #member_cfg_ident: [#cr::NodeCfg; #k] = [ #(#member_cfgs),* ];
        #(#cfg)*
        #[doc(hidden)]
        static #member_handle_ident: [#cr::TaskHandle; #k] =
            [const { #cr::TaskHandle::new() }; #k];
        #(#cfg)*
        #[doc = #members_doc]
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
            // Spanned on the clause each assert is about, and named after the
            // pool, so a failure points at the expression to fix.
            let at_min = format!("pool `{ident}`: `min:` must not exceed `max:`");
            let over_members = format!("pool `{ident}`: `max:` exceeds the declared member count");
            let over_u8 = format!("pool `{ident}`: `max:` exceeds 255 (ElasticPool bounds are u8)");
            let mut asserts = quote::quote_spanned! {min_e.span()=>
                #(#cfg)*
                const _: () = ::core::assert!(#min_const <= #max_const, #at_min);
            };
            asserts.extend(quote::quote_spanned! {max_e.span()=>
                #(#cfg)*
                const _: () = ::core::assert!(#max_const <= #members_const, #over_members);
                #(#cfg)*
                const _: () = ::core::assert!(#max_const <= 255, #over_u8);
            });
            (quote!({ #min_e }), quote!({ #max_e }), asserts)
        }
    };
    let min_doc =
        format!("Pool `{ident}`'s `min:` floor (validated at expansion or by const assert).");
    let max_doc = format!(
        "Pool `{ident}`'s `max:` scaling ceiling: the most members ever running concurrently."
    );
    let members_count_doc =
        format!("Pool `{ident}`'s declared member count (the `[TaskNode; K]` array length).");
    defs.push(quote! {
        #(#cfg)*
        #[doc = #min_doc]
        pub const #min_const: usize = #min_tokens;
        #(#cfg)*
        #[doc = #max_doc]
        pub const #max_const: usize = #max_tokens;
        #(#cfg)*
        #[doc = #members_count_doc]
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
    let pool_doc = format!(
        "The `ElasticPool` over the `{ident}` members: the `min:`/`max:` bounds and the \
         scaling policy `Supervisor::run_pools` drives. Also reachable through `GRAPH.pools`."
    );
    defs.push(quote! {
        #(#cfg)*
        #[doc = #pool_doc]
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

        // A dep repeated in one list was refused by the parser.
        let mut dep_toks: Vec<TokenStream2> = Vec::new();
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
            dep_toks.push(quote!( #(#cfg)* #idx ));
        }
        deps_entries.push(quote!( &[ #(#dep_toks),* ] ));
    }
    Ok((all_entries, deps_entries))
}

/// Refuse an item or resource named like a public item the expansion derives
/// from another one's name (`X_EXIT`, `P_POOL`, `P_MIN`, `P_MAX`,
/// `P_MEMBERS`): both would be items of the declaring module.
fn check_derived_names(graph: &GraphSpec) -> SynResult<()> {
    let mut derived: HashMap<String, String> = HashMap::new();
    for item in &graph.items {
        match item {
            Item::Node(n) if n.exit.is_some() => {
                let x = n.ident.unraw();
                derived.insert(
                    format!("{x}_EXIT"),
                    format!("the `exit:` slot of node `{x}`"),
                );
            }
            Item::Pool(p) => {
                let id = p.ident.unraw();
                for (suffix, what) in [
                    ("POOL", "the `ElasticPool` static"),
                    ("MIN", "the `min:` const"),
                    ("MAX", "the `max:` const"),
                    ("MEMBERS", "the member-count const"),
                ] {
                    derived.insert(format!("{id}_{suffix}"), format!("{what} of pool `{id}`"));
                }
            }
            _ => {}
        }
    }
    for item in &graph.items {
        let own = match item {
            Item::Node(n) => &n.ident,
            Item::Pool(p) => &p.ident,
            Item::Executor(x) => &x.ident,
        };
        for ident in std::iter::once(own).chain(item_resources(item).iter().map(|r| &r.ident)) {
            if let Some(what) = derived.get(&ident.unraw().to_string()) {
                return Err(syn::Error::new_spanned(
                    ident,
                    format!(
                        "`{ident}` is also the name the macro gives {what}; rename one of them"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Refuse two items that share a runtime name: a node goes by its lower-cased
/// ident, a pool member by its pool's followed by its index, and a lookup by
/// name finds only the first. An ident declared twice is left to the
/// duplicate-name check.
fn check_runtime_names(graph: &GraphSpec) -> SynResult<()> {
    // runtime name -> (how the first declaration reads, its ident)
    let mut seen: HashMap<String, (String, String)> = HashMap::new();
    for item in &graph.items {
        let names: Vec<(String, String, &Ident)> = match item {
            Item::Node(n) => vec![(name_string(&n.ident), n.ident.to_string(), &n.ident)],
            Item::Pool(p) => (0..p.modes.len())
                .map(|j| {
                    let name = format!("{}{j}", name_string(&p.ident));
                    (name, format!("{}[{j}]", p.ident), &p.ident)
                })
                .collect(),
            Item::Executor(_) => continue,
        };
        for (name, label, ident) in names {
            match seen.get(&name) {
                Some((first, first_ident)) if *ident != *first_ident => {
                    return Err(syn::Error::new_spanned(
                        ident,
                        format!(
                            "runtime name `{name}` is taken twice: `{first}` and `{label}` \
                             both map to it (names are lower-cased, and pool members are \
                             `<pool><index>`); rename one"
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    seen.insert(name, (label, ident.to_string()));
                }
            }
        }
    }
    Ok(())
}

fn expand(mut graph: GraphSpec, f: &Features) -> SynResult<TokenStream2> {
    let deferred = gate::gate(&mut graph, f)?;
    check_derived_names(&graph)?;
    check_runtime_names(&graph)?;
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
        let (local, local_gate) = (&helpers.local_slot, &helpers.local_gate);
        let cell = quote!(::core::cell::Cell<::core::option::Option<T>>);
        let raw = quote!(#cr::_export::CriticalSectionRawMutex);
        let safety = quote! {
            #[doc = ""]
            #[doc = "# Safety"]
            #[doc = ""]
            #[doc = "Call only from the one executor this slot's graph declarations run on."]
            #[doc = "The slot is `Sync` for every `T`, so nothing else stops a `!Send` value"]
            #[doc = "from crossing threads."]
        };
        defs.push(quote! {
            #[allow(dead_code)]
            pub struct #local<T> {
                slot: #cr::_export::BlockingMutex<#raw, #cell>,
            }
            unsafe impl<T> ::core::marker::Sync for #local<T> {}
            #[allow(dead_code)]
            impl<T> #local<T> {
                pub const fn new() -> Self {
                    Self {
                        slot: #cr::_export::BlockingMutex::new(
                            ::core::cell::Cell::new(::core::option::Option::None),
                        ),
                    }
                }
                /// Move `value` in, dropping any value already there.
                #safety
                pub unsafe fn provide(&self, value: T) {
                    self.slot.lock(|c| c.set(::core::option::Option::Some(value)));
                    #cr::__sv_gate_event();
                }
                /// Move the value out, leaving the slot empty.
                #safety
                pub unsafe fn take(&self) -> ::core::option::Option<T> {
                    self.slot.lock(::core::cell::Cell::take)
                }
                /// Put a taken value back: `provide` under the name the
                /// generated shell uses.
                #safety
                pub unsafe fn restore(&self, value: T) {
                    // SAFETY: the caller's contract is `provide`'s.
                    unsafe { self.provide(value) }
                }
                /// Empty the slot, dropping its value outside the lock.
                #safety
                pub unsafe fn clear(&self) {
                    let stale = self.slot.lock(::core::cell::Cell::take);
                    ::core::mem::drop(stale);
                }
                /// Whether the slot holds a value. It never moves the value,
                /// so any executor may ask.
                pub fn is_filled(&self) -> bool {
                    self.slot.lock(|c| {
                        // SAFETY: lock held; we only read, never move.
                        unsafe { (*c.as_ptr()).is_some() }
                    })
                }
            }
            #[allow(dead_code)]
            impl<T: ::core::marker::Copy> #local<T> {
                /// Copy the value out, leaving the slot filled.
                #safety
                pub unsafe fn get(&self) -> ::core::option::Option<T> {
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
            /// What the graph's gate and `provides:` tables name for a `local`
            /// slot, so the slot itself implements nothing safe that moves
            /// its value.
            #[doc(hidden)]
            pub struct #local_gate<T: 'static>(pub &'static #local<T>);
            impl<T: 'static> #cr::ResourceGate for #local_gate<T> {
                fn is_filled(&self) -> bool {
                    self.0.is_filled()
                }
                fn clear(&self) {
                    // SAFETY: reached only from the provider's own ack or
                    // exit, and the graph places that provider on the slot's
                    // executor.
                    unsafe { self.0.clear() }
                }
            }
        });
    }

    let executor_names: Vec<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Executor(x) => Some(x.ident.to_string()),
            _ => None,
        })
        .collect();
    // One executor fence per `executor NAME;` (see `SpawnerSlot`): a resident
    // task on that tier, spawned when the slot is filled, whose single poll
    // proves every task polled before it has returned from its poll.
    if !executor_names.is_empty() {
        let fence = &helpers.fence;
        let n = LitInt::new(
            &executor_names.len().to_string(),
            proc_macro2::Span::call_site(),
        );
        defs.push(quote! {
            #[::embassy_executor::task(pool_size = #n)]
            async fn #fence(__slot: &'static #cr::SpawnerSlot) {
                __slot.__sv_fence_loop().await
            }
        });
    }
    let dep_targets: HashMap<String, DepTarget<'_>> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Node(n) => Some((
                n.ident.to_string(),
                DepTarget {
                    cfg: &n.cfg,
                    pool: false,
                },
            )),
            Item::Pool(p) => Some((
                p.ident.to_string(),
                DepTarget {
                    cfg: &p.cfg,
                    pool: true,
                },
            )),
            Item::Executor(_) => None,
        })
        .collect();

    /// A graph-wide slot — a `shared` slot or a `divisible` budget — that
    /// several items may declare and the graph emits once.
    struct GraphSlotPlan<'a> {
        /// First declaration: supplies the emitted static's ident (span), type
        /// and markers.
        decl: &'a ResourceDecl,
        /// Kinds+type token string every re-declaration must match.
        sig: String,
        /// One entry per declaring site: `None` = unconditional (the slot is
        /// then unconditional too), `Some(pred)` = that site's item and entry
        /// `#[cfg]`s combined.
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
    // Resource name -> the predicate its emitted static is present under, and
    // whether it is a pool's per-member slot array (see `ResourcePlan::slots`).
    let mut slot_presence: HashMap<String, (Option<TokenStream2>, bool)> = HashMap::new();
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
            // A clash names the fragment the declaration came from, if any.
            let suffix = fragment_suffix(match item {
                Item::Node(n) => &n.fragment,
                Item::Pool(p) => &p.fragment,
                Item::Executor(_) => continue,
            });
            // Either declaration order of a take-kind and a graph-wide slot
            // gets the same message.
            let one_kind = |ident: &Ident| {
                syn::Error::new_spanned(
                    ident,
                    format!(
                        "`{ident}` is declared as a take-kind resource and as \
                         `shared`/`divisible`: a name has one kind{suffix}"
                    ),
                )
            };
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
                        return Err(one_kind(&r.ident));
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
                                         must repeat the same kind markers and type{suffix}",
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
                    if shared_plans.iter().any(|(k, _)| *k == key) {
                        return Err(one_kind(&r.ident));
                    }
                    if !taken.insert(key.clone()) {
                        return Err(syn::Error::new_spanned(
                            &r.ident,
                            format!(
                                "duplicate resource name `{}`: resource slots are \
                                 statics and must be unique across the graph (only \
                                 `shared` and `divisible` entries may repeat a \
                                 name){suffix}",
                                r.ident
                            ),
                        ));
                    }
                    slot_presence.insert(key, (pred, matches!(item, Item::Pool(_))));
                }
            }
        }
    }
    // `local` slots are confined to a single executor. Every consumer and the
    // provider must run on that executor; `#[cfg]`s are not checked.
    let mut local_slots: HashSet<String> = HashSet::new();
    {
        let tier = |e: &Option<String>| match e {
            Some(x) => format!("`{x}`"),
            None => "the supervisor's executor".to_string(),
        };
        // slot -> (first declarer, its executor)
        let mut homes: HashMap<String, (String, Option<String>)> = HashMap::new();
        for item in &graph.items {
            let Some((owner, _)) = item_ident_cfg(item) else {
                continue;
            };
            let executor_text = item_executor(item).map(ToString::to_string);
            for r in item_resources(item) {
                let Some(marker) = &r.local else {
                    continue;
                };
                match homes.get(&r.ident.to_string()) {
                    Some((first, home)) if *home != executor_text => {
                        return Err(syn::Error::new_spanned(
                            marker,
                            format!(
                                "`{}` is `local`: every declaration must run on one \
                                 executor (its value is only ever touched from that \
                                 executor), but `{}` runs on {} and `{}` on {}",
                                r.ident,
                                first,
                                tier(home),
                                owner,
                                tier(&executor_text),
                            ),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        homes.insert(
                            r.ident.to_string(),
                            (owner.to_string(), executor_text.clone()),
                        );
                    }
                }
            }
        }
        local_slots.extend(homes.keys().cloned());
        for item in &graph.items {
            let Item::Node(n) = item else {
                continue;
            };
            let executor_text = n.executor.as_ref().map(ToString::to_string);
            for p in &n.provides {
                let Some((first, home)) = homes.get(&p.ident.to_string()) else {
                    continue;
                };
                // A provider the application spawns runs wherever it put it,
                // which the graph cannot see.
                let routed = match &n.source {
                    Some(TaskSource::Shell(_)) => true,
                    Some(TaskSource::Spawn(e)) => matches!(e, Expr::Path(_) | Expr::Call(_)),
                    None => false,
                };
                if !routed {
                    return Err(syn::Error::new_spanned(
                        &p.ident,
                        format!(
                            "`{slot}` is `local`: its provider's executor must be known \
                             to the graph, but `{node}` is spawned by the application. \
                             Give `{node}` a `task:` or a path-form `spawn:` (with \
                             `executor:` if it runs on another tier)",
                            slot = p.ident,
                            node = n.ident,
                        ),
                    ));
                }
                if *home != executor_text {
                    return Err(syn::Error::new_spanned(
                        &p.ident,
                        format!(
                            "`{}` is `local`: its provider must run on the executor \
                             of its declarations (the value is only ever touched \
                             from that executor), but `{}` declares it on {} and \
                             `{}` provides it on {}",
                            p.ident,
                            first,
                            tier(home),
                            n.ident,
                            tier(&executor_text),
                        ),
                    ));
                }
            }
        }
    }
    for (key, plan) in &shared_plans {
        let res = &plan.decl.ident;
        // Present wherever one declarer is: unconditional if any one is.
        let presence = if plan.preds.iter().any(Option::is_none) {
            None
        } else {
            let preds = plan.preds.iter().flatten();
            Some(quote!(any(#(#preds),*)))
        };
        let cfg_attr = presence.as_ref().map(|p| quote!(#[cfg(#p)]));
        slot_presence.insert(key.clone(), (presence, false));
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
        let local = plan.decl.local.is_some();
        let slot_ty = if local {
            let slot = &helpers.local_slot;
            quote!(#slot<#ty>)
        } else {
            quote!(#cr::ResourceSlot<#ty>)
        };
        let mut doc = format!(
            "Shared (fan-out) resource slot declared by `{}` (generated by \
             `supervisor_graph!`). `provide()` the `Copy` handle before \
             `Supervisor::start`; every consumer's glue copies it out with \
             `get()`, so the slot STAYS FILLED — re-`provide()` only to replace \
             the handle (e.g. after rebuilding the underlying driver).",
            plan.owners.join("`, `"),
        );
        let gate_def = if local {
            doc.push_str(LOCAL_SLOT_DOC);
            let local_gate = &helpers.local_gate;
            let lgate = local_gate_static(res);
            quote! {
                #cfg_attr
                #[doc(hidden)]
                #[allow(non_upper_case_globals)]
                pub static #lgate: #local_gate<#ty> = #local_gate(&#res);
            }
        } else {
            quote!()
        };
        defs.push(quote! {
            #cfg_attr
            #[doc = #doc]
            pub static #res: #slot_ty = <#slot_ty>::new();
            #gate_def
        });
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
            let stem = d.stem();
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
        slots: slot_presence,
        local: local_slots,
        claim_bases,
        veto_bases,
    };

    // Private helper names carry the item's index (and the graph's name), so no
    // two items, in this graph or another graph of the module, share one. The
    // name's length and a `g` lead a named tag: `A` + `B_2_C` and `A_1_B` + `C`
    // stay apart, and an unnamed tag always has `_` right after its digits.
    let item_tag = |idx: usize, owner: &Ident| match &graph.name {
        None => format!("{idx}_{}", owner.unraw()),
        Some(g) => {
            let g = g.unraw().to_string();
            format!("{}g{g}_{idx}_{}", g.len(), owner.unraw())
        }
    };
    for (idx, item) in graph.items.iter().enumerate() {
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
                let (def, slot) = emit_node(
                    n,
                    &item_tag(idx, &n.ident),
                    &cr,
                    &spawn_fn,
                    &dep_targets,
                    &helpers,
                    &observe,
                    &plan,
                    f,
                )?;
                defs.push(def);
                slots.push(slot);
            }
            Item::Executor(x) => {
                let (cfg, ident) = (&x.cfg, &x.ident);
                let fence = &helpers.fence;
                defs.push(quote! {
                    #(#cfg)*
                    pub static #ident: #cr::SpawnerSlot = #cr::SpawnerSlot::new().with_fence(
                        (|__sp: ::embassy_executor::SendSpawner,
                          __slot: &'static #cr::SpawnerSlot| {
                            __sp.spawn(#fence(__slot)?);
                            ::core::result::Result::Ok(())
                        }) as #cr::FenceSpawn,
                    );
                });
            }
            Item::Pool(p) => {
                if f.pool {
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
                    let (pool_defs, pool_entry, pool_slots) = emit_pool(
                        p,
                        &item_tag(idx, &p.ident),
                        &cr,
                        &spawn_fn,
                        &dep_targets,
                        &helpers,
                        &observe,
                        &plan,
                        f,
                    )?;
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

    let pools_field = if f.pool && !pool_entries.is_empty() {
        quote!( .with_pools(&[ #(#pool_entries),* ]) )
    } else {
        quote!()
    };

    // The hook bodies are the supervisor's (`__sv_trace_hooks!`): which executor
    // hook API they speak is decided by that crate's build, not this one's.
    let trace_hooks = if f.trace_hooks && graph.name.is_none() {
        quote!( #cr::__sv_trace_hooks!(); )
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
        None => Ident::new("__SV_DEPS", proc_macro2::Span::call_site()),
    };
    let graph_ref_ident = helpers.graph_ref.clone();
    let (graph_ref_def, graph_ref_field) = if f.graph_ref {
        // `trace-self`: the supervisor's own node gets a handle the graph owns.
        let (self_def, with_self) = if f.trace_self {
            let self_handle = &helpers.self_handle;
            (
                quote!( static #self_handle: #cr::TaskHandle = #cr::TaskHandle::new(); ),
                quote!( .with_self_handle(&#self_handle) ),
            )
        } else {
            (quote!(), quote!())
        };
        (
            quote! {
                #self_def
                static #graph_ref_ident: #cr::GraphRef =
                    #cr::GraphRef::new(&#nodes_ident) #with_self;
            },
            quote!( .with_graph_ref(&#graph_ref_ident) ),
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
        // its builder chain below; the node count is `.nodes.len()`.
        static #nodes_ident: [::core::option::Option<&'static #cr::TaskNode>; #m] = [ #(#all_entries),* ];
        #deps_def
        #graph_ref_def

        #[allow(non_camel_case_types)]
        pub type #topo_alias = #topo_ty;

        pub static #graph_ident: #cr::Graph<#m, #topo_alias> =
            #cr::Graph::new(&#nodes_ident, #topo_val) #pools_field #graph_ref_field;

        #trace_hooks

        #deferred
    })
}

/// Derives a fn's read and write coupling tables from the supervisor verbs it
/// calls through its `&'static TaskNode` parameter (`put`, `get`, `writer`,
/// `reader`, the `beat_*` forms), emits them as two flash statics beside the
/// fn, and rewrites each call to carry its table entry. A node binds the
/// tables with `discover` (its `task:` fn) or `dataflow: [path::to_fn]`.
///
/// The arguments register verbs of your own, by direction:
/// `#[dataflow(read(subscribe), write(publish))]`. Needs the
/// `embassy-supervisor` feature `dataflow`. Goes on a free fn, above
/// `#[embassy_executor::task]`; the other order, or a method, is a compile
/// error.
#[proc_macro_attribute]
pub fn dataflow(args: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    match dataflow_expand(args.into(), item.clone(), &ENABLED) {
        Ok(out) => out.into(),
        Err(e) => dataflow_fallback(item, e, &ENABLED).into(),
    }
}

/// Adopts a module of `#[dataflow]` fns as one entry: concatenates every
/// member's tables into one pair of statics named `BUNDLE`, or the name given
/// as the argument (`#[dataflow_bundle(NAME)]`), so
/// `dataflow: [crate::api::BUNDLE]` binds them all.
///
/// Needs the `embassy-supervisor` feature `dataflow` and an inline
/// `mod x { .. }`. A member's own `#[cfg]` gates every entry it contributes,
/// and each member follows the `#[dataflow]` rules, including going above
/// `#[embassy_executor::task]`.
#[proc_macro_attribute]
pub fn dataflow_bundle(args: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    match bundle_expand(args.into(), item.clone(), &ENABLED) {
        Ok(out) => out.into(),
        Err(e) => {
            let err = e.into_compile_error();
            quote!( #item #err ).into()
        }
    }
}

/// A failed `#[dataflow]` keeps its fn, so callers still resolve it, and for
/// a free fn emits the two tables a `discover` or `dataflow:` binding names,
/// empty, so the one error does not cascade into "cannot find" errors there.
/// Without the `dataflow` feature no binding can name them (and `Coupling`
/// may not exist), so none are emitted.
fn dataflow_fallback(item: TokenStream2, e: syn::Error, features: &Features) -> TokenStream2 {
    let err = e.into_compile_error();
    let tables = match syn::parse2::<syn::ItemFn>(item.clone()) {
        Ok(f) if features.dataflow && f.sig.receiver().is_none() => {
            let cr = quote!(::embassy_supervisor);
            let reads = format_ident!("__SV_DATAFLOW_READS_{}", f.sig.ident);
            let writes = format_ident!("__SV_DATAFLOW_WRITES_{}", f.sig.ident);
            quote! {
                #[doc(hidden)]
                #[allow(non_upper_case_globals)]
                pub static #reads: [#cr::Coupling; 0] = [];
                #[doc(hidden)]
                #[allow(non_upper_case_globals)]
                pub static #writes: [#cr::Coupling; 0] = [];
            }
        }
        _ => quote!(),
    };
    quote!( #item #err #tables )
}

fn bundle_expand(
    args: TokenStream2,
    item: TokenStream2,
    features: &Features,
) -> SynResult<TokenStream2> {
    use quote::ToTokens;

    let mut m: syn::ItemMod = syn::parse2(item)?;
    if !features.dataflow {
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
        let Some((attr, _)) = f
            .attrs
            .iter()
            .find_map(embassy_supervisor_syntax::dataflow_attr)
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

fn derived_prefix_expr(entries: &[DerivedEntry]) -> TokenStream2 {
    let attrs: Vec<Vec<Attribute>> = entries
        .iter()
        .map(|e| match derived_predicate(&e.alts) {
            None => Vec::new(),
            Some(pred) => vec![syn::parse_quote!(#[cfg(#pred)])],
        })
        .collect();
    cfg_aware_len(attrs.iter())
}

fn derived_entry_tokens(cr: &TokenStream2, e: &DerivedEntry) -> TokenStream2 {
    let (path, target) = (&e.path, &e.target);
    let plain = quote!( #cr::Coupling::new(#path, & #target) );
    match derived_predicate(&e.alts) {
        None => plain,
        Some(pred) => quote!( #[cfg(#pred)] #plain ),
    }
}

fn dataflow_expand(
    args: TokenStream2,
    item: TokenStream2,
    features: &Features,
) -> SynResult<TokenStream2> {
    use quote::ToTokens;

    let mut f: syn::ItemFn = syn::parse2(item)?;
    if !features.dataflow {
        return Err(syn::Error::new_spanned(
            &f.sig.ident,
            "`#[dataflow]` requires the `dataflow` feature \
             (embassy-supervisor feature `dataflow`) — it derives this \
             fn's coupling tables from the supervisor verbs it calls",
        ));
    }
    // `#[embassy_executor::task]` above `#[dataflow]` expands first: it moves
    // the body into an inner fn and hands this attribute its spawner, whose
    // body calls no verb.
    if let syn::ReturnType::Type(_, ty) = &f.sig.output
        && ty.to_token_stream().to_string().contains("SpawnToken")
    {
        return Err(syn::Error::new_spanned(
            &f.sig.ident,
            "put `#[dataflow]` above `#[embassy_executor::task]`: here the task \
             macro has already replaced this fn's body with its spawner, so no \
             access can be derived",
        ));
    }
    if let Some(receiver) = f.sig.receiver() {
        return Err(syn::Error::new_spanned(
            receiver,
            "`#[dataflow]` goes on free fns: the tables it derives are module statics",
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
            if !features.liveness
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
                "open" | "lease" if !features.data_deps => Some("`data-deps` feature"),
                "veto" if !features.veto => Some("`veto` feature"),
                "retire" if !(features.data_deps && features.readiness) => {
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

/// Declares a task graph: per node its config, handle and `TaskNode` statics,
/// the pools, the resource and executor slots, and the `GRAPH` static
/// (renamed by `name:`) that `Supervisor::new` takes. The dependency order is
/// sorted at expansion, and anything structurally wrong (a cycle, an unknown
/// name, a duplicate, a clause whose feature is off) is a compile error. A
/// clause that needs an `embassy-supervisor` feature says so in its error.
///
/// Three item kinds, each one `;`-terminated line of comma-separated clauses
/// in any order:
///
/// ```text
/// supervisor_graph! {
///     name: IDENT;                        // optional first item: rename the GRAPH static
///     executor NAME;                      // a runtime-filled spawner slot (other core / IRQ tier)
///     default executor NAME;              // ..and the tier every task:/spawn:-fn item without executor: inherits
///
///     node NAME = Mode,                   // Mode: Terminate | Pause | OnDemand
///         deps: [A, POOL, NET ready]      // bring-up order; `ready` also waits for set_ready()
///         , task: worker                  //   or `spawn: task_fn`; omit both = app-spawned
///         , resources: [R: Type, ..]      //   owned values handed over at spawn (kinds: local/shared/consume)
///         , provides: [R, ..]             //   slots this task fills at runtime, cleared at its shutdown ack
///         , exit: Type                    //   capture the worker's return value
///         , state: Type = expr            //   per-activation boxed state, freed on exit
///         , state: zeroed Type            //   same, allocated zero-filled (Type: Zeroable)
///         , cancel                        //   shell owns the shutdown race; worker takes no node
///         , reads: [crate::SIG, ..]       //   declared dataflow; entry marker: observed
///         , writes: [crate::TRIP veto]    //   a contributor slot of a VetoGate (feature veto)
///         , writes: [crate::SIG observed beat] //     `observed beat` marks the heartbeat write (polled)
///         , discover                      //   or: bind the tables the task fn's #[dataflow] derived
///                                         //     (a list beside it may only add markers)
///         , dataflow: [crate::setter]     //   adopt an accessor fn's #[dataflow] tables
///         , beat_timeout: MS , ready_on_write //   every value-level clause (the timeouts, beats,
///                                         //     ready_on_write, disabled, discover, enabled_if:,
///                                         //     restart_budget:) takes a #[cfg(...)] gate
///         , beat_window: N                //   1..=255 consecutive stale sweeps before a report
///         , enabled_if: crate::settings::lora_on //   fn() -> bool; false at start() latches disabled
///         , restart_budget: MAX in MS     //   restart() refuses the MAX+1th inside the window
///         , executor: NAME , slot_timeout: MS , ack_timeout: MS , disabled;
///
///     pool NAME = [Mode, ..],             // one Mode per member, floor first
///         deps: [..], task: worker,
///         resources: [..],                //   take kinds become per-member slot arrays
///         policy: DeferredShrink::new(..),// scaling policy (required)
///         min: EXPR, max: EXPR            //   required; any order, like every clause
///         , cancel;                       //   same flag, applied to every member
/// }
/// ```
///
/// Use it as `embassy_supervisor::supervisor_graph!`; the
/// [DSL chapter](https://docs.rs/embassy-supervisor/latest/embassy_supervisor/guide/dsl/index.html)
/// of the `embassy-supervisor` guide is the full reference.
#[proc_macro]
pub fn supervisor_graph(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as GraphSpec);
    expand(graph, &ENABLED)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declares a named slice of a graph that a compose site assembles:
/// `supervisor_fragment! { name: NET_FRAG; node NET = Terminate, task: net_task; }`.
///
/// The items are checked for syntax here, with their own spans, and the macro
/// emits a `#[macro_export]` relay named after the fragment;
/// `embassy_supervisor::compose_graph!` feeds every listed relay into one
/// `supervisor_graph!` expansion, where names resolve and every static is
/// emitted. A fragment cannot set what belongs to the whole graph: a `name:`
/// in its body, an `observe` default or a `default executor`. Needs no
/// feature beyond those its items use.
#[proc_macro]
pub fn supervisor_fragment(input: TokenStream) -> TokenStream {
    fragment_expand(input.into(), &ENABLED)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn fragment_expand(input: TokenStream2, f: &Features) -> SynResult<TokenStream2> {
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
    let mut spec_parsed = syn::parse2::<GraphSpec>(substituted)?;
    // A body `name:`, an `observe` default or a default executor would set it
    // for the whole composed graph.
    check_fragment_body(&spec_parsed)?;
    // Ungated rejections surface at the fragment; the compose site gates the
    // relayed items again and emits the deferred ones.
    gate::gate(&mut spec_parsed, f)?;

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
mod tests;

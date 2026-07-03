//! Proc-macro for `embassy-supervisor`.
//!
//! `supervisor_graph!` is the **single source** of a task graph: it declares the
//! nodes (and an optional elastic pool), generates their `static`s, and computes
//! the topological order at **compile time**. A dependency cycle is a compile
//! error; an unknown dependency name is a compile error.
//!
//! Surface (each item may be `#[cfg(...)]`-prefixed):
//! ```text
//! node NAME = Mode, deps: [A, B], spawn: <spawn>[, executor: EXEC][, disabled];
//! node NAME = Mode, deps: [A];                 // no `spawn:` => a parked node the app spawns
//! executor EXEC;                               // runtime-filled SendSpawner slot
//! pool NAME = [Mode, ..], deps: [A][, executor: EXEC], spawn: <fn>,
//!     policy: [<Ty> =] <expr>, min: N, max: M;
//! ```
//! `executor EXEC;` emits a `pub static EXEC: SpawnerSlot`; the app fills it with a
//! `SendSpawner` (`InterruptExecutor::start`, `Spawner::make_send`) before
//! `Supervisor::start`, and nodes carrying `executor: EXEC` spawn through it instead
//! of the supervisor's own executor (their futures must be `Send`; an unfilled slot
//! fails the spawn with `SpawnError::Busy`).
//! A pool is emitted as `ElasticPool<P>`, so the macro needs the policy type `P`. By
//! default it derives `P` from a `Ty::new(..)`-shaped `policy:` value (e.g.
//! `DeferredShrink::new(..)` => `P = DeferredShrink`). Give `policy: <Ty> = <expr>` to
//! state `P` explicitly when the value isn't that shape — a const, a free fn, a builder
//! chain (`X::new(..).with(..)`), or a qualified path.
//! `spawn:` takes a path or a partial call to a task fn taking the node **first**
//! (`spawn: f` => `s.spawn(f(&NAME)?)`; `spawn: f(a)` => `s.spawn(f(&NAME, a)?)`), or,
//! for a node, a closure / ready spawn fn emitted verbatim (for anything that doesn't
//! fit that shape). A pool's `spawn:` is the same path/partial-call form with `&POOL[j]`
//! injected first, via a generated `spawn_<pool>::<j>` glue fn; a pool has no closure
//! form (members are instantiated per index).
//!
//! A graph holds at most **256 node slots** (including pool members): all graph
//! indices are `u8`, and the macro rejects a larger declaration at expansion.
//!
//! Nodes, pools, and individual deps may carry `#[cfg(...)]` attributes. A
//! proc-macro can't evaluate `cfg`, so the node array is a fixed-length
//! `[Option<&TaskNode>; M]` over all declared slots (each entry `Some`/`None` via a
//! cfg-expression), the dep table is cfg-aware per-dep, and the order runs through
//! `topo_sort_const` at const-eval (after cfg). Absent nodes are skipped at runtime.
//!
//! Generated items (at the call site): one `pub static` per `node`, a `[TaskNode; K]`
//! array + `spawn_<pool>` glue fn + `<POOL>_POOL` `ElasticPool` per `pool`, plus a
//! single `pub static GRAPH: Graph<M>` bundling the node slots, the dependency table,
//! the topological order, and (with the `pool` feature) the pools — pass `&GRAPH` to
//! `Supervisor::new`. The backing tables are private; read them through `GRAPH.nodes`
//! / `GRAPH.deps` / `GRAPH.order` / `GRAPH.pools` (node count is `GRAPH.nodes.len()`).
//!
//! With the supervisor's `trace` feature (forwarded here) the generated spawn glue
//! also captures each `SpawnToken`'s task id into its node (`set_task_id`), and
//! `trace-names` stamps the node name into the task Metadata. With `trace-hooks`
//! the macro additionally defines the seven `_embassy_trace_*` hook symbols at the
//! declaration site (the supervisor crate is `forbid(unsafe_code)` and cannot),
//! forwarding to the supervisor's `trace` recorders — requires an edition-2024
//! consumer, and exactly one graph declaration (or hook set) per binary.
//!
//! Types are referenced absolutely (`::embassy_supervisor::…`), so the consuming
//! crate must depend on `embassy-supervisor` under its real name (not aliased).

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::HashMap;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, Ident, LitInt, Meta, Path, Result as SynResult, Token, Type, bracketed,
};

mod kw {
    syn::custom_keyword!(node);
    syn::custom_keyword!(pool);
    syn::custom_keyword!(deps);
    syn::custom_keyword!(spawn);
    syn::custom_keyword!(policy);
    syn::custom_keyword!(min);
    syn::custom_keyword!(max);
    syn::custom_keyword!(disabled);
    syn::custom_keyword!(executor);
}

/// A dependency reference: a node ident, optionally `#[cfg(...)]`-gated.
#[derive(Clone)]
struct Dep {
    cfg: Vec<Attribute>,
    ident: Ident,
}

/// `deps: [a, #[cfg(feature = "x")] b, …]`
fn parse_dep_list(input: ParseStream) -> SynResult<Vec<Dep>> {
    let content;
    bracketed!(content in input);
    let mut deps = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let ident: Ident = content.parse()?;
        deps.push(Dep { cfg, ident });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(deps)
}

/// `[Terminate, OnDemand, …]` — the bracketed mode list.
fn parse_mode_list(input: ParseStream) -> SynResult<Vec<Ident>> {
    let content;
    bracketed!(content in input);
    let punct = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
    Ok(punct.into_iter().collect())
}

struct NodeItem {
    cfg: Vec<Attribute>,
    ident: Ident,
    mode: Ident,
    deps: Vec<Dep>,
    /// `None` = a parked node the app spawns itself (no `spawn:` given).
    spawn: Option<Expr>,
    disabled: bool,
    /// `executor: NAME` — spawn through the named [`SpawnerSlot`] (a
    /// `SendSpawner` the app registers at runtime) instead of the supervisor's
    /// own `Spawner`. `None` = the default executor.
    executor: Option<Ident>,
}

/// `executor NAME;` — declares a `pub static NAME: SpawnerSlot` the application
/// fills with a `SendSpawner` before `Supervisor::start` (an InterruptExecutor
/// tier, core1, ...). Nodes reference it with `executor: NAME`.
struct ExecutorItem {
    cfg: Vec<Attribute>,
    ident: Ident,
}

struct PoolItem {
    cfg: Vec<Attribute>,
    ident: Ident,
    modes: Vec<Ident>,
    deps: Vec<Dep>,
    /// The member task. Either a bare path (`http_task`) or a partial call carrying
    /// extra args (`mcp_server_task(stack())`); the macro spawns member `j` as
    /// `s.spawn(<fn>(&POOL[j] [, extra args])?)` — the node is always the first arg.
    /// No closure form (members are instantiated per index), unlike a node's `spawn:`.
    spawn: Expr,
    /// The scaling policy value, emitted as the `policy:` field of the `ElasticPool`
    /// static. The static is typed `ElasticPool<P>`, so the macro needs the policy
    /// *type* `P`: when `policy_ty` is `None` it derives `P` from this expr via
    /// `policy_type` (requires a `Type::new(..)` shape); when `policy_ty` is `Some`
    /// the caller stated `P` explicitly and this expr can be any value of that type.
    policy: Expr,
    /// Optional explicit policy type from the `policy: <Type> = <expr>` form. `Some`
    /// bypasses `policy_type` derivation, allowing a value the deriver can't handle
    /// (a free fn, a const, a builder chain, a qualified path).
    policy_ty: Option<Type>,
    /// `executor: NAME` — spawn every member through the named [`SpawnerSlot`]
    /// (e.g. a worker pool on the second core, scaled by this core's supervisor).
    executor: Option<Ident>,
    min: LitInt,
    max: LitInt,
}

// Both variants embed a large `syn::Expr` (and `PoolItem` a bit more), so their sizes
// are close but unequal — enough for `large_enum_variant` to flag the gap. This AST is
// parsed once and lives only briefly in a `Vec` during expansion, so boxing a variant
// to shave a few bytes per element buys nothing real; suppress the lint instead of
// paying a heap allocation.
#[allow(clippy::large_enum_variant)]
enum Item {
    Node(NodeItem),
    Pool(PoolItem),
    Executor(ExecutorItem),
}

/// The parsed macro input: the list of `node`/`pool` declarations, in source order.
/// Named `GraphSpec` (not `Graph`) to stay distinct from the *emitted* public type
/// [`embassy_supervisor::Graph`] that `expand` produces as the `GRAPH` static.
struct GraphSpec {
    items: Vec<Item>,
}

impl Parse for GraphSpec {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let mut items = Vec::new();
        while !input.is_empty() {
            let cfg = input.call(Attribute::parse_outer)?;
            if input.peek(kw::node) {
                items.push(Item::Node(parse_node(input, cfg)?));
            } else if input.peek(kw::pool) {
                items.push(Item::Pool(parse_pool(input, cfg)?));
            } else if input.peek(kw::executor) {
                // `executor NAME;` — a runtime-filled SendSpawner slot.
                input.parse::<kw::executor>()?;
                let ident: Ident = input.parse()?;
                input.parse::<Token![;]>()?;
                items.push(Item::Executor(ExecutorItem { cfg, ident }));
            } else {
                return Err(input.error(
                    "expected `node`, `pool`, or `executor` (optionally `#[cfg(...)]`-prefixed)",
                ));
            }
        }
        Ok(GraphSpec { items })
    }
}

// node IDENT = MODE, deps: [..] [, spawn: <expr>] [, disabled];
fn parse_node(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<NodeItem> {
    input.parse::<kw::node>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let mode: Ident = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_dep_list(input)?;

    let mut spawn = None;
    let mut disabled = false;
    let mut executor = None;
    while input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        if input.peek(kw::spawn) {
            input.parse::<kw::spawn>()?;
            input.parse::<Token![:]>()?;
            spawn = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::disabled) {
            input.parse::<kw::disabled>()?;
            disabled = true;
        } else if input.peek(kw::executor) {
            input.parse::<kw::executor>()?;
            input.parse::<Token![:]>()?;
            executor = Some(input.parse::<Ident>()?);
        } else {
            return Err(input.error("expected `spawn:`, `executor:`, or `disabled`"));
        }
    }
    input.parse::<Token![;]>()?;
    Ok(NodeItem {
        cfg,
        ident,
        mode,
        deps,
        spawn,
        disabled,
        executor,
    })
}

// pool IDENT = [MODES], deps: [..][, executor: EXEC], spawn: <fn>, policy: EXPR, min: N, max: M;
fn parse_pool(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<PoolItem> {
    input.parse::<kw::pool>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let modes = parse_mode_list(input)?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_dep_list(input)?;
    input.parse::<Token![,]>()?;
    // Optional `executor: NAME,` — run the whole pool on the named SpawnerSlot's
    // executor (e.g. a worker pool on the second core, scaled from this one).
    let executor = if input.peek(kw::executor) {
        input.parse::<kw::executor>()?;
        input.parse::<Token![:]>()?;
        let ex: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        Some(ex)
    } else {
        None
    };
    // The member task: a path, or a partial call supplying extra args (the macro
    // injects `&POOL[j]` as the first argument in either case).
    input.parse::<kw::spawn>()?;
    input.parse::<Token![:]>()?;
    let spawn: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::policy>()?;
    input.parse::<Token![:]>()?;
    // Optional explicit policy type: `policy: <Ty> = <expr>`. Fork to see if a `Type`
    // is followed by `=`; if so it's an annotation (commit on the real stream + eat the
    // `=`), otherwise rewind and treat the whole thing as the value expr (type derived
    // from it in `emit_pool`). For the common `Ty::new(..)` value the fork parses only a
    // partial type and then sees `(`, not `=`, so it correctly falls back to the derive
    // path — this keeps the bare form working unchanged.
    let policy_ty = {
        let fork = input.fork();
        if fork.parse::<Type>().is_ok() && fork.peek(Token![=]) {
            let ty: Type = input.parse()?;
            input.parse::<Token![=]>()?;
            Some(ty)
        } else {
            None
        }
    };
    let policy: Expr = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::min>()?;
    input.parse::<Token![:]>()?;
    let min: LitInt = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::max>()?;
    input.parse::<Token![:]>()?;
    let max: LitInt = input.parse()?;
    input.parse::<Token![;]>()?;
    Ok(PoolItem {
        cfg,
        ident,
        modes,
        deps,
        spawn,
        policy,
        policy_ty,
        executor,
        min,
        max,
    })
}

/// The node/pool name string: ident lowercased with `_`→`-` (`WIFI_CTRL` → "wifi-ctrl").
fn name_string(ident: &Ident) -> String {
    ident.to_string().to_lowercase().replace('_', "-")
}

/// Build a task-call expression with `node_ref` injected as the **first** argument:
/// a bare path `f` => `f(node_ref)`; a partial call `f(a, b)` => `f(node_ref, a, b)`.
/// The task fn is thus expected to take the node/`&POOL[i]` first, then any extra args.
fn inject_node_call(task: &Expr, node_ref: &TokenStream2) -> SynResult<TokenStream2> {
    match task {
        Expr::Path(_) => Ok(quote!(#task(#node_ref))),
        Expr::Call(c) => {
            let f = &c.func;
            let args = c.args.iter();
            Ok(quote!(#f(#node_ref #(, #args)*)))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "expected a task-fn path or a partial call like `f(extra_args)`",
        )),
    }
}

/// Combine an item's `#[cfg(...)]` attributes into one predicate (`all(..)` if
/// several), used to gate its `GRAPH.nodes` slot to `Some`/`None`. `None` = always present.
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
}

/// The `Option<fn(..)>` spawn expression for a node. `None` (no `spawn:`) is a
/// parked node the app spawns itself. A path or partial call is a task fn taking
/// `&NODE` first (plus any given args); the macro wraps it as
/// `|s| { s.spawn(<task>(&NODE, ..)?); Ok(()) }`. Anything else (a closure, or a
/// ready spawn fn) is emitted verbatim. Every form is cast to `spawn_fn` so it
/// coerces cleanly inside `Option::Some(..)`.
fn node_spawn(
    ident: &Ident,
    spawn: &Option<Expr>,
    executor: &Option<Ident>,
    spawn_fn: &TokenStream2,
) -> SynResult<TokenStream2> {
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
            let call = inject_node_call(e, &quote!(&#ident))?;
            match executor {
                None => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(s));
                    quote!(::core::option::Option::Some(
                        (|s| {
                            #stmts
                            ::core::result::Result::Ok(())
                        }) as #spawn_fn
                    ))
                }
                Some(ex) => {
                    let stmts = spawn_stmts(&call, &quote!(&#ident), &quote!(__sp));
                    quote!(::core::option::Option::Some(
                        (|_s| {
                            let __sp = #ex
                                .get()
                                .ok_or(::embassy_executor::SpawnError::Busy)?;
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

/// The spawn statement(s) for the generated glue. Plain `s.spawn(<call>?)`
/// normally; with the `trace` feature the `SpawnToken` is bound first so its task
/// id can be captured into the node (`set_task_id`) — the id→node mapping the
/// supervisor's `trace` recorders resolve against (in embassy-executor 0.10 the
/// task-fn call returns `Result<SpawnToken, SpawnError>` and `Spawner::spawn`
/// itself is infallible, so the token is available between the two). Under
/// `trace-names` the node's name is also stamped into the task Metadata so
/// external consumers (rtos-trace/SystemView) see names instead of raw ids.
fn spawn_stmts(call: &TokenStream2, node_ref: &TokenStream2, sp: &TokenStream2) -> TokenStream2 {
    if cfg!(feature = "trace") {
        // `adopt` = set_task_id + (under trace-names) Metadata name stamp.
        quote! {
            let __token = #call?;
            (#node_ref).adopt(&__token);
            #sp.spawn(__token);
        }
    } else {
        quote!(#sp.spawn(#call?);)
    }
}

/// Emit a `node`: its `pub static #ident: TaskNode` definition and its `Slot`. The
/// caller assigns the slot index and records the name, so this touches neither.
fn emit_node(
    n: &NodeItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
) -> SynResult<(TokenStream2, Slot)> {
    let ident = &n.ident;
    let cfg = &n.cfg;
    let mode = &n.mode;
    let name = name_string(&n.ident);
    let disabled = n.disabled;
    let spawn = node_spawn(ident, &n.spawn, &n.executor, spawn_fn)?;
    let def = quote! {
        #(#cfg)*
        pub static #ident: #cr::TaskNode =
            #cr::TaskNode::new(#name, #cr::Mode::#mode, #spawn, #disabled);
    };
    let slot = Slot {
        cfg_pred: cfg_predicate(cfg),
        reference: quote!(&#ident),
        deps: n.deps.clone(),
    };
    Ok((def, slot))
}

/// Emit a `pool`: the member `[TaskNode; K]` array, the `spawn_<pool>` glue fn, and
/// the `ElasticPool` static (returned as `defs`, in that emission order), plus the
/// pool-registry entry (for `GRAPH.pools`) and one `Slot` per member (members occupy
/// slots but aren't name-addressable, so no name is recorded).
fn emit_pool(
    p: &PoolItem,
    cr: &TokenStream2,
    spawn_fn: &TokenStream2,
) -> SynResult<(Vec<TokenStream2>, TokenStream2, Vec<Slot>)> {
    let ident = &p.ident;
    let cfg = &p.cfg;
    let lname = name_string(&p.ident);
    let pool_static = format_ident!("{}_POOL", ident);
    let k = p.modes.len();

    // Validate the scaling bounds at expansion time. `base10_parse::<u8>` also
    // rejects values > 255 with a span-attached error (the `ElasticPool` fields are
    // `u8`). `min > max` makes the policy contradict itself; `max > k` is a ceiling
    // the pool can never reach (there are only `k` member slots) — both are
    // declaration bugs, caught here rather than surfacing as odd runtime scaling.
    // `max < k` is allowed (spare declared members below the ceiling), as is
    // `min: 0` (the policy may scale the pool all the way down when idle).
    let min_v: u8 = p.min.base10_parse()?;
    let max_v: u8 = p.max.base10_parse()?;
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

    // Build member `I`'s spawn call from the `spawn:` expr, injecting
    // `&POOL[I]` as the first argument (see `inject_node_call`).
    let call = inject_node_call(&p.spawn, &quote!(&#ident[I]))?;
    // Per-member spawn fn: a generated `spawn_<pool>::<I>` wrapper. Same optional
    // trace capture as a node's closure, against member `I`'s slot. With
    // `executor: NAME` the wrapper ignores the supervisor's `Spawner` and spawns
    // through the named SpawnerSlot (unfilled slot = SpawnError::Busy; member
    // futures must then be `Send`), so a whole worker pool can live on another
    // executor — e.g. the second core — while this core's supervisor scales it.
    let (param, prelude, sp_tokens) = match &p.executor {
        None => (quote!(s), quote!(), quote!(s)),
        Some(ex) => (
            quote!(_s),
            quote! {
                let __sp = #ex.get().ok_or(::embassy_executor::SpawnError::Busy)?;
            },
            quote!(__sp),
        ),
    };
    let pool_spawn_stmts = spawn_stmts(&call, &quote!(&#ident[I]), &sp_tokens);
    let wrapper = format_ident!("spawn_{}", lname);
    let mut defs: Vec<TokenStream2> = Vec::new();
    defs.push(quote! {
        #(#cfg)*
        fn #wrapper<const I: usize>(
            #param: ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError> {
            #prelude
            #pool_spawn_stmts
            ::core::result::Result::Ok(())
        }
    });
    let member_spawn: Vec<TokenStream2> = (0..k).map(|j| quote!(#wrapper::<#j>)).collect();

    let members = p
        .modes
        .iter()
        .zip(&member_spawn)
        .enumerate()
        .map(|(j, (mode, sp))| {
            let nm = format!("{lname}{j}");
            quote! {
                #cr::TaskNode::new(
                    #nm, #cr::Mode::#mode,
                    ::core::option::Option::Some((#sp) as #spawn_fn), false,
                )
            }
        });
    defs.push(quote! {
        #(#cfg)*
        pub static #ident: [#cr::TaskNode; #k] = [ #(#members),* ];
    });

    let member_refs = (0..k).map(|j| quote!(&#ident[#j]));
    let policy = &p.policy;
    // The `ElasticPool<P>` type argument: honor an explicit `policy: <Ty> = ..`
    // annotation, else derive `P` from the constructor expr (`Ty::new(..)` shape).
    let policy_ty = match &p.policy_ty {
        Some(ty) => quote!(#ty),
        None => {
            let path = policy_type(policy)?;
            quote!(#path)
        }
    };
    let (min, max) = (&p.min, &p.max);
    defs.push(quote! {
        #(#cfg)*
        pub static #pool_static: #cr::ElasticPool<#policy_ty> = #cr::ElasticPool {
            nodes: &[ #(#member_refs),* ],
            min: #min,
            max: #max,
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
        })
        .collect();

    Ok((defs, pool_entry, slots))
}

/// Second pass: build the node-slot entries for `GRAPH.nodes` (`Option`, cfg-gated) and
/// the cfg-aware dep-index entries for `GRAPH.deps`. Runs after every slot + name is
/// known, since a dep may forward-reference a node declared later. An unknown dep name
/// is a compile error.
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
        for d in &slot.deps {
            let idx = match names.get(&d.ident.to_string()) {
                Some(&i) => i as u8,
                None => {
                    return Err(syn::Error::new_spanned(
                        &d.ident,
                        format!("unknown dependency `{}` — not a declared node", d.ident),
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

fn expand(graph: GraphSpec) -> SynResult<TokenStream2> {
    let cr = quote!(::embassy_supervisor);
    // The node spawn fn-pointer type. Spawn exprs (closures / const-generic fns) are
    // cast to this so they coerce cleanly inside `Option::Some(..)`.
    let spawn_fn = quote!(
        fn(
            ::embassy_executor::Spawner,
        ) -> ::core::result::Result<(), ::embassy_executor::SpawnError>
    );

    // First pass: emit the statics/glue in declaration order, assign stable slot
    // indices, and record each slot + its raw deps. `names` maps a node ident to its
    // slot index for dep resolution — keyed on the *raw* ident (not the runtime
    // `name_string`), and populated for `node`s only (pool members occupy slots but
    // aren't name-addressable, so a dep on a pool name stays an "unknown dependency").
    let mut defs: Vec<TokenStream2> = Vec::new();
    let mut pool_entries: Vec<TokenStream2> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();

    // Pre-pass: collect the declared `executor NAME;` slots so a node's
    // `executor:` reference can be validated regardless of declaration order.
    let executor_names: Vec<String> = graph
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Executor(x) => Some(x.ident.to_string()),
            _ => None,
        })
        .collect();

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
                // The index is the slot's position, taken *before* the push.
                names.insert(n.ident.to_string(), slots.len());
                let (def, slot) = emit_node(n, &cr, &spawn_fn)?;
                defs.push(def);
                slots.push(slot);
            }
            Item::Executor(x) => {
                let (cfg, ident) = (&x.cfg, &x.ident);
                // A runtime-filled SendSpawner slot: the app registers the
                // executor's spawner before `Supervisor::start`; nodes declared
                // `executor: NAME` spawn through it. Occupies no graph slot.
                defs.push(quote! {
                    #(#cfg)*
                    /// Spawner slot for the graph's `executor:`-annotated nodes
                    /// (generated by `supervisor_graph!`). Fill with
                    /// `SpawnerSlot::set` before `Supervisor::start`.
                    pub static #ident: #cr::SpawnerSlot = #cr::SpawnerSlot::new();
                });
            }
            Item::Pool(p) => {
                // Pools are only meaningful with the supervisor's `pool` feature (which
                // forwards to this crate). Without it, `Graph` has no `pools` field and
                // `ElasticPool` doesn't exist — so refuse a `pool` with a clear message
                // rather than emitting dangling references.
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
                    let (pool_defs, pool_entry, pool_slots) = emit_pool(p, &cr, &spawn_fn)?;
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
    // Every graph index (dep entries, `topo_sort_const`'s queue/order) is a `u8`, so
    // more than 256 slots would silently truncate (`i as u8`) and corrupt the order.
    // 256 slots means max index 255 and max per-node dep count 255 — both fit exactly.
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

    // `Graph.pools` is `#[cfg(feature = "pool")]`; emit that field iff this macro was
    // built with pool support (forwarded from the supervisor's `pool` feature).
    let pools_field = if cfg!(feature = "pool") {
        quote!( pools: &[ #(#pool_entries),* ], )
    } else {
        quote!()
    };

    // embassy-executor's trace hooks (declared `unsafe extern "Rust"` in the
    // executor), defined once here at the graph declaration site — the supervisor
    // crate is `forbid(unsafe_code)` and cannot carry `#[unsafe(no_mangle)]` items.
    // They forward to the supervisor's `trace` recorders. `task_new` and
    // `task_ready_begin` carry nothing the recorders need (the id→node mapping
    // comes from the spawn glue above), so they are no-ops. Exactly one definition
    // of each may exist per binary: enable `trace-hooks` OR write your own set.
    // Requires an edition-2024 consumer (`#[unsafe(no_mangle)]` syntax).
    let trace_hooks = if cfg!(feature = "trace-hooks") {
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

    Ok(quote! {
        #(#defs)*

        // Private backing tables — the application uses `GRAPH`. The topological order
        // and pools are inlined into the `GRAPH` literal below; the node count is
        // `GRAPH.nodes.len()`.
        static NODES: [::core::option::Option<&'static #cr::TaskNode>; #m] = [ #(#all_entries),* ];
        const DEPS: [&'static [u8]; #m] = [ #(#deps_entries),* ];

        /// The compile-time task graph — node slots, dependency table, topological order,
        /// and (with the `pool` feature) the elastic pools. Pass to `Supervisor::new`.
        pub static GRAPH: #cr::Graph<#m> = #cr::Graph {
            nodes: &NODES,
            deps: &DEPS,
            order: #cr::topo_sort_const(&DEPS),
            #pools_field
        };

        #trace_hooks
    })
}

/// Declare a supervised task graph; see the crate docs for the surface syntax.
#[proc_macro]
pub fn supervisor_graph(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as GraphSpec);
    expand(graph)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

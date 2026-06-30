//! Proc-macro for `embassy-supervisor`.
//!
//! `supervisor_graph!` is the **single source** of a task graph: it declares the
//! nodes (and an optional elastic pool), generates their `static`s, and computes
//! the topological `ORDER` at **compile time**. A dependency cycle is a compile
//! error; an unknown dependency name is a compile error.
//!
//! Generated items (at the call site):
//! - one `pub static <NODE>: TaskNode` per `node`, and one
//!   `pub static <POOL>: [TaskNode; K]` + `spawn_<pool>` fn + `<POOL>_POOL`
//!   `ElasticPool` per `pool`;
//! - `pub const NODE_COUNT`, `pub static ALL_NODES`, `pub const DEPS`
//!   (index adjacency), `pub const ORDER = topo_sort_const(&DEPS)`, and
//!   `pub static POOLS`.
//!
//! Types are referenced absolutely (`::embassy_supervisor::…`,
//! `::embassy_executor::…`), so the consuming crate must depend on
//! `embassy-supervisor` under its real name (not aliased) and on `embassy-executor`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::HashMap;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, Ident, LitInt, Path, Result as SynResult, Token, bracketed};

mod kw {
    syn::custom_keyword!(node);
    syn::custom_keyword!(pool);
    syn::custom_keyword!(deps);
    syn::custom_keyword!(spawn);
    syn::custom_keyword!(worker);
    syn::custom_keyword!(policy);
    syn::custom_keyword!(min);
    syn::custom_keyword!(max);
    syn::custom_keyword!(disabled);
}

/// `deps: [a, b, c]` → the bracketed identifier list.
fn parse_ident_list(input: ParseStream) -> SynResult<Vec<Ident>> {
    let content;
    bracketed!(content in input);
    let punct = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
    Ok(punct.into_iter().collect())
}

struct NodeItem {
    ident: Ident,
    mode: Ident,
    deps: Vec<Ident>,
    task: Path,
    disabled: bool,
}

struct PoolItem {
    ident: Ident,
    modes: Vec<Ident>,
    deps: Vec<Ident>,
    worker: Path,
    policy: Expr,
    min: LitInt,
    max: LitInt,
}

enum Item {
    Node(NodeItem),
    Pool(PoolItem),
}

struct Graph {
    items: Vec<Item>,
}

impl Parse for Graph {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let mut items = Vec::new();
        while !input.is_empty() {
            if input.peek(kw::node) {
                items.push(Item::Node(parse_node(input)?));
            } else if input.peek(kw::pool) {
                items.push(Item::Pool(parse_pool(input)?));
            } else {
                return Err(input.error("expected `node` or `pool`"));
            }
        }
        Ok(Graph { items })
    }
}

// node NET = Terminate, deps: [..], spawn: path::to::task [, disabled];
fn parse_node(input: ParseStream) -> SynResult<NodeItem> {
    input.parse::<kw::node>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let mode: Ident = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_ident_list(input)?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::spawn>()?;
    input.parse::<Token![:]>()?;
    let task: Path = input.parse()?;
    let mut disabled = false;
    if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        input.parse::<kw::disabled>()?;
        disabled = true;
    }
    input.parse::<Token![;]>()?;
    Ok(NodeItem {
        ident,
        mode,
        deps,
        task,
        disabled,
    })
}

// pool HTTP = [Terminate, OnDemand, ..], deps: [..], worker: path, policy: <expr>, min: N, max: M;
fn parse_pool(input: ParseStream) -> SynResult<PoolItem> {
    input.parse::<kw::pool>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let modes = parse_ident_list(input)?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::deps>()?;
    input.parse::<Token![:]>()?;
    let deps = parse_ident_list(input)?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::worker>()?;
    input.parse::<Token![:]>()?;
    let worker: Path = input.parse()?;
    input.parse::<Token![,]>()?;
    input.parse::<kw::policy>()?;
    input.parse::<Token![:]>()?;
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
        ident,
        modes,
        deps,
        worker,
        policy,
        min,
        max,
    })
}

/// Extract the policy *type* from a `Type::new(..)` constructor expression so the
/// generated `ElasticPool<Type>` static can be concretely typed.
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
        "pool `policy:` must be a `Type::new(..)` constructor (e.g. `DeferredShrink::new(..)`)",
    ))
}

/// One emitted node, in final index order.
struct Emitted {
    /// `&NET` or `&HTTP[2]` — the entry for `ALL_NODES`.
    reference: TokenStream2,
    /// Dependency node names, resolved to indices in the second pass.
    dep_idents: Vec<Ident>,
}

fn expand(graph: Graph) -> SynResult<TokenStream2> {
    let cr = quote!(::embassy_supervisor);
    let ex = quote!(::embassy_executor);

    let mut defs: Vec<TokenStream2> = Vec::new();
    let mut pool_entries: Vec<TokenStream2> = Vec::new();
    let mut emitted: Vec<Emitted> = Vec::new();
    let mut name_to_index: HashMap<String, usize> = HashMap::new();

    // First pass: emit statics, assign indices, record references + raw deps.
    for item in &graph.items {
        match item {
            Item::Node(n) => {
                let index = emitted.len();
                name_to_index.insert(n.ident.to_string(), index);
                let ident = &n.ident;
                let mode = &n.mode;
                let task = &n.task;
                let disabled = n.disabled;
                let name = n.ident.to_string().to_lowercase();
                defs.push(quote! {
                    pub static #ident: #cr::TaskNode = #cr::TaskNode::new(
                        #name,
                        #cr::Mode::#mode,
                        |s| { s.spawn(#task(&#ident)?); Ok(()) },
                        #disabled,
                    );
                });
                emitted.push(Emitted {
                    reference: quote!(&#ident),
                    dep_idents: n.deps.clone(),
                });
            }
            Item::Pool(p) => {
                let ident = &p.ident;
                let worker = &p.worker;
                let lname = ident.to_string().to_lowercase();
                let spawn_fn = format_ident!("spawn_{}", lname);
                let pool_static = format_ident!("{}_POOL", ident);
                let k = p.modes.len();

                let members = p.modes.iter().enumerate().map(|(j, mode)| {
                    let nm = format!("{lname}{j}");
                    quote! { #cr::TaskNode::new(#nm, #cr::Mode::#mode, #spawn_fn::<#j>, false) }
                });
                defs.push(quote! {
                    pub static #ident: [#cr::TaskNode; #k] = [ #(#members),* ];
                    fn #spawn_fn<const I: usize>(
                        s: #ex::Spawner,
                    ) -> ::core::result::Result<(), #ex::SpawnError> {
                        s.spawn(#worker(&#ident[I])?);
                        Ok(())
                    }
                });

                let member_refs = (0..k).map(|j| quote!(&#ident[#j]));
                let policy = &p.policy;
                let policy_ty = policy_type(policy)?;
                let min = &p.min;
                let max = &p.max;
                defs.push(quote! {
                    pub static #pool_static: #cr::ElasticPool<#policy_ty> = #cr::ElasticPool {
                        nodes: &[ #(#member_refs),* ],
                        min: #min,
                        max: #max,
                        policy: #policy,
                    };
                });
                pool_entries.push(quote!(&#pool_static));

                // Every pool member shares the pool's deps.
                for j in 0..k {
                    emitted.push(Emitted {
                        reference: quote!(&#ident[#j]),
                        dep_idents: p.deps.clone(),
                    });
                }
            }
        }
    }

    let n = emitted.len();

    // Second pass: resolve dep names → indices for the DEPS table.
    let mut deps_entries: Vec<TokenStream2> = Vec::new();
    for e in &emitted {
        let mut idxs: Vec<u8> = Vec::new();
        for d in &e.dep_idents {
            match name_to_index.get(&d.to_string()) {
                Some(&i) => idxs.push(i as u8),
                None => {
                    return Err(syn::Error::new_spanned(
                        d,
                        format!("unknown dependency `{d}` — not a declared node"),
                    ));
                }
            }
        }
        deps_entries.push(quote!(&[ #(#idxs),* ]));
    }

    let all_refs = emitted.iter().map(|e| &e.reference);

    Ok(quote! {
        #(#defs)*

        /// Number of supervised nodes in the graph.
        pub const NODE_COUNT: usize = #n;
        /// All nodes, in declaration order (pool members expanded in place).
        pub static ALL_NODES: [&'static #cr::TaskNode; #n] = [ #(#all_refs),* ];
        /// Per-node dependency indices into `ALL_NODES` (the runtime topology).
        pub const DEPS: [&'static [u8]; #n] = [ #(#deps_entries),* ];
        /// Compile-time topological order (a dependency cycle is a compile error).
        pub const ORDER: [u8; #n] = #cr::topo_sort_const(&DEPS);
        /// Elastic pools to register via `Supervisor::with_pools`.
        pub static POOLS: &'static [&'static dyn #cr::Pool] = &[ #(#pool_entries),* ];
    })
}

/// Declare a supervised task graph; see the crate docs for the surface syntax.
#[proc_macro]
pub fn supervisor_graph(input: TokenStream) -> TokenStream {
    let graph = syn::parse_macro_input!(input as Graph);
    expand(graph)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

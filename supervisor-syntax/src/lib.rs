#![deny(missing_docs)]

//! Parser and AST for the [`embassy-supervisor`](https://docs.rs/embassy-supervisor)
//! task-graph DSL.
//!
//! This crate exists because a `proc-macro` crate type cannot export anything
//! other than macros, so the grammar shared between `embassy-supervisor-macros`
//! and tooling lives here instead. The AST is an internal contract: it is not
//! a stable API and changes whenever the graph syntax does.
//!
//! The parser checks grammatical shape (empty clauses, repeated paths, numeric
//! ranges, and so on). Semantic checks such as duplicate names, missing
//! fragments, or feature-gated constructs are left to callers.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Ident, LitInt, Meta, Result as SynResult, Token, Type, bracketed};

/// Custom keywords used by the supervisor graph DSL.
///
/// These are `syn` keyword tokens for every named clause and marker in a
/// `supervisor_graph!` / `supervisor_fragment!` / `compose_graph!` body.
#[allow(missing_docs)]
pub mod kw {
    syn::custom_keyword!(node);
    syn::custom_keyword!(pool);
    syn::custom_keyword!(deps);
    syn::custom_keyword!(discover);
    syn::custom_keyword!(dataflow);
    syn::custom_keyword!(spawn);
    syn::custom_keyword!(task);
    syn::custom_keyword!(pool_size);
    syn::custom_keyword!(policy);
    syn::custom_keyword!(min);
    syn::custom_keyword!(max);
    syn::custom_keyword!(disabled);
    syn::custom_keyword!(executor);
    syn::custom_keyword!(resources);
    syn::custom_keyword!(provides);
    syn::custom_keyword!(slot_timeout);
    syn::custom_keyword!(ack_timeout);
    syn::custom_keyword!(beat_timeout);
    syn::custom_keyword!(beat_window);
    syn::custom_keyword!(reads);
    syn::custom_keyword!(writes);
    syn::custom_keyword!(observe);
    syn::custom_keyword!(ready_on_write);
    syn::custom_keyword!(exit);
    syn::custom_keyword!(name);
    syn::custom_keyword!(state);
    syn::custom_keyword!(zeroed);
    syn::custom_keyword!(cancel);
    syn::custom_keyword!(fragment);
    syn::custom_keyword!(endfragment);
}

/// How a node's `state:` initial value is specified.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum StateInit {
    /// An explicit expression, e.g. `state: Type = expr;`.
    Expr(Expr),
    /// The `zeroed` shorthand, e.g. `state: Type zeroed;`.
    Zeroed(kw::zeroed),
}

/// A single entry in a `deps: [ ... ]` list.
#[derive(Clone)]
pub struct Dep {
    /// `#[cfg(...)]` attributes attached to this dep.
    pub cfg: Vec<Attribute>,
    /// The name of the dependency node or pool.
    pub ident: Ident,
    /// Present when the dep is marked `ready`.
    pub ready: Option<Ident>,
    /// Present when the dep is marked `bound`.
    pub bound: Option<Ident>,
}

/// Parse a bracketed dependency list of the form `[A, B ready, C ready bound]`.
pub fn parse_dep_list(input: ParseStream) -> SynResult<Vec<Dep>> {
    let content;
    bracketed!(content in input);
    let mut deps = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let ident: Ident = content.parse()?;
        let mut ready: Option<Ident> = None;
        let mut bound: Option<Ident> = None;
        while content.peek(Ident) {
            let marker: Ident = content.parse()?;
            let slot = match () {
                _ if marker == "ready" => &mut ready,
                _ if marker == "bound" => &mut bound,
                _ => {
                    return Err(syn::Error::new_spanned(
                        &marker,
                        format!(
                            "expected `,`, `]`, or a dep marker (`ready`, \
                             `bound`), found `{marker}`"
                        ),
                    ));
                }
            };
            if slot.is_some() {
                return Err(syn::Error::new_spanned(
                    &marker,
                    format!("duplicate `{marker}` marker on this dep"),
                ));
            }
            *slot = Some(marker);
        }
        if let (Some(b), None) = (&bound, &ready) {
            return Err(syn::Error::new_spanned(
                b,
                "`bound` implies `ready` — write `deps: [X ready bound]`",
            ));
        }
        deps.push(Dep {
            cfg,
            ident,
            ready,
            bound,
        });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(deps)
}

/// A single entry in a `reads:` or `writes:` signal list.
#[derive(Clone)]
pub struct SignalDecl {
    /// `#[cfg(...)]` attributes attached to this entry.
    pub cfg: Vec<Attribute>,
    /// The path to the signal static.
    pub path: syn::Path,
    /// An array index, if the entry names one element (`SIGNAL[i]`).
    pub index: Option<Expr>,
    /// Present when the entry is marked `observed`.
    pub observed: Option<Ident>,
    /// Present when the entry is marked `beat`.
    pub beat: Option<Ident>,
    /// The accessor expression supplied by `observed via <expr>`.
    pub via: Option<Expr>,
}

impl SignalDecl {
    /// Return the token stream that names the signal target, including any index.
    pub fn target(&self) -> TokenStream2 {
        let path = &self.path;
        match &self.index {
            Some(i) => quote!(#path[#i]),
            None => quote!(#path),
        }
    }

    /// Return a canonical string representation of the signal path.
    pub fn display(&self) -> String {
        let mut out = path_to_string(&self.path);
        if let Some(i) = &self.index {
            out.push('[');
            out.push_str(&quote!(#i).to_string().replace(' ', ""));
            out.push(']');
        }
        out
    }
}

/// Parse a bracketed signal list such as `[S, T observed, U beat]`.
pub fn parse_signal_list(input: ParseStream) -> SynResult<Vec<SignalDecl>> {
    let content;
    bracketed!(content in input);
    let mut decls = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let path: syn::Path = content.parse()?;
        let index = if content.peek(syn::token::Bracket) {
            let idx;
            bracketed!(idx in content);
            Some(idx.parse::<Expr>()?)
        } else {
            None
        };
        let mut observed = None;
        let mut beat = None;
        let mut via = None;
        if content.peek(Ident) {
            let marker: Ident = content.parse()?;
            match marker.to_string().as_str() {
                "observed" => observed = Some(marker),
                "beat" => beat = Some(marker),
                "via" => {
                    return Err(syn::Error::new_spanned(
                        &marker,
                        "`via` supplies the accessor for an `observed` entry: \
                         write `observed via <expr>`",
                    ));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &marker,
                        format!(
                            "expected `,`, `]`, or the `observed`/`beat` markers, \
                             found `{other}`"
                        ),
                    ));
                }
            }
            if beat.is_none() && content.peek(Ident) && content.fork().parse::<Ident>()? == "beat" {
                beat = Some(content.parse::<Ident>()?);
            }
            if content.peek(Ident) {
                let kw: Ident = content.parse()?;
                if kw != "via" {
                    return Err(syn::Error::new_spanned(
                        &kw,
                        format!("expected `beat`, `via <accessor>`, `,` or `]`, found `{kw}`"),
                    ));
                }
                if observed.is_none() {
                    return Err(syn::Error::new_spanned(
                        &kw,
                        "`via` supplies the polling accessor, which only an `observed` \
                         entry has: `beat` only ever qualifies `observed`, and a \
                         heartbeat the body can state is stated by its verb",
                    ));
                }
                via = Some(content.parse::<Expr>()?);
            }
        }
        if let (Some(b), None) = (&beat, &observed) {
            return Err(syn::Error::new_spanned(
                b,
                "a bare `beat` entry is not a declaration: write the heartbeat \
                 at the site that produces it, with `node.beat_put(&SIG, v)` / \
                 `node.beat_writer(&SIG)` or a `node.beat()` call in the body. \
                 `observed beat` is the form for a body the supervisor cannot \
                 see",
            ));
        }
        decls.push(SignalDecl {
            cfg,
            path,
            index,
            observed,
            beat,
            via,
        });
        if !content.is_empty() {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(decls)
}

/// Check that a `reads:` or `writes:` list is non-empty and has no duplicates.
///
/// `clause` is the clause name (`"reads"` or `"writes"`) used in diagnostics.
pub fn check_signal_list<T: quote::ToTokens>(
    tok: &T,
    clause: &str,
    decls: &[SignalDecl],
) -> SynResult<()> {
    if decls.is_empty() {
        return Err(syn::Error::new_spanned(
            tok,
            format!(
                "`{clause}:` must declare at least one signal path — omit the \
                 clause entirely to declare nothing"
            ),
        ));
    }
    for (i, decl) in decls.iter().enumerate() {
        let name = decl.display();
        if decls[..i].iter().any(|prev| prev.display() == name) {
            return Err(syn::Error::new_spanned(
                &decl.path,
                format!("duplicate `{clause}:` entry `{name}`"),
            ));
        }
        let bare = path_to_string(&decl.path);
        if decls[..i].iter().any(|prev| {
            path_to_string(&prev.path) == bare && prev.index.is_some() != decl.index.is_some()
        }) {
            return Err(syn::Error::new_spanned(
                &decl.path,
                format!(
                    "`{bare}` is declared both as a whole array and by element — \
                     pick one. An element-0 reference has the same address as the \
                     array, so nothing downstream can tell them apart"
                ),
            ));
        }
    }
    Ok(())
}

/// Render a `syn::Path` as its `::`-separated string form.
pub fn path_to_string(p: &syn::Path) -> String {
    let mut out = String::new();
    if p.leading_colon.is_some() {
        out.push_str("::");
    }
    for (i, seg) in p.segments.iter().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(&seg.ident.to_string());
    }
    out
}

/// Parse a bracketed list of pool member mode identifiers.
pub fn parse_mode_list(input: ParseStream) -> SynResult<Vec<Ident>> {
    let content;
    bracketed!(content in input);
    let punct = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
    Ok(punct.into_iter().collect())
}

/// How a node or pool member obtains its task future.
#[derive(Clone)]
pub enum TaskSource {
    /// A hand-written `#[embassy_executor::task]` fn referenced by `spawn:`.
    Spawn(Expr),
    /// A worker fn wrapped by a generated shell via `task:`.
    Shell(Expr),
}

/// A single entry in a `resources: [ ... ]` list.
#[derive(Clone)]
pub struct ResourceDecl {
    /// `#[cfg(...)]` attributes attached to this resource.
    pub cfg: Vec<Attribute>,
    /// The resource slot identifier.
    pub ident: Ident,
    /// The Rust type stored in the slot.
    pub ty: Type,
    /// Present when the resource is marked `local`.
    pub local: Option<Ident>,
    /// Present when the resource is marked `consume`.
    pub consume: Option<Ident>,
    /// Present when the resource is marked `shared`.
    pub shared: Option<Ident>,
}

impl ResourceDecl {
    /// Return a human-readable signature string for the resource kind.
    pub fn shared_signature(&self) -> String {
        let ty = &self.ty;
        format!(
            "{}shared {}",
            if self.local.is_some() { "local " } else { "" },
            quote!(#ty)
        )
    }
}

/// Look ahead for a resource kind marker (`local`, `consume`, or `shared`).
///
/// Returns `None` for ordinary identifiers that happen to share a name with a
/// marker, using the following token to decide.
pub fn peek_kind_marker(content: ParseStream) -> Option<Ident> {
    if !content.peek(syn::Ident) {
        return None;
    }
    let fork = content.fork();
    let ident: Ident = fork.parse().ok()?;
    if ident != "local" && ident != "consume" && ident != "shared" {
        return None;
    }
    if fork.is_empty() || fork.peek(Token![,]) || fork.peek(Token![::]) || fork.peek(Token![<]) {
        return None;
    }
    Some(ident)
}

/// Parse a bracketed resource list such as `[SLOT: Type local shared]`.
pub fn parse_resource_list(input: ParseStream) -> SynResult<Vec<ResourceDecl>> {
    let content;
    bracketed!(content in input);
    let mut resources = Vec::new();
    while !content.is_empty() {
        let cfg = content.call(Attribute::parse_outer)?;
        let ident: Ident = content.parse()?;
        content.parse::<Token![:]>()?;
        let mut local: Option<Ident> = None;
        let mut consume: Option<Ident> = None;
        let mut shared: Option<Ident> = None;
        while let Some(marker) = peek_kind_marker(&content) {
            content.parse::<Ident>()?;
            let slot = if marker == "local" {
                &mut local
            } else if marker == "consume" {
                &mut consume
            } else {
                &mut shared
            };
            if slot.is_some() {
                return Err(syn::Error::new_spanned(
                    &marker,
                    format!("duplicate `{marker}` marker"),
                ));
            }
            *slot = Some(marker);
        }
        if let (Some(_), Some(s)) = (&consume, &shared) {
            return Err(syn::Error::new_spanned(
                s,
                "`consume` and `shared` are mutually exclusive — `consume` takes \
                 the single value out for one owner, `shared` copies it out to \
                 any number of consumers",
            ));
        }
        let ty: Type = content.parse()?;
        resources.push(ResourceDecl {
            cfg,
            ident,
            ty,
            local,
            consume,
            shared,
        });
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(resources)
}

/// A function adopted via a `dataflow: [ ... ]` clause.
#[derive(Clone)]
pub struct AdoptedFn {
    /// `#[cfg(...)]` attributes attached to this adoption.
    pub cfg: Vec<Attribute>,
    /// The path to the `#[dataflow]` fn whose accesses are adopted.
    pub path: syn::Path,
}

/// A clause value together with the `#[cfg(...)]` attributes gating it.
#[derive(Clone)]
pub struct Gated<K, V = ()> {
    /// `#[cfg(...)]` attributes gating this clause.
    pub cfg: Vec<Attribute>,
    /// The clause keyword token, for diagnostics.
    pub kw: K,
    /// The clause value.
    pub value: V,
}

/// A single `provides:` entry: a resource-slot name, optionally `#[cfg]`-gated.
#[derive(Clone)]
pub struct ProvideDecl {
    /// `#[cfg(...)]` attributes attached to this entry.
    pub cfg: Vec<Attribute>,
    /// The resource slot this item fills.
    pub ident: Ident,
}

/// A parsed `node NAME = Mode, ...;` declaration.
#[derive(Clone)]
pub struct NodeItem {
    /// `#[cfg(...)]` attributes attached to the node.
    pub cfg: Vec<Attribute>,
    /// The node identifier.
    pub ident: Ident,
    /// The lifecycle mode (`Terminate`, `Pause`, `OnDemand`, ...).
    pub mode: Ident,
    /// The node's `deps:` list.
    pub deps: Vec<Dep>,
    /// The task source, if any (`spawn:` or `task:`).
    pub source: Option<TaskSource>,
    /// The generated task pool size, if specified.
    pub pool_size: Option<LitInt>,
    /// The `resources:` list.
    pub resources: Vec<ResourceDecl>,
    /// The `disabled` marker, if present, with any `#[cfg(...)]` gate.
    pub disabled: Option<Gated<kw::disabled>>,
    /// The named executor, if specified.
    pub executor: Option<Ident>,
    /// The `slot_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub slot_timeout: Option<Gated<kw::slot_timeout, LitInt>>,
    /// The `ack_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub ack_timeout: Option<Gated<kw::ack_timeout, LitInt>>,
    /// The `beat_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub beat_timeout: Option<Gated<kw::beat_timeout, LitInt>>,
    /// The `beat_window:` value, with any `#[cfg(...)]` gate.
    pub beat_window: Option<Gated<kw::beat_window, LitInt>>,
    /// The `ready_on_write` marker, if present, with any `#[cfg(...)]` gate.
    pub ready_on_write: Option<Gated<Ident>>,
    /// The `reads:` signal list.
    pub reads: Vec<SignalDecl>,
    /// The `writes:` signal list.
    pub writes: Vec<SignalDecl>,
    /// The `exit:` result type.
    pub exit: Option<syn::Type>,
    /// The `state:` declaration, if any.
    pub state: Option<(kw::state, syn::Type, StateInit)>,
    /// Whether the node is marked `cancel`.
    pub cancel: bool,
    /// The `discover` marker, if present, with any `#[cfg(...)]` gate.
    pub discover: Option<Gated<kw::discover>>,
    /// Functions adopted via `dataflow:`.
    pub dataflow: Vec<AdoptedFn>,
    /// Slots this node `provides:`, each optionally `#[cfg]`-gated.
    pub provides: Vec<ProvideDecl>,
    /// The `provides` keyword token, for diagnostics.
    pub provides_kw: Option<kw::provides>,
    /// The fragment name this node belongs to, if any.
    pub fragment: Option<String>,
}

/// A parsed `executor NAME;` declaration.
#[derive(Clone)]
pub struct ExecutorItem {
    /// `#[cfg(...)]` attributes attached to the executor.
    pub cfg: Vec<Attribute>,
    /// The executor identifier.
    pub ident: Ident,
}

/// A parsed `pool NAME = [Mode, ...], ...;` declaration.
#[derive(Clone)]
pub struct PoolItem {
    /// `#[cfg(...)]` attributes attached to the pool.
    pub cfg: Vec<Attribute>,
    /// The pool identifier.
    pub ident: Ident,
    /// The allowed member lifecycle modes.
    pub modes: Vec<Ident>,
    /// The pool's `deps:` list.
    pub deps: Vec<Dep>,
    /// The task source (`spawn:` or `task:`).
    pub source: TaskSource,
    /// The scaling policy expression.
    pub policy: Expr,
    /// The optional explicit scaling policy type.
    pub policy_ty: Option<Type>,
    /// The named executor, if specified.
    pub executor: Option<Ident>,
    /// The `resources:` list.
    pub resources: Vec<ResourceDecl>,
    /// The `slot_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub slot_timeout: Option<Gated<kw::slot_timeout, LitInt>>,
    /// The `ack_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub ack_timeout: Option<Gated<kw::ack_timeout, LitInt>>,
    /// The `reads:` signal list.
    pub reads: Vec<SignalDecl>,
    /// The `writes:` signal list.
    pub writes: Vec<SignalDecl>,
    /// The `min:` expression for elastic scaling.
    pub min: Expr,
    /// The `max:` expression for elastic scaling.
    pub max: Expr,
    /// The `state:` declaration, if any.
    pub state: Option<(kw::state, syn::Type, StateInit)>,
    /// Whether pool members are marked `cancel`.
    pub cancel: bool,
    /// The `discover` marker, if present, with any `#[cfg(...)]` gate.
    pub discover: Option<Gated<kw::discover>>,
    /// Functions adopted via `dataflow:`.
    pub dataflow: Vec<AdoptedFn>,
    /// The fragment name this pool belongs to, if any.
    pub fragment: Option<String>,
}

/// A top-level item inside a graph declaration.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum Item {
    /// A supervised node.
    Node(NodeItem),
    /// An elastic pool.
    Pool(PoolItem),
    /// A named executor.
    Executor(ExecutorItem),
}

/// The parsed contents of a `supervisor_graph!` or `supervisor_fragment!` body.
#[derive(Clone)]
pub struct GraphSpec {
    /// The optional `name:` identifier.
    pub name: Option<Ident>,
    /// The default `observe writes:` accessor expression.
    pub observe_writes: Option<(kw::observe, Expr)>,
    /// The default `observe reads:` accessor expression.
    pub observe_reads: Option<(kw::observe, Expr)>,
    /// The nodes, pools, and executors declared in the graph.
    pub items: Vec<Item>,
}

/// Return the resource declarations for an item, if any.
pub fn item_resources(item: &Item) -> &[ResourceDecl] {
    match item {
        Item::Node(n) => &n.resources,
        Item::Pool(p) => &p.resources,
        Item::Executor(_) => &[],
    }
}

/// Iterate over every signal entry declared by an item.
///
/// Yields `(is_write, decl)` pairs, where `is_write` is `true` for writes and
/// `false` for reads.
pub fn item_signal_entries(item: &Item) -> impl Iterator<Item = (bool, &SignalDecl)> {
    let (reads, writes) = match item {
        Item::Node(n) => (&n.reads[..], &n.writes[..]),
        Item::Pool(p) => (&p.reads[..], &p.writes[..]),
        Item::Executor(_) => (&[][..], &[][..]),
    };
    reads
        .iter()
        .map(|d| (false, d))
        .chain(writes.iter().map(|d| (true, d)))
}

/// Return the identifying name and `#[cfg]` attributes of an item, if it has one.
pub fn item_ident_cfg(item: &Item) -> Option<(&Ident, &[Attribute])> {
    match item {
        Item::Node(n) => Some((&n.ident, &n.cfg)),
        Item::Pool(p) => Some((&p.ident, &p.cfg)),
        Item::Executor(_) => None,
    }
}

impl Parse for GraphSpec {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let name = if input.peek(kw::name) && input.peek2(Token![:]) {
            input.parse::<kw::name>()?;
            input.parse::<Token![:]>()?;
            let n: Ident = input.parse()?;
            input.parse::<Token![;]>()?;
            Some(n)
        } else {
            None
        };
        let mut observe_writes: Option<(kw::observe, Expr)> = None;
        let mut observe_reads: Option<(kw::observe, Expr)> = None;
        let mut items = Vec::new();
        let mut current_fragment: Option<String> = None;
        while !input.is_empty() {
            if input.peek(Token![@]) {
                input.parse::<Token![@]>()?;
                if input.peek(kw::fragment) {
                    input.parse::<kw::fragment>()?;
                    current_fragment = Some(input.parse::<Ident>()?.to_string());
                } else if input.peek(kw::endfragment) {
                    input.parse::<kw::endfragment>()?;
                    current_fragment = None;
                } else {
                    return Err(input.error("expected `@fragment NAME;` or `@endfragment;`"));
                }
                input.parse::<Token![;]>()?;
                continue;
            }
            if input.peek(kw::observe) {
                let k = input.parse::<kw::observe>()?;
                let (slot, dir) = if input.peek(kw::writes) {
                    input.parse::<kw::writes>()?;
                    (&mut observe_writes, "writes")
                } else if input.peek(kw::reads) {
                    input.parse::<kw::reads>()?;
                    (&mut observe_reads, "reads")
                } else {
                    return Err(input.error("expected `observe writes:` or `observe reads:`"));
                };
                if slot.is_some() {
                    return Err(syn::Error::new_spanned(
                        k,
                        format!("duplicate `observe {dir}:` default"),
                    ));
                }
                input.parse::<Token![:]>()?;
                *slot = Some((k, input.parse::<Expr>()?));
                input.parse::<Token![;]>()?;
                continue;
            }
            let cfg = input.call(Attribute::parse_outer)?;
            if input.peek(kw::node) {
                let mut n = parse_node(input, cfg)?;
                n.fragment = current_fragment.clone();
                items.push(Item::Node(n));
            } else if input.peek(kw::pool) {
                let mut p = parse_pool(input, cfg)?;
                p.fragment = current_fragment.clone();
                items.push(Item::Pool(p));
            } else if input.peek(kw::executor) {
                input.parse::<kw::executor>()?;
                let ident: Ident = input.parse()?;
                input.parse::<Token![;]>()?;
                items.push(Item::Executor(ExecutorItem { cfg, ident }));
            } else {
                return Err(input.error(
                    "expected `node`, `pool`, `executor`, or `observe` (optionally \
                     `#[cfg(...)]`-prefixed)",
                ));
            }
        }
        Ok(GraphSpec {
            name,
            observe_writes,
            observe_reads,
            items,
        })
    }
}

/// Clauses shared between `node` and `pool` declarations.
///
/// This is a mutable accumulator used while parsing comma-separated clauses.
#[derive(Clone, Default)]
pub struct CommonClauses {
    /// The named executor, if `executor:` was given.
    pub executor: Option<Ident>,
    /// The `spawn:` expression, if any.
    pub spawn: Option<Expr>,
    /// The `task:` expression, if any.
    pub task: Option<(kw::task, Expr)>,
    /// The `resources:` list, if any.
    pub resources: Option<(kw::resources, Vec<ResourceDecl>)>,
    /// The `reads:` signal list.
    pub reads: Vec<SignalDecl>,
    /// The `writes:` signal list.
    pub writes: Vec<SignalDecl>,
    /// The `state:` declaration, if any.
    pub state: Option<(kw::state, syn::Type, StateInit)>,
    /// The `slot_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub slot_timeout: Option<Gated<kw::slot_timeout, LitInt>>,
    /// The `ack_timeout:` value in milliseconds, with any `#[cfg(...)]` gate.
    pub ack_timeout: Option<Gated<kw::ack_timeout, LitInt>>,
    /// Present when the item is marked `cancel`.
    pub cancel: Option<kw::cancel>,
    /// The `discover` marker, if present, with any `#[cfg(...)]` gate.
    pub discover: Option<Gated<kw::discover>>,
    /// The `dataflow:` adoption list, if any.
    pub dataflow: Option<(kw::dataflow, Vec<AdoptedFn>)>,
}

const COMMON_CLAUSE_NAMES: &str = "`task:`, `spawn:`, `executor:`, `resources:`, \
     `reads:`, `writes:`, `discover`, `dataflow:`, `state:`, `slot_timeout:`, \
     `ack_timeout:`, `cancel`";

fn dup_clause<T: quote::ToTokens>(tok: &T, name: &str) -> syn::Error {
    syn::Error::new_spanned(
        tok,
        format!("duplicate `{name}:` clause — one declaration is the contract"),
    )
}

/// The `#[cfg]` attributes' token text, normalized for predicate comparison.
pub fn cfg_text(attrs: &[Attribute]) -> String {
    attrs
        .iter()
        .map(|a| {
            quote::ToTokens::to_token_stream(a)
                .to_string()
                .replace(' ', "")
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Clone, Copy)]
enum ClauseHost {
    Node,
    Pool,
}

fn require_cfg_list(a: &Attribute, what: &str) -> SynResult<()> {
    match &a.meta {
        Meta::List(l) if l.path.is_ident("cfg") => Ok(()),
        _ => Err(syn::Error::new_spanned(
            a,
            format!("only `#[cfg(...)]` attributes may gate a {what}"),
        )),
    }
}

/// Parse an optional run of `#[cfg(...)]` attributes gating the NEXT clause.
fn parse_clause_cfg(input: ParseStream, host: ClauseHost) -> SynResult<Vec<Attribute>> {
    if !input.peek(Token![#]) {
        return Ok(Vec::new());
    }
    let attrs = input.call(Attribute::parse_outer)?;
    for a in &attrs {
        require_cfg_list(a, "clause")?;
    }
    let common =
        input.peek(kw::slot_timeout) || input.peek(kw::ack_timeout) || input.peek(kw::discover);
    let gateable = match host {
        ClauseHost::Node => {
            common
                || input.peek(kw::beat_timeout)
                || input.peek(kw::beat_window)
                || input.peek(kw::ready_on_write)
                || input.peek(kw::disabled)
        }
        ClauseHost::Pool => common,
    };
    if !gateable {
        return Err(input.error(match host {
            ClauseHost::Node => {
                "`#[cfg(...)]` may only gate `slot_timeout:`, `ack_timeout:`, \
                 `beat_timeout:`, `beat_window:`, `ready_on_write`, `disabled`, \
                 or `discover` — gate the whole node, or a single entry inside \
                 `deps:`/`resources:`/`reads:`/`writes:`/`dataflow:`/`provides:`, \
                 for anything structural"
            }
            ClauseHost::Pool => {
                "`#[cfg(...)]` may only gate `slot_timeout:`, `ack_timeout:`, or \
                 `discover` — gate the whole pool, or a single entry inside \
                 `deps:`/`resources:`/`reads:`/`writes:`/`dataflow:`, for \
                 anything structural"
            }
        }));
    }
    Ok(attrs)
}

impl CommonClauses {
    /// Parse the next common clause from `input` into `self`.
    pub fn parse_one(
        &mut self,
        input: ParseStream,
        clause_cfg: &mut Vec<Attribute>,
    ) -> SynResult<bool> {
        if input.peek(kw::spawn) {
            let k = input.parse::<kw::spawn>()?;
            input.parse::<Token![:]>()?;
            if self.spawn.is_some() {
                return Err(dup_clause(&k, "spawn"));
            }
            self.spawn = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::task) {
            let k = input.parse::<kw::task>()?;
            input.parse::<Token![:]>()?;
            if self.task.is_some() {
                return Err(dup_clause(&k, "task"));
            }
            self.task = Some((k, input.parse::<Expr>()?));
        } else if input.peek(kw::executor) {
            let k = input.parse::<kw::executor>()?;
            input.parse::<Token![:]>()?;
            if self.executor.is_some() {
                return Err(dup_clause(&k, "executor"));
            }
            self.executor = Some(input.parse::<Ident>()?);
        } else if input.peek(kw::resources) {
            let k = input.parse::<kw::resources>()?;
            input.parse::<Token![:]>()?;
            if self.resources.is_some() {
                return Err(dup_clause(&k, "resources"));
            }
            self.resources = Some((k, parse_resource_list(input)?));
        } else if input.peek(kw::reads) {
            let k = input.parse::<kw::reads>()?;
            input.parse::<Token![:]>()?;
            if !self.reads.is_empty() {
                return Err(dup_clause(&k, "reads"));
            }
            self.reads = parse_signal_list(input)?;
            check_signal_list(&k, "reads", &self.reads)?;
        } else if input.peek(kw::writes) {
            let k = input.parse::<kw::writes>()?;
            input.parse::<Token![:]>()?;
            if !self.writes.is_empty() {
                return Err(dup_clause(&k, "writes"));
            }
            self.writes = parse_signal_list(input)?;
            check_signal_list(&k, "writes", &self.writes)?;
        } else if input.peek(kw::state) {
            let k = input.parse::<kw::state>()?;
            input.parse::<Token![:]>()?;
            if self.state.is_some() {
                return Err(dup_clause(&k, "state"));
            }
            if input.peek(kw::zeroed) && !input.peek2(Token![=]) {
                let z = input.parse::<kw::zeroed>()?;
                let ty: syn::Type = input.parse()?;
                self.state = Some((k, ty, StateInit::Zeroed(z)));
            } else {
                let ty: syn::Type = input.parse()?;
                input.parse::<Token![=]>()?;
                let init: Expr = input.parse()?;
                self.state = Some((k, ty, StateInit::Expr(init)));
            }
        } else if input.peek(kw::slot_timeout) {
            let k = input.parse::<kw::slot_timeout>()?;
            input.parse::<Token![:]>()?;
            if self.slot_timeout.is_some() {
                return Err(dup_clause(&k, "slot_timeout"));
            }
            let st: LitInt = input.parse()?;
            if st.base10_parse::<u64>()? == 0 {
                return Err(syn::Error::new_spanned(
                    &st,
                    "`slot_timeout:` must be at least 1 (milliseconds)",
                ));
            }
            self.slot_timeout = Some(Gated {
                cfg: core::mem::take(clause_cfg),
                kw: k,
                value: st,
            });
        } else if input.peek(kw::ack_timeout) {
            let k = input.parse::<kw::ack_timeout>()?;
            input.parse::<Token![:]>()?;
            if self.ack_timeout.is_some() {
                return Err(dup_clause(&k, "ack_timeout"));
            }
            let at: LitInt = input.parse()?;
            if at.base10_parse::<u64>()? == 0 {
                return Err(syn::Error::new_spanned(
                    &at,
                    "`ack_timeout:` must be at least 1 (milliseconds)",
                ));
            }
            self.ack_timeout = Some(Gated {
                cfg: core::mem::take(clause_cfg),
                kw: k,
                value: at,
            });
        } else if input.peek(kw::discover) {
            let k = input.parse::<kw::discover>()?;
            if self.discover.is_some() {
                return Err(syn::Error::new_spanned(k, "duplicate `discover` marker"));
            }
            if input.peek(Token![:]) {
                return Err(syn::Error::new_spanned(
                    k,
                    "`discover` takes no argument — the tables come from the \
                     task fn's `#[dataflow]` attribute, sized by its scan",
                ));
            }
            self.discover = Some(Gated {
                cfg: core::mem::take(clause_cfg),
                kw: k,
                value: (),
            });
        } else if input.peek(kw::dataflow) {
            let k = input.parse::<kw::dataflow>()?;
            input.parse::<Token![:]>()?;
            if self.dataflow.is_some() {
                return Err(dup_clause(&k, "dataflow"));
            }
            let content;
            bracketed!(content in input);
            let mut fns: Vec<AdoptedFn> = Vec::new();
            while !content.is_empty() {
                let cfg = content.call(Attribute::parse_outer)?;
                let path: syn::Path = content.parse()?;
                if fns
                    .iter()
                    .any(|f| tokens_text(&f.path) == tokens_text(&path))
                {
                    return Err(syn::Error::new_spanned(
                        &path,
                        "duplicate `dataflow:` fn — one adoption binds its tables",
                    ));
                }
                fns.push(AdoptedFn { cfg, path });
                if content.peek(Token![,]) {
                    content.parse::<Token![,]>()?;
                }
            }
            if fns.is_empty() {
                return Err(syn::Error::new_spanned(
                    k,
                    "`dataflow:` must name at least one `#[dataflow]` fn — omit \
                     the clause entirely to adopt nothing",
                ));
            }
            self.dataflow = Some((k, fns));
        } else if input.peek(kw::cancel) {
            let k = input.parse::<kw::cancel>()?;
            if self.cancel.is_some() {
                return Err(syn::Error::new_spanned(k, "duplicate `cancel` marker"));
            }
            self.cancel = Some(k);
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Resolve the optional `spawn:` / `task:` clauses into a [`TaskSource`].
///
/// Returns an error if both clauses are present.
pub fn task_source(
    spawn: Option<Expr>,
    task: Option<(kw::task, Expr)>,
) -> SynResult<Option<TaskSource>> {
    if let (Some(_), Some((k, _))) = (&spawn, &task) {
        return Err(syn::Error::new_spanned(
            k,
            "`task:` and `spawn:` are mutually exclusive — `spawn:` names a \
             hand-written `#[embassy_executor::task]` fn, `task:` generates one",
        ));
    }
    Ok(match (spawn, task) {
        (Some(e), _) => Some(TaskSource::Spawn(e)),
        (None, Some((_, e))) => Some(TaskSource::Shell(e)),
        (None, None) => None,
    })
}

/// Parse a `node NAME = Mode, ...;` declaration after the leading `node` keyword.
pub fn parse_node(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<NodeItem> {
    input.parse::<kw::node>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let mode: Ident = input.parse()?;
    let mut deps: Option<Vec<Dep>> = None;

    let mut common = CommonClauses::default();
    let mut pool_size = None;
    let mut disabled: Option<Gated<kw::disabled>> = None;
    let mut beat_timeout = None;
    let mut beat_window = None;
    let mut ready_on_write: Option<Gated<Ident>> = None;
    let mut exit: Option<(kw::exit, syn::Type)> = None;
    let mut provides: Vec<ProvideDecl> = Vec::new();
    let mut provides_kw: Option<kw::provides> = None;
    while input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        let mut clause_cfg = parse_clause_cfg(input, ClauseHost::Node)?;
        if common.parse_one(input, &mut clause_cfg)? {
            continue;
        }
        if input.peek(kw::deps) {
            let k = input.parse::<kw::deps>()?;
            input.parse::<Token![:]>()?;
            if deps.is_some() {
                return Err(syn::Error::new_spanned(
                    k,
                    "duplicate `deps:` clause — one list declares them all",
                ));
            }
            deps = Some(parse_dep_list(input)?);
        } else if input.peek(kw::pool_size) {
            input.parse::<kw::pool_size>()?;
            input.parse::<Token![:]>()?;
            pool_size = Some(input.parse::<LitInt>()?);
        } else if input.peek(kw::disabled) {
            let k = input.parse::<kw::disabled>()?;
            if disabled.is_some() {
                return Err(syn::Error::new_spanned(k, "duplicate `disabled` marker"));
            }
            disabled = Some(Gated {
                cfg: clause_cfg,
                kw: k,
                value: (),
            });
        } else if input.peek(kw::ready_on_write) {
            let k = input.parse::<kw::ready_on_write>()?;
            if ready_on_write.is_some() {
                return Err(syn::Error::new_spanned(
                    k,
                    "duplicate `ready_on_write` marker",
                ));
            }
            ready_on_write = Some(Gated {
                cfg: clause_cfg,
                kw: Ident::new("ready_on_write", k.span),
                value: (),
            });
        } else if input.peek(kw::beat_timeout) {
            let k = input.parse::<kw::beat_timeout>()?;
            input.parse::<Token![:]>()?;
            if beat_timeout.is_some() {
                return Err(dup_clause(&k, "beat_timeout"));
            }
            beat_timeout = Some(Gated {
                cfg: clause_cfg,
                kw: k,
                value: input.parse::<LitInt>()?,
            });
        } else if input.peek(kw::beat_window) {
            let k = input.parse::<kw::beat_window>()?;
            input.parse::<Token![:]>()?;
            if beat_window.is_some() {
                return Err(dup_clause(&k, "beat_window"));
            }
            beat_window = Some(Gated {
                cfg: clause_cfg,
                kw: k,
                value: input.parse::<LitInt>()?,
            });
        } else if input.peek(kw::exit) {
            let k = input.parse::<kw::exit>()?;
            input.parse::<Token![:]>()?;
            exit = Some((k, input.parse::<syn::Type>()?));
        } else if input.peek(kw::provides) {
            let k = input.parse::<kw::provides>()?;
            input.parse::<Token![:]>()?;
            let content;
            bracketed!(content in input);
            while !content.is_empty() {
                let entry_cfg = content.call(Attribute::parse_outer)?;
                for a in &entry_cfg {
                    require_cfg_list(a, "`provides:` entry")?;
                }
                let slot: Ident = content.parse()?;
                if provides.iter().any(|p| p.ident == slot) {
                    return Err(syn::Error::new_spanned(
                        &slot,
                        "duplicate `provides:` slot — one entry clears it",
                    ));
                }
                provides.push(ProvideDecl {
                    cfg: entry_cfg,
                    ident: slot,
                });
                if content.peek(Token![,]) {
                    content.parse::<Token![,]>()?;
                }
            }
            if provides.is_empty() {
                return Err(syn::Error::new_spanned(
                    k,
                    "`provides:` must name at least one resource slot — omit \
                     the clause entirely to provide nothing",
                ));
            }
            provides_kw = Some(k);
        } else {
            return Err(input.error(format!(
                "expected {COMMON_CLAUSE_NAMES}, `deps:`, `pool_size:`, \
                 `beat_timeout:`, `beat_window:`, `ready_on_write`, `exit:`, \
                 `provides:`, or `disabled`"
            )));
        }
    }
    input.parse::<Token![;]>()?;
    let CommonClauses {
        executor,
        spawn,
        task,
        resources,
        reads,
        writes,
        state,
        slot_timeout,
        ack_timeout,
        cancel,
        discover,
        dataflow,
    } = common;

    if let Some(bt) = &beat_timeout
        && bt.value.base10_parse::<u64>()? == 0
    {
        return Err(syn::Error::new_spanned(
            &bt.value,
            "`beat_timeout:` must be at least 1 (milliseconds) — omit the clause \
             to leave the node unpoliced",
        ));
    }

    if let (Some(bw), None) = (&beat_window, &beat_timeout) {
        return Err(syn::Error::new_spanned(
            &bw.value,
            "`beat_window:` requires `beat_timeout:` — the window counts \
             consecutive sweeps that found the node past its beat budget",
        ));
    }

    if let Some(bw) = &beat_window
        && !(1..=255).contains(&bw.value.base10_parse::<u64>()?)
    {
        return Err(syn::Error::new_spanned(
            &bw.value,
            "`beat_window:` must be in 1..=255 — omit the clause for the \
             default of 1, which reports on the first stale sweep",
        ));
    }

    if let Some(bt) = &beat_timeout
        && !bt.cfg.is_empty()
    {
        if let Some(bw) = &beat_window
            && cfg_text(&bw.cfg) != cfg_text(&bt.cfg)
        {
            return Err(syn::Error::new_spanned(
                &bw.value,
                "`beat_window:` must carry the same `#[cfg]` predicate as its \
                 `beat_timeout:` — the window counts sweeps of a budget that \
                 gate compiles out",
            ));
        }
        if let Some(row) = &ready_on_write
            && cfg_text(&row.cfg) != cfg_text(&bt.cfg)
        {
            return Err(syn::Error::new_spanned(
                &row.kw,
                "`ready_on_write` must carry the same `#[cfg]` predicate as its \
                 `beat_timeout:` — readiness is asserted by the monitor sweep, \
                 which that gate compiles out",
            ));
        }
    }

    for d in &reads {
        if let Some(b) = &d.beat {
            return Err(syn::Error::new_spanned(
                b,
                "`beat` belongs on a `writes:` entry — a node's heartbeat is \
                 something it produces, not something it consumes",
            ));
        }
    }

    if let Some(k) = &discover {
        for d in reads.iter().chain(writes.iter()) {
            if d.observed.is_none() && d.beat.is_none() {
                return Err(syn::Error::new_spanned(
                    &d.path,
                    "beside `discover`, a `reads:`/`writes:` entry may only add \
                     markers (`observed`, `beat`) to a signal the task fn \
                     already accesses — this one carries none, so it would \
                     declare a coupling the scan did not find. Drop the entry, \
                     or drop `discover` and declare the whole relation",
                ));
            }
        }
        if spawn.is_none() && task.is_none() {
            return Err(syn::Error::new_spanned(
                k.kw,
                "`discover` needs a `task:`/`spawn:` fn to take its tables \
                 from — a parked node has nothing to scan",
            ));
        }
    }

    if let Some(row) = &ready_on_write {
        if !writes
            .iter()
            .any(|w| w.beat.is_some() && w.observed.is_some())
        {
            return Err(syn::Error::new_spanned(
                &row.kw,
                "`ready_on_write` requires an `observed beat` entry in \
                 `writes:` — the sweep's own poll of that write is what asserts \
                 the readiness. A body that beats through its verbs asserts \
                 readiness itself, with `set_ready()` at the same write",
            ));
        }
        if beat_timeout.is_none() {
            return Err(syn::Error::new_spanned(
                &row.kw,
                "`ready_on_write` requires `beat_timeout:` — readiness is \
                 asserted from the monitor sweep, which only visits nodes that \
                 declare a beat budget",
            ));
        }
    }

    if let (Some(ps), None) = (&pool_size, &task) {
        return Err(syn::Error::new_spanned(
            ps,
            "`pool_size:` requires `task:` — a `spawn:` task fn sets its own \
             `#[embassy_executor::task(pool_size = ...)]`",
        ));
    }
    if let Some((k, decls)) = &resources {
        if task.is_none() {
            return Err(syn::Error::new_spanned(
                k,
                "`resources:` requires `task:` — resources are handed to the \
                 generated shell as owned arguments and restored by it; a \
                 `spawn:` task fn manages its own arguments",
            ));
        }
        if decls.is_empty() {
            return Err(syn::Error::new_spanned(
                k,
                "`resources:` must declare at least one `NAME: Type` entry",
            ));
        }
        for (i, d) in decls.iter().enumerate() {
            if decls[..i].iter().any(|prev| prev.ident == d.ident) {
                return Err(syn::Error::new_spanned(
                    &d.ident,
                    format!("duplicate resource name `{}`", d.ident),
                ));
            }
        }
    }
    if let Some(ps) = &pool_size
        && ps.base10_parse::<usize>()? == 0
    {
        return Err(syn::Error::new_spanned(
            ps,
            "`pool_size:` must be at least 1",
        ));
    }
    if let Some((k, _, _)) = &state
        && task.is_none()
    {
        return Err(syn::Error::new_spanned(
            k,
            "`state:` requires `task:` — the generated shell owns the boxed \
             state across the worker call and drops it on exit; a `spawn:` \
             task fn can Box its own state",
        ));
    }
    if let Some((k, _)) = &exit
        && task.is_none()
    {
        return Err(syn::Error::new_spanned(
            k,
            "`exit:` requires `task:` — the generated shell is what captures \
                 the worker's return value; a `spawn:` task fn can provide() into \
                 a slot itself",
        ));
    }
    if let Some(k) = &cancel {
        if task.is_none() {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` requires `task:` — it wraps the generated shell's call \
                 to the worker; a `spawn:` task fn can call \
                 `node.run_cancellable(..)` itself",
            ));
        }
        if mode == "Pause" {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` cannot be combined with `Mode::Pause` — a Pause worker \
                 must survive the stop and park on `wait_resume()`, but `cancel` \
                 drops its future and records an exit; use `Mode::Terminate` (or \
                 `OnDemand`), or drive the pause by hand in the worker",
            ));
        }
    }
    if let (Some((_, decls)), Some(ex)) = (&resources, &executor)
        && let Some(l) = decls.iter().find_map(|d| d.local.as_ref())
    {
        return Err(syn::Error::new_spanned(
            l,
            format!(
                "`local` resources cannot be combined with `executor: {ex}` — a \
                     local slot exists to carry `!Send` values, and a node routed \
                     through a `SpawnerSlot` (`SendSpawner`) must have a `Send` \
                     future; run the node on the supervisor's own executor"
            ),
        ));
    }

    let source = task_source(spawn, task)?;

    Ok(NodeItem {
        cfg,
        ident,
        mode,
        deps: deps.unwrap_or_default(),
        source,
        pool_size,
        disabled,
        executor,
        resources: resources.map(|(_, decls)| decls).unwrap_or_default(),
        slot_timeout,
        ack_timeout,
        beat_timeout,
        beat_window,
        ready_on_write,
        reads,
        writes,
        exit: exit.map(|(_, ty)| ty),
        state,
        cancel: cancel.is_some(),
        discover,
        dataflow: dataflow.map(|(_, f)| f).unwrap_or_default(),
        provides,
        provides_kw,
        fragment: None,
    })
}

/// Parse a `pool NAME = [Mode, ...], ...;` declaration after the leading `pool` keyword.
pub fn parse_pool(input: ParseStream, cfg: Vec<Attribute>) -> SynResult<PoolItem> {
    input.parse::<kw::pool>()?;
    let ident: Ident = input.parse()?;
    input.parse::<Token![=]>()?;
    let modes = parse_mode_list(input)?;

    let mut common = CommonClauses::default();
    let mut deps: Option<Vec<Dep>> = None;
    let mut policy: Option<Expr> = None;
    let mut policy_ty: Option<Type> = None;
    let mut min: Option<Expr> = None;
    let mut max: Option<Expr> = None;

    while input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        let mut clause_cfg = parse_clause_cfg(input, ClauseHost::Pool)?;
        if common.parse_one(input, &mut clause_cfg)? {
            continue;
        }
        if input.peek(kw::deps) {
            let k = input.parse::<kw::deps>()?;
            input.parse::<Token![:]>()?;
            if deps.is_some() {
                return Err(syn::Error::new_spanned(
                    k,
                    "duplicate `deps:` clause — one list declares them all",
                ));
            }
            deps = Some(parse_dep_list(input)?);
        } else if input.peek(kw::policy) {
            let k = input.parse::<kw::policy>()?;
            input.parse::<Token![:]>()?;
            if policy.is_some() {
                return Err(dup_clause(&k, "policy"));
            }
            policy_ty = {
                let fork = input.fork();
                if fork.parse::<Type>().is_ok() && fork.peek(Token![=]) {
                    let ty: Type = input.parse()?;
                    input.parse::<Token![=]>()?;
                    Some(ty)
                } else {
                    None
                }
            };
            policy = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::min) {
            let k = input.parse::<kw::min>()?;
            input.parse::<Token![:]>()?;
            if min.is_some() {
                return Err(dup_clause(&k, "min"));
            }
            min = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::max) {
            let k = input.parse::<kw::max>()?;
            input.parse::<Token![:]>()?;
            if max.is_some() {
                return Err(dup_clause(&k, "max"));
            }
            max = Some(input.parse::<Expr>()?);
        } else if input.peek(kw::exit) {
            let k = input.parse::<kw::exit>()?;
            return Err(syn::Error::new_spanned(
                k,
                "`exit:` is not supported on `pool` — the K members share one shell, \
                 so per-member exit values need per-member storage; use per-node \
                 `exit:` declarations, or have the worker provide() into an \
                 app-declared slot itself",
            ));
        } else {
            return Err(input.error(format!(
                "expected {COMMON_CLAUSE_NAMES}, `deps:`, `policy:`, `min:`, or `max:`"
            )));
        }
    }
    input.parse::<Token![;]>()?;
    let CommonClauses {
        executor,
        spawn,
        task,
        resources,
        reads,
        writes,
        state,
        slot_timeout,
        ack_timeout,
        cancel,
        discover,
        dataflow,
    } = common;
    let source = task_source(spawn, task)?;
    let resources = resources.map(|(_, decls)| decls).unwrap_or_default();

    if let Some(bad) = resources
        .iter()
        .find(|d| d.local.is_some() && d.shared.is_none())
    {
        return Err(syn::Error::new_spanned(
            &bad.ident,
            "`local` is not supported on take-kind `pool` resources (the single-core \
             slot contract + per-member restore is deferred); a `shared local` entry \
             works (one pool-wide fan-out slot), or declare the take-kind `local` \
             resource on a node",
        ));
    }

    let mut absent: Vec<&str> = Vec::new();
    if source.is_none() {
        absent.push("`task:` or `spawn:`");
    }
    if policy.is_none() {
        absent.push("`policy:`");
    }
    if min.is_none() {
        absent.push("`min:`");
    }
    if max.is_none() {
        absent.push("`max:`");
    }
    if !absent.is_empty() {
        return Err(syn::Error::new_spanned(
            &ident,
            format!(
                "`pool {ident}` is missing {} — an elastic pool needs its \
                 dependencies, a member task, a scaling policy, and the \
                 floor/ceiling the policy scales between",
                absent.join(", ")
            ),
        ));
    }
    let source = source.expect("absence checked above");

    if let Some(k) = &cancel {
        if matches!(source, TaskSource::Spawn(_)) {
            return Err(syn::Error::new_spanned(
                k,
                "`cancel` requires `task:` — it wraps the generated shell's call to \
                 the member worker; a `spawn:` member fn can call \
                 `node.run_cancellable(..)` itself",
            ));
        }
        if let Some(m) = modes.iter().find(|m| *m == "Pause") {
            return Err(syn::Error::new_spanned(
                m,
                "`cancel` cannot be combined with a `Pause` member — a Pause worker \
                 must survive the stop and park on `wait_resume()`, but `cancel` \
                 drops its future and records an exit; use `Terminate` (or \
                 `OnDemand`) members, or drive the pause by hand in the worker",
            ));
        }
    }
    if let Some(ex) = &executor
        && let Some(l) = resources.iter().find_map(|d| d.local.as_ref())
    {
        return Err(syn::Error::new_spanned(
            l,
            format!(
                "`local` resources cannot be combined with `executor: {ex}` — a \
                     local slot exists to carry `!Send` values, and a pool routed \
                     through a `SpawnerSlot` (`SendSpawner`) must have `Send` \
                     futures; run the pool on the supervisor's own executor"
            ),
        ));
    }
    Ok(PoolItem {
        cfg,
        ident,
        modes,
        deps: deps.unwrap_or_default(),
        source,
        policy: policy.expect("absence checked above"),
        policy_ty,
        executor,
        resources,
        slot_timeout,
        ack_timeout,
        reads,
        writes,
        min: min.expect("absence checked above"),
        max: max.expect("absence checked above"),
        state,
        cancel: cancel.is_some(),
        discover,
        dataflow: dataflow.map(|(_, f)| f).unwrap_or_default(),
        fragment: None,
    })
}
/// Convert an identifier to a lower-case, hyphenated string.
///
/// Used to derive file-friendly names from graph identifiers.
pub fn name_string(ident: &Ident) -> String {
    ident.to_string().to_lowercase().replace('_', "-")
}

/// Rewrite bare `crate` paths in a fragment body into `$crate`.
///
/// Fragments are parsed in isolation; this prepares them so that
/// [`substitute_dollar_crate`](fn@substitute_dollar_crate) can later resolve
/// them against the real crate path at the compose site.
pub fn normalize_fragment_crate(stream: TokenStream2) -> TokenStream2 {
    use proc_macro2::{Punct, Spacing, TokenStream as TS, TokenTree};
    let mut out = TS::new();
    let mut iter = stream.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Group(g) => {
                let inner = normalize_fragment_crate(g.stream());
                let mut ng = proc_macro2::Group::new(g.delimiter(), inner);
                ng.set_span(g.span());
                out.extend([TokenTree::Group(ng)]);
            }
            TokenTree::Punct(p) if p.as_char() == '$' => {
                out.extend([TokenTree::Punct(p)]);
                if let Some(TokenTree::Ident(i)) = iter.peek()
                    && i == "crate"
                {
                    let i = iter.next().expect("peeked");
                    out.extend([i]);
                }
            }
            TokenTree::Ident(i) if i == "crate" => {
                let mut dollar = Punct::new('$', Spacing::Joint);
                dollar.set_span(i.span());
                out.extend([TokenTree::Punct(dollar), TokenTree::Ident(i)]);
            }
            other => out.extend([other]),
        }
    }
    out
}

/// Replace every `$crate` occurrence in `stream` with `replacement`.
///
/// Used to resolve fragment paths against a placeholder while validating, or
/// the real crate path once a compose site has named it.
pub fn substitute_dollar_crate(stream: TokenStream2, replacement: &TokenStream2) -> TokenStream2 {
    use proc_macro2::{TokenStream as TS, TokenTree};
    let mut out = TS::new();
    let mut iter = stream.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Group(g) => {
                let inner = substitute_dollar_crate(g.stream(), replacement);
                let mut ng = proc_macro2::Group::new(g.delimiter(), inner);
                ng.set_span(g.span());
                out.extend([TokenTree::Group(ng)]);
            }
            TokenTree::Punct(p) if p.as_char() == '$' => {
                if let Some(TokenTree::Ident(i)) = iter.peek()
                    && i == "crate"
                {
                    let span = iter.next().map(|t| t.span()).unwrap_or_else(|| p.span());
                    out.extend(respan(replacement.clone(), span));
                } else {
                    out.extend([TokenTree::Punct(p)]);
                }
            }
            other => out.extend([other]),
        }
    }
    out
}

fn respan(stream: TokenStream2, span: proc_macro2::Span) -> TokenStream2 {
    stream
        .into_iter()
        .map(|mut tt| {
            tt.set_span(span);
            tt
        })
        .collect()
}

/// A single dataflow verb call discovered in a `#[dataflow]` fn body.
#[derive(Clone)]
pub struct VerbCall {
    /// The verb name, e.g. `put` or `open`.
    pub verb: String,
    /// `#[cfg(...)]` predicates inherited from surrounding scopes.
    pub cfgs: Vec<TokenStream2>,
    /// `true` if this is a write, `false` if it is a read.
    pub write: bool,
    /// The expression passed as the signal argument.
    pub target: Expr,
    /// The signal path as a string.
    pub path: String,
}

/// Built-in read verb names recognised by the dataflow scanner.
pub const BUILTIN_READS: &[&str] = &["get", "reader", "open", "lease"];
/// Built-in write verb names recognised by the dataflow scanner.
pub const BUILTIN_WRITES: &[&str] = &["put", "writer", "beat_put", "beat_writer"];

/// Registry of read/write verbs used when scanning a `#[dataflow]` fn.
#[derive(Debug, Clone, Default)]
pub struct VerbTable {
    custom: Vec<(String, bool)>,
}

impl VerbTable {
    /// Return a table containing only the built-in verbs.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Return the direction of a verb, if known.
    ///
    /// `Some(true)` means write, `Some(false)` means read, and `None` means
    /// the verb is not registered.
    pub fn direction(&self, ident: &str) -> Option<bool> {
        if BUILTIN_WRITES.contains(&ident) {
            return Some(true);
        }
        if BUILTIN_READS.contains(&ident) {
            return Some(false);
        }
        self.custom
            .iter()
            .find(|(n, _)| n == ident)
            .map(|(_, w)| *w)
    }

    fn add(&mut self, name: &Ident, write: bool) -> SynResult<()> {
        let text = name.to_string();
        if BUILTIN_READS.contains(&text.as_str()) || BUILTIN_WRITES.contains(&text.as_str()) {
            return Err(syn::Error::new_spanned(
                name,
                format!(
                    "`{text}` is a built-in verb and is always recognised: \
                     registering it would either repeat what the crate already \
                     says or contradict it. Give the new verb its own name"
                ),
            ));
        }
        if self.custom.iter().any(|(n, _)| *n == text) {
            return Err(syn::Error::new_spanned(
                name,
                format!("`{text}` is registered twice; a verb points one way"),
            ));
        }
        self.custom.push((text, write));
        Ok(())
    }
}

impl Parse for VerbTable {
    fn parse(input: ParseStream) -> SynResult<Self> {
        let mut table = VerbTable::builtin();
        while !input.is_empty() {
            let kw: Ident = input.parse().map_err(|_| {
                syn::Error::new(
                    input.span(),
                    "`#[dataflow]` takes verb registrations: \
                     `read(<verb>, ..)` / `write(<verb>, ..)`, naming methods \
                     your own extension trait adds to `TaskNode`. A derived \
                     table states this fn's couplings and nothing else",
                )
            })?;
            let write = match kw.to_string().as_str() {
                "read" => false,
                "write" => true,
                other => {
                    return Err(syn::Error::new_spanned(
                        &kw,
                        format!(
                            "expected `read(..)` or `write(..)`, found `{other}`. \
                             The walker has no type information, so which way a \
                             registered verb points is stated here"
                        ),
                    ));
                }
            };
            let names;
            syn::parenthesized!(names in input);
            let names = names.parse_terminated(Ident::parse, Token![,])?;
            if names.is_empty() {
                return Err(syn::Error::new_spanned(
                    &kw,
                    format!("`{kw}(..)` names no verb"),
                ));
            }
            for name in &names {
                table.add(name, write)?;
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(table)
    }
}

/// Build a [`VerbTable`] from a `#[dataflow(...)]` attribute.
///
/// The attribute list may contain `read(verb, ...)` and `write(verb, ...)`
/// registrations for custom verbs. An empty or bare `#[dataflow]` attribute
/// yields the built-in table.
pub fn verb_table_of(attr: &Attribute) -> VerbTable {
    match &attr.meta {
        Meta::List(list) => syn::parse2(list.tokens.clone()).unwrap_or_default(),
        _ => VerbTable::builtin(),
    }
}

/// Walk a `#[dataflow]` fn body and rewrite recognised verb calls.
///
/// For every call of the form `NODE.verb(&SIGNAL, ...)` where `verb` is
/// registered in `verbs`, `on_call` is invoked with a [`VerbCall`] describing
/// the access. The callback may return a replacement expression for the first
/// argument, or `None` to leave it unchanged.
pub fn rewrite_verb_calls(
    body: TokenStream2,
    node_param: &str,
    verbs: &VerbTable,
    on_call: &mut dyn FnMut(VerbCall) -> SynResult<Option<TokenStream2>>,
) -> SynResult<TokenStream2> {
    rewrite_verb_calls_in(body, node_param, verbs, &[], on_call)
}

fn cfg_attr_predicate(g: &proc_macro2::Group) -> Option<TokenStream2> {
    use proc_macro2::{Delimiter, TokenTree};
    if g.delimiter() != Delimiter::Bracket {
        return None;
    }
    let mut it = g.stream().into_iter();
    match (it.next(), it.next(), it.next()) {
        (Some(TokenTree::Ident(i)), Some(TokenTree::Group(p)), None)
            if i == "cfg" && p.delimiter() == Delimiter::Parenthesis =>
        {
            Some(p.stream())
        }
        _ => None,
    }
}

fn rewrite_verb_calls_in(
    body: TokenStream2,
    node_param: &str,
    verbs: &VerbTable,
    inherited: &[TokenStream2],
    on_call: &mut dyn FnMut(VerbCall) -> SynResult<Option<TokenStream2>>,
) -> SynResult<TokenStream2> {
    use proc_macro2::{Delimiter, Group, TokenTree};

    let toks: Vec<TokenTree> = body.into_iter().collect();
    let mut out = TokenStream2::new();
    let mut i = 0;
    let mut pending: Vec<TokenStream2> = Vec::new();
    let mut current: Vec<TokenStream2> = Vec::new();
    while i < toks.len() {
        if let TokenTree::Punct(p) = &toks[i]
            && p.as_char() == '#'
            && let Some(TokenTree::Group(g)) = toks.get(i + 1)
            && g.delimiter() == Delimiter::Bracket
        {
            if let Some(pred) = cfg_attr_predicate(g) {
                pending.push(pred);
            }
            out.extend([toks[i].clone(), toks[i + 1].clone()]);
            i += 2;
            continue;
        }
        if !pending.is_empty() {
            current.append(&mut pending);
        }
        if matches!(&toks[i], TokenTree::Ident(id) if *id == "fn")
            && matches!(toks.get(i + 1), Some(TokenTree::Ident(_)))
        {
            while i < toks.len() {
                let done = matches!(&toks[i],
                    TokenTree::Group(g) if g.delimiter() == Delimiter::Brace)
                    || matches!(&toks[i], TokenTree::Punct(p) if p.as_char() == ';');
                out.extend([toks[i].clone()]);
                i += 1;
                if done {
                    break;
                }
            }
            current.clear();
            continue;
        }
        let own_receiver = i == 0
            || !matches!(&toks[i - 1], TokenTree::Punct(p)
                if p.as_char() == '.' || p.as_char() == ':');
        let matched = if own_receiver && i + 3 < toks.len() {
            match (&toks[i], &toks[i + 1], &toks[i + 2], &toks[i + 3]) {
                (
                    TokenTree::Ident(n),
                    TokenTree::Punct(dot),
                    TokenTree::Ident(verb),
                    TokenTree::Group(g),
                ) if *n == node_param
                    && dot.as_char() == '.'
                    && g.delimiter() == Delimiter::Parenthesis =>
                {
                    let name = verb.to_string();
                    verbs.direction(&name).map(|write| (name, write))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((verb, write)) = matched {
            let TokenTree::Group(g) = &toks[i + 3] else {
                unreachable!()
            };
            let mut cfgs: Vec<TokenStream2> = inherited.to_vec();
            cfgs.extend(current.iter().cloned());
            let rebuilt = rewrite_first_arg(g, verb, write, node_param, verbs, &cfgs, on_call)?;
            out.extend(toks[i..i + 3].iter().cloned());
            let mut ng = Group::new(Delimiter::Parenthesis, rebuilt);
            ng.set_span(g.span());
            out.extend([TokenTree::Group(ng)]);
            i += 4;
            continue;
        }
        match &toks[i] {
            TokenTree::Group(g) => {
                let mut child = inherited.to_vec();
                child.extend(current.iter().cloned());
                let inner = rewrite_verb_calls_in(g.stream(), node_param, verbs, &child, on_call)?;
                let mut ng = Group::new(g.delimiter(), inner);
                ng.set_span(g.span());
                out.extend([TokenTree::Group(ng)]);
                if g.delimiter() == Delimiter::Brace {
                    let continues = matches!(
                        toks.get(i + 1),
                        Some(TokenTree::Ident(id)) if *id == "else"
                    ) || matches!(
                        toks.get(i + 1),
                        Some(TokenTree::Punct(p)) if matches!(p.as_char(), '.' | '?')
                    );
                    if !continues {
                        current.clear();
                    }
                }
            }
            TokenTree::Punct(p) if matches!(p.as_char(), ';' | ',') => {
                out.extend([toks[i].clone()]);
                current.clear();
            }
            t => out.extend([t.clone()]),
        }
        i += 1;
    }
    Ok(out)
}

fn rewrite_first_arg(
    g: &proc_macro2::Group,
    verb: String,
    write: bool,
    node_param: &str,
    verbs: &VerbTable,
    cfgs: &[TokenStream2],
    on_call: &mut dyn FnMut(VerbCall) -> SynResult<Option<TokenStream2>>,
) -> SynResult<TokenStream2> {
    use proc_macro2::TokenTree;

    let toks: Vec<TokenTree> = g.stream().into_iter().collect();
    let split = toks
        .iter()
        .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
        .unwrap_or(toks.len());
    let (first, rest) = toks.split_at(split);
    let arg: TokenStream2 = first.iter().cloned().collect();
    let target = match syn::parse2::<Expr>(arg.clone()) {
        Ok(Expr::Reference(r))
            if matches!(*r.expr, Expr::Path(_)) || matches!(*r.expr, Expr::Index(_)) =>
        {
            *r.expr
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &arg,
                "the supervisor derives dataflow from the literal path: name \
                 the signal directly (`&path::TO_SIGNAL`, `&ARR[i]`)",
            ));
        }
    };
    let path = tokens_text(&target);
    let replacement = on_call(VerbCall {
        verb,
        write,
        target,
        path,
        cfgs: cfgs.to_vec(),
    })?;
    let mut out = replacement.unwrap_or(arg);
    out.extend(rewrite_verb_calls_in(
        rest.iter().cloned().collect(),
        node_param,
        verbs,
        cfgs,
        on_call,
    )?);
    Ok(out)
}

fn tokens_text<T: quote::ToTokens>(t: &T) -> String {
    t.to_token_stream().to_string().replace(' ', "")
}

/// Find the name of the function argument whose type contains `TaskNode`.
///
/// Returns `None` if no such argument exists.
pub fn node_param(sig: &syn::Signature) -> Option<Ident> {
    sig.inputs.iter().find_map(|arg| match arg {
        syn::FnArg::Typed(t) if tokens_text(&t.ty).contains("TaskNode") => match &*t.pat {
            syn::Pat::Ident(p) => Some(p.ident.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// Return `true` if `attr` is a `#[dataflow]` attribute.
pub fn is_dataflow_attr(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "dataflow")
}

/// One dataflow access discovered by scanning a `#[dataflow]` fn body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The name of the function that contains this access.
    pub func: String,
    /// The verb used, e.g. `put` or `get`.
    pub verb: String,
    /// `true` for writes, `false` for reads.
    pub write: bool,
    /// The signal path accessed, as a string.
    pub path: String,
    /// `#[cfg(...)]` predicates that guard this access.
    pub cfgs: Vec<String>,
}

/// Scan Rust source for `#[dataflow]` functions and append their accesses to `out`.
///
/// This is a textual scan: it looks for calls on the `TaskNode` parameter
/// whose method name is a registered read or write verb, and records the
/// accessed signal path.
pub fn scan_dataflow(src: &str, out: &mut Vec<Access>) {
    let Ok(file) = syn::parse_file(src) else {
        return;
    };
    for item in &file.items {
        scan_item(item, out);
    }
}

fn scan_item(item: &syn::Item, out: &mut Vec<Access>) {
    match item {
        syn::Item::Fn(f) => scan_fn(&f.attrs, &f.sig, &f.block, out),
        syn::Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for i in items {
                    scan_item(i, out);
                }
            }
        }
        syn::Item::Impl(im) => {
            for ii in &im.items {
                if let syn::ImplItem::Fn(f) = ii {
                    scan_fn(&f.attrs, &f.sig, &f.block, out);
                }
            }
        }
        _ => {}
    }
}

fn scan_fn(attrs: &[Attribute], sig: &syn::Signature, block: &syn::Block, out: &mut Vec<Access>) {
    let Some(attr) = attrs.iter().find(|a| is_dataflow_attr(a)) else {
        return;
    };
    let Some(param) = node_param(sig) else {
        return;
    };
    let verbs = verb_table_of(attr);
    let func = sig.ident.to_string();
    let fn_cfgs: Vec<String> = attrs
        .iter()
        .filter_map(|a| match &a.meta {
            syn::Meta::List(l) if l.path.is_ident("cfg") => {
                Some(l.tokens.to_string().replace(' ', ""))
            }
            _ => None,
        })
        .collect();
    let mut seen: Vec<(bool, String)> = Vec::new();
    let _ = rewrite_verb_calls(quote!(#block), &param.to_string(), &verbs, &mut |call| {
        if !seen.contains(&(call.write, call.path.clone())) {
            seen.push((call.write, call.path.clone()));
            out.push(Access {
                func: func.clone(),
                verb: call.verb.clone(),
                write: call.write,
                path: call.path.clone(),
                cfgs: fn_cfgs
                    .iter()
                    .cloned()
                    .chain(call.cfgs.iter().map(|c| c.to_string().replace(' ', "")))
                    .collect(),
            });
        }
        Ok(None)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_dep_marker_rejected() {
        match syn::parse_str::<GraphSpec>("node A = Terminate, deps: [B rdy];") {
            Ok(_) => panic!("unknown marker accepted"),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("dep marker"), "got: {msg}");
                assert!(msg.contains("ready"), "got: {msg}");
                assert!(msg.contains("rdy"), "names the offending token: {msg}");
            }
        }
    }

    #[test]
    fn beat_timeout_zero_rejected() {
        match syn::parse_str::<GraphSpec>("node A = Terminate, deps: [], beat_timeout: 0;") {
            Ok(_) => panic!("zero budget accepted"),
            Err(err) => assert!(
                err.to_string()
                    .contains("`beat_timeout:` must be at least 1"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn beat_window_without_timeout_rejected() {
        match syn::parse_str::<GraphSpec>("node A = Terminate, deps: [], beat_window: 3;") {
            Ok(_) => panic!("orphan window accepted"),
            Err(err) => assert!(
                err.to_string()
                    .contains("`beat_window:` requires `beat_timeout:`"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn beat_window_out_of_range_rejected() {
        for w in ["0", "256", "300"] {
            let src = format!("node A = Terminate, deps: [], beat_timeout: 100, beat_window: {w};");
            match syn::parse_str::<GraphSpec>(&src) {
                Ok(_) => panic!("`beat_window: {w}` accepted"),
                Err(err) => assert!(
                    err.to_string()
                        .contains("`beat_window:` must be in 1..=255"),
                    "got: {err}"
                ),
            }
        }
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], beat_timeout: 100, beat_window: 255;"
            )
            .is_ok(),
            "255 is in range"
        );
    }

    #[test]
    fn ready_on_write_needs_a_heartbeat_source() {
        let nothing = "node A = Terminate, deps: [], beat_timeout: 100, \
             ready_on_write, writes: [crate::X];";
        match syn::parse_str::<GraphSpec>(nothing) {
            Ok(_) => panic!("accepted with no `beat` write"),
            Err(err) => assert!(
                err.to_string()
                    .contains("requires an `observed beat` entry"),
                "got: {err}"
            ),
        }

        let no_budget = "observe writes: it.get();\n\
             node A = Terminate, deps: [], ready_on_write, \
             writes: [crate::X observed beat];";
        match syn::parse_str::<GraphSpec>(no_budget) {
            Ok(_) => panic!("accepted without `beat_timeout:`"),
            Err(err) => assert!(
                err.to_string().contains("requires `beat_timeout:`"),
                "got: {err}"
            ),
        }

        let ok = "observe writes: it.get();\n\
             node A = Terminate, deps: [], beat_timeout: 100, ready_on_write, \
             writes: [crate::X observed beat];";
        assert!(
            syn::parse_str::<GraphSpec>(ok).is_ok(),
            "both halves present"
        );

        let adopted = "node A = Terminate, deps: [], ready_on_write, \
             dataflow: [crate::hb::set_period];";
        match syn::parse_str::<GraphSpec>(adopted) {
            Ok(_) => panic!("adoption accepted as a heartbeat source"),
            Err(err) => assert!(
                err.to_string()
                    .contains("requires an `observed beat` entry"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn dataflow_scan_keys_on_the_node_param() {
        let src = r#"
            #[embassy_supervisor::dataflow]
            async fn worker(node: &'static TaskNode, map: Map) {
                let v = node.get(&PERIOD);
                map.get(&KEY);
                node.put(&OUT, v);
                if v > 0 {
                    node.writer(&crate::stats::HITS[1]).fetch_add(1, O);
                }
                let mut rx = node.reader(&EST).receiver();
                node.put(&OUT, v + 1); // second site, one entry
            }
            async fn unannotated(node: &'static TaskNode) {
                node.put(&IGNORED, 1);
            }
        "#;
        let mut out = Vec::new();
        scan_dataflow(src, &mut out);
        let key: Vec<(&str, bool, &str)> = out
            .iter()
            .map(|a| (a.func.as_str(), a.write, a.path.as_str()))
            .collect();
        assert_eq!(
            key,
            [
                ("worker", false, "PERIOD"),
                ("worker", true, "OUT"),
                ("worker", true, "crate::stats::HITS[1]"),
                ("worker", false, "EST"),
            ],
            "{out:?}"
        );
    }

    #[test]
    fn nested_fns_are_not_walked() {
        let body = quote!({
            node.put(&OUT, 1);
            fn helper(node: &'static TaskNode) {
                node.put(&INNER, 2);
            }
            let f = || node.get(&IN);
        });
        let mut seen = Vec::new();
        let out = rewrite_verb_calls(body, "node", &VerbTable::builtin(), &mut |call| {
            seen.push(call.path.clone());
            Ok(Some(quote!(REPL)))
        })
        .unwrap()
        .to_string()
        .replace(' ', "");
        assert_eq!(seen, ["OUT", "IN"], "the nested fn's access is not ours");
        assert!(
            out.contains("put(&INNER,2)"),
            "and stays unrewritten: {out}"
        );
        assert!(out.contains("get(REPL)"), "the closure's is: {out}");
    }

    /// The walk keys on whatever the fn names its node parameter.
    #[test]
    fn walker_keys_on_the_actual_param_name() {
        let src = r#"
            #[dataflow]
            fn f(n: &'static TaskNode, node: Map) {
                n.put(&OUT, 1);
                node.get(&KEY);
            }
        "#;
        let mut out = Vec::new();
        scan_dataflow(src, &mut out);
        let key: Vec<(&str, bool)> = out.iter().map(|a| (a.path.as_str(), a.write)).collect();
        assert_eq!(key, [("OUT", true)], "{out:?}");
    }

    /// A computed first argument cannot become a compile-time table entry.
    #[test]
    fn dataflow_walker_rejects_a_computed_target() {
        let body = quote!({ node.get(some_binding) });
        let err =
            rewrite_verb_calls(body, "node", &VerbTable::builtin(), &mut |_| Ok(None)).unwrap_err();
        assert!(err.to_string().contains("literal path"), "{err}");
    }

    /// The rewriter's replacement lands as the call's first argument, rest
    /// untouched.
    #[test]
    fn dataflow_walker_replaces_the_first_argument() {
        let body = quote!({
            node.put(&OUT, v);
            node.get(&IN)
        });
        let out = rewrite_verb_calls(body, "node", &VerbTable::builtin(), &mut |call| {
            let k = if call.write { 1u32 } else { 0 };
            Ok(Some(quote!(REPL(#k))))
        })
        .unwrap()
        .to_string()
        .replace(' ', "");
        assert!(out.contains("put(REPL(1u32),v)"), "{out}");
        assert!(out.contains("get(REPL(0u32))"), "{out}");
    }

    /// A registered verb is walked exactly like a built-in one, and an
    /// unregistered method on the node is still left alone: a `#[dataflow]` fn
    /// calls `set_ready()` and `beat()` on its node like any other.
    #[test]
    fn registered_verbs_are_walked_and_others_are_not() {
        let verbs: VerbTable = syn::parse_str("read(subscribe), write(publish, emit)").unwrap();
        let body = quote!({
            node.subscribe(&IN);
            node.publish(&OUT, v);
            node.emit(&LOG, e);
            node.set_ready();
            node.reader(&ALSO);
        });
        let mut seen = Vec::new();
        let out = rewrite_verb_calls(body, "node", &verbs, &mut |call| {
            seen.push((call.verb.clone(), call.write, call.path.clone()));
            Ok(Some(quote!(REPL)))
        })
        .unwrap()
        .to_string()
        .replace(' ', "");
        assert_eq!(
            seen,
            [
                ("subscribe".into(), false, "IN".to_string()),
                ("publish".into(), true, "OUT".to_string()),
                ("emit".into(), true, "LOG".to_string()),
                ("reader".into(), false, "ALSO".to_string()),
            ],
            "registered verbs join the built-ins, direction as declared"
        );
        assert!(out.contains("set_ready()"), "not a verb, untouched: {out}");
    }

    /// The two ways a registration is a mistake rather than an intent, and the
    /// shape errors around them. A bare `#[dataflow]` is an empty argument
    /// list, which is the built-in table.
    #[test]
    fn verb_registration_rejects_its_mistakes() {
        assert!(
            syn::parse_str::<VerbTable>("").is_ok(),
            "bare `#[dataflow]`"
        );

        let cases = [
            ("read(put)", "built-in"),
            ("write(reader)", "built-in"),
            ("read(a), write(a)", "twice"),
            ("read(a, a)", "twice"),
            ("beat(a)", "expected `read(..)` or `write(..)`"),
            ("subscribe", "expected `read(..)` or `write(..)`"),
            ("read()", "names no verb"),
            // Not an ident at all: the arguments are not a marker list, and
            // the error says what they are instead.
            ("42", "verb registrations"),
        ];
        for (src, want) in cases {
            match syn::parse_str::<VerbTable>(src) {
                Ok(_) => panic!("`{src}` accepted"),
                Err(err) => assert!(
                    err.to_string().contains(want),
                    "`{src}`: wanted {want:?}, got: {err}"
                ),
            }
        }
    }

    /// The diagram tool reads the registrations from the same attribute the
    /// build does, so a consumer's verbs reach the diagram with no
    /// configuration channel of their own.
    #[test]
    fn the_scanner_reads_the_registrations_too() {
        let src = r#"
            #[dataflow(read(subscribe), write(publish))]
            async fn entry(node: &'static TaskNode) {
                let rx = node.subscribe(&crate::EST);
                node.publish(&crate::ARMED, true);
                node.put(&crate::OTHER, 1);
            }
        "#;
        let mut out = Vec::new();
        scan_dataflow(src, &mut out);
        let key: Vec<(&str, &str, bool)> = out
            .iter()
            .map(|a| (a.verb.as_str(), a.path.as_str(), a.write))
            .collect();
        assert_eq!(
            key,
            [
                ("subscribe", "crate::EST", false),
                ("publish", "crate::ARMED", true),
                ("put", "crate::OTHER", true),
            ]
        );
    }

    /// The entry markers: `observed`? `beat`? `via <expr>`?, in that order.
    /// `beat` only ever qualifies `observed`; alone it is rejected, because a
    /// body the supervisor can see states its heartbeat at the write.
    #[test]
    fn signal_entry_markers_compose() {
        match syn::parse_str::<GraphSpec>("node A = Terminate, deps: [], writes: [crate::X beat];")
        {
            Ok(_) => panic!("accepted a bare `beat` entry"),
            Err(err) => assert!(
                err.to_string().contains("beat_put"),
                "the rejection names the verb that carries it: {err}"
            ),
        }
        for src in [
            "node A = Terminate, deps: [], writes: [crate::X observed beat];",
            "node A = Terminate, deps: [], \
             writes: [crate::X observed beat via it.get()];",
        ] {
            let spec = syn::parse_str::<GraphSpec>(src).unwrap_or_else(|e| panic!("{src}: {e}"));
            let Item::Node(n) = &spec.items[0] else {
                unreachable!()
            };
            let entry = n.reads.first().or(n.writes.first()).unwrap();
            assert!(entry.beat.is_some(), "{src}");
        }
    }

    /// `discover` binds the `#[dataflow]` tables: bare only, list-exclusive,
    /// and never on a node with nothing to scan.
    #[test]
    fn discover_clause_shape() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: w, discover;\n\
             pool P = [Terminate, OnDemand], deps: [], task: w, discover, \
             policy: Pol::new(), min: 1, max: 2;",
        )
        .unwrap();
        let Item::Node(n) = &spec.items[0] else {
            unreachable!()
        };
        assert!(n.discover.is_some());
        let Item::Pool(p) = &spec.items[1] else {
            unreachable!()
        };
        assert!(p.discover.is_some());

        // A marked entry composes: the scan states the coupling, the list adds
        // the marker a derived table cannot carry.
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], task: w, discover, \
                 writes: [crate::X observed beat];"
            )
            .is_ok(),
            "a marked entry may sit beside `discover`"
        );

        for (bad, want) in [
            (
                "node A = Terminate, deps: [], task: w, discover: 8;",
                "takes no argument",
            ),
            (
                "node A = Terminate, deps: [], task: w, discover, reads: [crate::X];",
                "may only add markers",
            ),
            ("node A = Terminate, deps: [], discover;", "nothing to scan"),
        ] {
            match syn::parse_str::<GraphSpec>(bad) {
                Ok(_) => panic!("accepted: {bad}"),
                Err(err) => assert!(err.to_string().contains(want), "{bad}: {err}"),
            }
        }
    }

    /// Inside a fragment, bare `crate` and `$crate` can only mean the
    /// fragment's own crate, so normalization makes them one spelling — and
    #[test]
    fn bare_crate_normalizes_to_dollar_crate() {
        let ts: TokenStream2 = "task: crate::w, reads: [$crate::X, (crate::Y)]"
            .parse()
            .unwrap();
        let out = normalize_fragment_crate(ts.clone())
            .to_string()
            .replace(' ', "");
        assert_eq!(out.matches("$crate").count(), 3, "{out}");

        let resolved =
            substitute_dollar_crate(normalize_fragment_crate(ts), &"::dep".parse().unwrap())
                .to_string()
                .replace(' ', "");
        assert_eq!(resolved, "task:::dep::w,reads:[::dep::X,(::dep::Y)]");
    }

    #[test]
    fn dataflow_clause_shape() {
        let spec = syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: w, discover, \
             dataflow: [crate::hb::set_period];\n\
             node B = Terminate, deps: [], reads: [crate::X], \
             dataflow: [crate::hb::set_period, other::adjust];",
        )
        .unwrap();
        let Item::Node(a) = &spec.items[0] else {
            unreachable!()
        };
        assert_eq!(a.dataflow.len(), 1);
        let Item::Node(b) = &spec.items[1] else {
            unreachable!()
        };
        assert_eq!(b.dataflow.len(), 2);

        for (bad, want) in [
            (
                "node A = Terminate, deps: [], dataflow: [];",
                "at least one",
            ),
            (
                "node A = Terminate, deps: [], dataflow: [f, f];",
                "duplicate",
            ),
        ] {
            match syn::parse_str::<GraphSpec>(bad) {
                Ok(_) => panic!("accepted: {bad}"),
                Err(err) => assert!(err.to_string().contains(want), "{bad}: {err}"),
            }
        }
    }

    #[test]
    fn discover_cannot_carry_ready_on_write() {
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: w, ready_on_write, discover;",
        ) {
            Ok(_) => panic!("accepted `ready_on_write` with nothing to fire from"),
            Err(err) => assert!(
                err.to_string()
                    .contains("requires an `observed beat` entry"),
                "got: {err}"
            ),
        }
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], task: w, ready_on_write, \
                 beat_timeout: 100, discover, \
                 writes: [crate::X observed beat];"
            )
            .is_ok(),
            "a marked entry beside `discover` is a heartbeat source"
        );
    }

    #[test]
    fn via_on_a_beat_only_entry_is_rejected() {
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], writes: [crate::X beat via it.get()];",
        ) {
            Ok(_) => panic!("`beat via` accepted"),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("only an `observed` entry has"), "got: {msg}");
                assert!(
                    msg.contains("`beat` only ever qualifies `observed`"),
                    "the `beat` half must be named too, got: {msg}"
                );
            }
        }
    }

    #[test]
    fn a_bare_qualifier_names_the_form_it_belongs_to() {
        let src = "node A = Terminate, deps: [], writes: [crate::X via it.get()];";
        match syn::parse_str::<GraphSpec>(src) {
            Ok(_) => panic!("accepted a bare qualifier: {src}"),
            Err(err) => assert!(
                err.to_string().contains("`via` supplies the accessor"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn beat_on_a_read_is_rejected() {
        let src = "observe reads: it.get();\n\
             node A = Terminate, deps: [], reads: [crate::X observed beat];";
        match syn::parse_str::<GraphSpec>(src) {
            Ok(_) => panic!("`beat` accepted on a read"),
            Err(err) => assert!(
                err.to_string().contains("belongs on a `writes:` entry"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn bound_without_ready_rejected() {
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [];\nnode B = Terminate, deps: [A bound];",
        ) {
            Ok(_) => panic!("`bound` without `ready` accepted"),
            Err(err) => assert!(
                err.to_string().contains("`bound` implies `ready`"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn dep_markers_compose() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [];\n\
                 node B = Terminate, deps: [A ready bound];\n\
                 node C = Terminate, deps: [A bound ready];",
            )
            .is_ok()
        );
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [];\nnode B = Terminate, deps: [A ready ready];",
        ) {
            Ok(_) => panic!("duplicate marker accepted"),
            Err(err) => assert!(err.to_string().contains("duplicate `ready`"), "got: {err}"),
        }
    }

    #[test]
    fn pool_accepts_coupling_clauses() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "pool P = [Terminate, OnDemand], deps: [], task: w, \
                 reads: [crate::IN], writes: [crate::OUT], \
                 policy: Pol::new(), min: 1, max: 2;",
            )
            .is_ok()
        );
        assert!(
            syn::parse_str::<GraphSpec>(
                "pool P = [Terminate], deps: [], task: w, writes: [crate::OUT], \
                 policy: Pol::new(), min: 1, max: 1;",
            )
            .is_ok()
        );
    }

    #[test]
    fn empty_signal_list_rejected() {
        for clause in ["reads", "writes"] {
            let src = format!("node A = Terminate, deps: [], {clause}: [];");
            match syn::parse_str::<GraphSpec>(&src) {
                Ok(_) => panic!("empty `{clause}:` accepted"),
                Err(err) => assert!(
                    err.to_string()
                        .contains(&format!("`{clause}:` must declare at least one")),
                    "got: {err}"
                ),
            }
        }
        assert!(
            syn::parse_str::<GraphSpec>(
                "pool P = [Terminate], deps: [], task: w, reads: [], \
                 policy: Pol::new(), min: 1, max: 1;",
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_signal_rejected() {
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], reads: [crate::SIG, other::X, crate::SIG];",
        ) {
            Ok(_) => panic!("duplicate accepted"),
            Err(err) => assert!(
                err.to_string()
                    .contains("duplicate `reads:` entry `crate::SIG`"),
                "got: {err}"
            ),
        }
        match syn::parse_str::<GraphSpec>(
            "pool P = [Terminate], deps: [], task: w, writes: [a::B, a::B], \
             policy: Pol::new(), min: 1, max: 1;",
        ) {
            Ok(_) => panic!("duplicate accepted on a pool"),
            Err(err) => assert!(
                err.to_string().contains("duplicate `writes:` entry `a::B`"),
                "got: {err}"
            ),
        }
    }

    #[test]
    fn distinct_paths_sharing_a_segment_are_fine() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], reads: [a::SIG, b::SIG, SIG];",
            )
            .is_ok()
        );
    }

    #[test]
    fn parked_node_may_declare_coupling() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], reads: [crate::IN], writes: [crate::OUT];",
            )
            .is_ok()
        );
    }

    #[test]
    fn signal_list_takes_paths() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], reads: [SIG, ::root::SIG, a::b::C];",
            )
            .is_ok()
        );
        assert!(
            syn::parse_str::<GraphSpec>("node A = Terminate, deps: [], reads: [1 + 2];").is_err()
        );
    }

    #[test]
    fn ack_timeout_accepted_and_validated() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], task: f, ack_timeout: 5000;\n\
                 pool P = [Terminate], deps: [], task: w, policy: Pol::new(), \
                 min: 1, max: 1, ack_timeout: 100;",
            )
            .is_ok()
        );
        for (src, needle) in [
            (
                "node A = Terminate, deps: [], task: f, ack_timeout: 0;",
                "must be at least 1",
            ),
            (
                "node A = Terminate, deps: [], task: f, ack_timeout: 10, ack_timeout: 20;",
                "duplicate `ack_timeout:` clause",
            ),
        ] {
            match syn::parse_str::<GraphSpec>(src) {
                Ok(_) => panic!("accepted: {src}"),
                Err(err) => assert!(err.to_string().contains(needle), "got: {err}"),
            }
        }
    }

    #[test]
    fn beat_clauses_accepted() {
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], beat_timeout: 100, beat_window: 3;\n\
                 node B = Terminate, deps: [], beat_window: 2, beat_timeout: 50, slot_timeout: 200;",
            )
            .is_ok()
        );
    }

    #[test]
    fn cfg_gated_clauses() {
        const P: &str = "#[cfg(feature = \"x\")]";
        let spec = syn::parse_str::<GraphSpec>(&format!(
            "node A = Terminate, deps: [], task: w, discover, \
             {P} slot_timeout: 100, {P} ack_timeout: 200, \
             {P} beat_timeout: 100, {P} beat_window: 3, {P} disabled;\n\
             node B = Terminate, deps: [], task: w, {P} discover, \
             provides: [{P} R1, R2];",
        ))
        .expect("cfg-gated clauses parse");
        let Item::Node(a) = &spec.items[0] else {
            unreachable!()
        };
        assert_eq!(a.slot_timeout.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(a.ack_timeout.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(a.beat_timeout.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(a.beat_window.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(a.disabled.as_ref().unwrap().cfg.len(), 1);
        assert!(
            a.discover.as_ref().unwrap().cfg.is_empty(),
            "un-gated stays empty"
        );
        let Item::Node(b) = &spec.items[1] else {
            unreachable!()
        };
        assert_eq!(b.discover.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(b.provides[0].cfg.len(), 1);
        assert!(b.provides[1].cfg.is_empty());

        // `ready_on_write` needs its prerequisites in place to parse at all.
        let spec = syn::parse_str::<GraphSpec>(&format!(
            "node A = Terminate, deps: [], task: w, \
             writes: [crate::S observed beat via it.get()], \
             {P} beat_timeout: 100, {P} ready_on_write;",
        ))
        .expect("gated ready_on_write parses beside its gated beat_timeout");
        let Item::Node(a) = &spec.items[0] else {
            unreachable!()
        };
        assert_eq!(a.ready_on_write.as_ref().unwrap().cfg.len(), 1);

        // Structural clauses reject the gate, naming the gateable set.
        for clause in [
            "task: w",
            "spawn: f()",
            "executor: HIGH",
            "exit: u32",
            "state: u32 = 0",
            "cancel",
            "pool_size: 2",
            "deps: [X]",
            "resources: [R: u32]",
            "reads: [crate::S]",
            "provides: [R]",
            "dataflow: [crate::f]",
        ] {
            match syn::parse_str::<GraphSpec>(&format!(
                "node A = Terminate, deps: [], {P} {clause};"
            )) {
                Ok(_) => panic!("`#[cfg]` on `{clause}` accepted"),
                Err(err) => assert!(
                    err.to_string().contains("may only gate `slot_timeout:`"),
                    "`{clause}`: wrong error: {err}"
                ),
            }
        }
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], #[allow(dead_code)] beat_timeout: 100;",
        ) {
            Ok(_) => panic!("a non-cfg attribute accepted"),
            Err(err) => assert!(
                err.to_string()
                    .contains("only `#[cfg(...)]` attributes may gate a clause"),
                "wrong error: {err}"
            ),
        }
    }

    #[test]
    fn gated_beat_timeout_predicate_pairing() {
        const P: &str = "#[cfg(feature = \"x\")]";
        assert!(
            syn::parse_str::<GraphSpec>(&format!(
                "node A = Terminate, deps: [], beat_timeout: 100, {P} beat_window: 3;"
            ))
            .is_ok()
        );
        for (tail, needle) in [
            (
                "beat_window: 3",
                "`beat_window:` must carry the same `#[cfg]`",
            ),
            (
                "#[cfg(feature = \"y\")] beat_window: 3",
                "`beat_window:` must carry the same `#[cfg]`",
            ),
        ] {
            match syn::parse_str::<GraphSpec>(&format!(
                "node A = Terminate, deps: [], {P} beat_timeout: 100, {tail};"
            )) {
                Ok(_) => panic!("mismatched gate accepted: {tail}"),
                Err(err) => assert!(err.to_string().contains(needle), "{tail}: {err}"),
            }
        }
        match syn::parse_str::<GraphSpec>(&format!(
            "node A = Terminate, deps: [], task: w, \
             writes: [crate::S observed beat via it.get()], \
             {P} beat_timeout: 100, ready_on_write;"
        )) {
            Ok(_) => panic!("un-gated ready_on_write over a gated beat_timeout accepted"),
            Err(err) => assert!(
                err.to_string()
                    .contains("`ready_on_write` must carry the same `#[cfg]`"),
                "{err}"
            ),
        }
    }

    #[test]
    fn duplicate_clauses_rejected() {
        for (src, needle) in [
            (
                "node A = Terminate, deps: [], deps: [];",
                "duplicate `deps:` clause",
            ),
            (
                "node A = Terminate, deps: [], task: f, task: g;",
                "duplicate `task:` clause",
            ),
            (
                "node A = Terminate, deps: [], reads: [crate::X], reads: [crate::Y];",
                "duplicate `reads:` clause",
            ),
            (
                "pool P = [Terminate], deps: [A], deps: [], task: w, \
                 policy: Pol::new(), min: 1, max: 1;",
                "duplicate `deps:` clause",
            ),
            (
                "pool P = [Terminate], deps: [], task: w, policy: Pol::new(), \
                 policy: Other::new(), min: 1, max: 1;",
                "duplicate `policy:` clause",
            ),
            (
                "pool P = [Terminate], deps: [], task: w, policy: Pol::new(), \
                 min: 1, max: 1, max: 2;",
                "duplicate `max:` clause",
            ),
            (
                "node A = Terminate, deps: [], beat_timeout: 100, beat_timeout: 200;",
                "duplicate `beat_timeout:` clause",
            ),
            (
                "node A = Terminate, deps: [], beat_timeout: 100, \
                 beat_window: 3, beat_window: 4;",
                "duplicate `beat_window:` clause",
            ),
            (
                "node A = Terminate, deps: [], disabled, disabled;",
                "duplicate `disabled` marker",
            ),
            (
                "node A = Terminate, deps: [], task: w, \
                 writes: [crate::S observed beat via it.get()], \
                 beat_timeout: 100, ready_on_write, ready_on_write;",
                "duplicate `ready_on_write` marker",
            ),
        ] {
            match syn::parse_str::<GraphSpec>(src) {
                Ok(_) => panic!("duplicate accepted: {src}"),
                Err(err) => assert!(err.to_string().contains(needle), "got: {err}"),
            }
        }
    }

    #[test]
    fn malformed_cfg_attribute_rejected() {
        for attr in ["#[cfg]", "#[cfg = \"x\"]", "#[cfg] #[cfg(feature = \"x\")]"] {
            for decl in [
                format!("node A = Terminate, deps: [], {attr} disabled;"),
                format!("node A = Terminate, deps: [], provides: [{attr} R];"),
            ] {
                match syn::parse_str::<GraphSpec>(&decl) {
                    Ok(_) => panic!("malformed cfg accepted: {decl}"),
                    Err(err) => assert!(
                        err.to_string().contains("only `#[cfg(...)]` attributes"),
                        "{decl}: {err}"
                    ),
                }
            }
        }
    }

    #[test]
    fn pool_clause_cfg_rejection_names_pool_alternatives() {
        const P: &str = "#[cfg(feature = \"x\")]";
        let spec = syn::parse_str::<GraphSpec>(&format!(
            "pool P = [Terminate], deps: [], task: w, \
             policy: Pol::new(), min: 1, max: 1, \
             {P} slot_timeout: 100, {P} ack_timeout: 200;"
        ))
        .expect("gated pool timeouts parse");
        let Item::Pool(p) = &spec.items[0] else {
            unreachable!()
        };
        assert_eq!(p.slot_timeout.as_ref().unwrap().cfg.len(), 1);
        assert_eq!(p.ack_timeout.as_ref().unwrap().cfg.len(), 1);

        for clause in ["policy: Pol::new()", "min: 1", "beat_timeout: 100"] {
            match syn::parse_str::<GraphSpec>(&format!(
                "pool P = [Terminate], deps: [], task: w, {P} {clause};"
            )) {
                Ok(_) => panic!("`#[cfg]` on pool `{clause}` accepted"),
                Err(err) => {
                    let msg = err.to_string();
                    assert!(msg.contains("gate the whole pool"), "`{clause}`: {msg}");
                    assert!(!msg.contains("provides"), "`{clause}`: {msg}");
                }
            }
        }
    }

    #[test]
    fn missing_comma_after_marked_entry_is_an_error() {
        match syn::parse_str::<GraphSpec>(
            "node A = Terminate, deps: [], task: f, \
             writes: [crate::S observed via it.get() beat];",
        ) {
            Ok(_) => panic!("phantom entry accepted"),
            Err(err) => assert!(err.to_string().contains("expected `,`"), "got: {err}"),
        }
        assert!(
            syn::parse_str::<GraphSpec>(
                "node A = Terminate, deps: [], task: f, \
                 writes: [crate::S observed beat via it.get(),];",
            )
            .is_ok(),
            "a trailing comma stays legal"
        );
    }

    #[test]
    fn scan_records_fn_level_cfgs() {
        let mut out = Vec::new();
        scan_dataflow(
            "#[cfg(feature = \"x\")]\n#[dataflow]\nasync fn f(node: &'static TaskNode) \
             { node.put(&crate::S, 1); }",
            &mut out,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].cfgs.iter().any(|c| c.contains("feature=")),
            "{:?}",
            out[0].cfgs
        );
    }

    #[test]
    fn qualified_receiver_is_not_a_verb_call() {
        let body = quote!({
            self.node.put(&NOT_OURS, 1);
            foo::node.get(&ALSO_NOT);
            node.put(&OURS, 2);
        });
        let mut seen = Vec::new();
        rewrite_verb_calls(body, "node", &VerbTable::builtin(), &mut |call| {
            seen.push(call.path.clone());
            Ok(None)
        })
        .unwrap();
        assert_eq!(seen, ["OURS"]);
    }
}

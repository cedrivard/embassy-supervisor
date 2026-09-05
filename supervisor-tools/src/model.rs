//! A typed, faithful projection of parsed graph declarations.
//!
//! [`full_model`] maps every clause the grammar carries into plain data:
//! consumers that need the whole declaration (an interpreter, a code
//! generator) read this instead of re-walking the syntax AST, and
//! [`model_json`](crate::model_json) is a projection of it, so the two
//! cannot drift.
//!
//! The projection interprets nothing. Literals and expressions come back as
//! parsed (`syn` values) together with their token text; what a consumer can
//! honor — say, restricting `min:` to an integer literal — is that consumer's
//! policy.

use embassy_supervisor_syntax::{Dep, Item, ResourceDecl, SignalDecl, StateInit, TaskSource};
use quote::ToTokens;
use syn::{Attribute, LitInt};

use crate::Decl;

/// `#[cfg(...)]` gate tokens, spaces stripped, one string per attribute.
pub type CfgTexts = Vec<String>;

fn cfg_texts(attrs: &[Attribute]) -> CfgTexts {
    attrs
        .iter()
        .filter_map(|a| match &a.meta {
            syn::Meta::List(l) if l.path.is_ident("cfg") => {
                Some(l.tokens.to_string().replace(' ', ""))
            }
            _ => None,
        })
        .collect()
}

fn expr_text(e: &syn::Expr) -> String {
    e.to_token_stream().to_string()
}

/// An integer clause value: the literal as parsed, its token text, and the
/// `#[cfg]` gates on the clause.
#[derive(Clone)]
pub struct LitValue {
    /// `#[cfg(...)]` gates on the clause.
    pub cfg: CfgTexts,
    /// The literal as parsed.
    pub lit: LitInt,
    /// The literal's token text.
    pub text: String,
}

impl LitValue {
    fn new(cfg: CfgTexts, lit: &LitInt) -> Self {
        Self {
            cfg,
            lit: lit.clone(),
            text: lit.to_string(),
        }
    }

    /// The value as a `u64`, when the literal is one.
    pub fn as_u64(&self) -> Option<u64> {
        self.lit.base10_parse().ok()
    }
}

/// An expression clause value: as parsed, plus its token text.
#[derive(Clone)]
pub struct ExprValue {
    /// The expression as parsed.
    pub expr: syn::Expr,
    /// The expression's token text.
    pub text: String,
}

impl ExprValue {
    fn new(e: &syn::Expr) -> Self {
        Self {
            expr: e.clone(),
            text: expr_text(e),
        }
    }
}

/// A type clause value: as parsed, plus its token text.
#[derive(Clone)]
pub struct TypeValue {
    /// The type as parsed.
    pub ty: syn::Type,
    /// The type's token text.
    pub text: String,
}

impl TypeValue {
    fn new(t: &syn::Type) -> Self {
        Self {
            ty: t.clone(),
            text: t.to_token_stream().to_string(),
        }
    }
}

/// A `deps:` entry.
#[derive(Clone)]
pub struct DepModel {
    /// The dep target (a node, or a pool resolved to its floor member).
    pub name: String,
    /// The `ready` marker.
    pub ready: bool,
    /// The `bound` marker.
    pub bound: bool,
    /// `#[cfg(...)]` gates on the entry.
    pub cfg: CfgTexts,
}

/// A `reads:` / `writes:` entry.
#[derive(Clone)]
pub struct SignalModel {
    /// The signal path as displayed.
    pub path: String,
    /// The `observed` marker.
    pub observed: bool,
    /// The `beat` marker.
    pub beat: bool,
    /// The `veto` marker: this writer holds a contributor slot of the gate.
    pub veto: bool,
    /// The `via ...` accessor expression's token text, if any.
    pub via: Option<String>,
    /// `#[cfg(...)]` gates on the entry.
    pub cfg: CfgTexts,
}

/// A `resources:` entry.
#[derive(Clone)]
pub struct ResourceModel {
    /// The resource slot name.
    pub name: String,
    /// The `local` marker.
    pub local: bool,
    /// The `consume` marker.
    pub consume: bool,
    /// The `shared` marker.
    pub shared: bool,
    /// The `divisible` marker: a budget the holder claims a share of.
    pub divisible: bool,
    /// The `serialized` marker: every holder runs on one executor.
    pub serialized: bool,
    /// `#[cfg(...)]` gates on the entry.
    pub cfg: CfgTexts,
}

/// A `provides:` entry.
#[derive(Clone)]
pub struct ProvideModel {
    /// The resource slot this item fills.
    pub name: String,
    /// `#[cfg(...)]` gates on the entry.
    pub cfg: CfgTexts,
}

/// Where a task body comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskKind {
    /// A hand-written `#[embassy_executor::task]` fn, via `spawn:`.
    Spawn,
    /// A worker fn wrapped by a generated shell, via `task:`.
    Shell,
}

/// A `spawn:` / `task:` clause.
#[derive(Clone)]
pub struct TaskModel {
    /// Which clause introduced the body.
    pub kind: TaskKind,
    /// The referenced expression's token text.
    pub path: String,
}

impl TaskModel {
    fn new(src: &TaskSource) -> Self {
        match src {
            TaskSource::Spawn(e) => Self {
                kind: TaskKind::Spawn,
                path: expr_text(e),
            },
            TaskSource::Shell(e) => Self {
                kind: TaskKind::Shell,
                path: expr_text(e),
            },
        }
    }
}

/// A `state:` clause.
#[derive(Clone)]
pub struct StateModel {
    /// The state type.
    pub ty: TypeValue,
    /// The initializer: an expression's token text, or `"zeroed"`.
    pub init: String,
}

/// A `node NAME = Mode, ...;` declaration, fully mapped.
#[derive(Clone)]
pub struct NodeModel {
    /// The node name.
    pub name: String,
    /// The lifecycle mode.
    pub mode: String,
    /// `#[cfg(...)]` gates on the node.
    pub cfg: CfgTexts,
    /// The `deps:` list.
    pub deps: Vec<DepModel>,
    /// The `spawn:` / `task:` clause; `None` for a parked node.
    pub task: Option<TaskModel>,
    /// The `pool_size:` literal, if any.
    pub pool_size: Option<LitValue>,
    /// The `resources:` list.
    pub resources: Vec<ResourceModel>,
    /// The `provides:` list.
    pub provides: Vec<ProvideModel>,
    /// The `disabled` marker: `Some(gates)` when present.
    pub disabled: Option<CfgTexts>,
    /// `executor:` name, if any.
    pub executor: Option<String>,
    /// `true` if this node inherited the graph's default executor.
    pub executor_defaulted: bool,
    /// The `slot_timeout:` value in milliseconds.
    pub slot_timeout_ms: Option<LitValue>,
    /// The `ack_timeout:` value in milliseconds.
    pub ack_timeout_ms: Option<LitValue>,
    /// The `beat_timeout:` value in milliseconds.
    pub beat_timeout_ms: Option<LitValue>,
    /// The `beat_window:` value.
    pub beat_window: Option<LitValue>,
    /// The `ready_on_write` marker: `Some(gates)` when present.
    pub ready_on_write: Option<CfgTexts>,
    /// The `reads:` list.
    pub reads: Vec<SignalModel>,
    /// The `writes:` list.
    pub writes: Vec<SignalModel>,
    /// The `exit:` result type, if any.
    pub exit: Option<TypeValue>,
    /// The `state:` clause, if any.
    pub state: Option<StateModel>,
    /// The `cancel` marker.
    pub cancel: bool,
    /// The `discover` marker: `Some(gates)` when present.
    pub discover: Option<CfgTexts>,
    /// Functions adopted via `dataflow:`, as `a::b::c` paths.
    pub dataflow: Vec<String>,
}

/// A `pool NAME = [Mode, ...], ...;` declaration, fully mapped.
#[derive(Clone)]
pub struct PoolModel {
    /// The pool name.
    pub name: String,
    /// The member lifecycle modes.
    pub modes: Vec<String>,
    /// `#[cfg(...)]` gates on the pool.
    pub cfg: CfgTexts,
    /// The `deps:` list.
    pub deps: Vec<DepModel>,
    /// The `spawn:` / `task:` clause.
    pub task: TaskModel,
    /// The scaling policy expression.
    pub policy: ExprValue,
    /// The explicit scaling policy type, if any.
    pub policy_ty: Option<TypeValue>,
    /// `executor:` name, if any.
    pub executor: Option<String>,
    /// `true` if this pool inherited the graph's default executor.
    pub executor_defaulted: bool,
    /// The `resources:` list.
    pub resources: Vec<ResourceModel>,
    /// The `slot_timeout:` value in milliseconds.
    pub slot_timeout_ms: Option<LitValue>,
    /// The `ack_timeout:` value in milliseconds.
    pub ack_timeout_ms: Option<LitValue>,
    /// The `reads:` list.
    pub reads: Vec<SignalModel>,
    /// The `writes:` list.
    pub writes: Vec<SignalModel>,
    /// The `min:` expression.
    pub min: ExprValue,
    /// The `max:` expression.
    pub max: ExprValue,
    /// The `state:` clause, if any.
    pub state: Option<StateModel>,
    /// The `cancel` marker.
    pub cancel: bool,
    /// The `discover` marker: `Some(gates)` when present.
    pub discover: Option<CfgTexts>,
    /// Functions adopted via `dataflow:`, as `a::b::c` paths.
    pub dataflow: Vec<String>,
}

/// An `executor NAME;` declaration.
#[derive(Clone)]
pub struct ExecutorModel {
    /// The executor name.
    pub name: String,
    /// `true` for the graph's `default executor NAME;` declaration.
    pub default: bool,
    /// `#[cfg(...)]` gates on the declaration.
    pub cfg: CfgTexts,
}

/// A top-level item, fully mapped.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum ItemModel {
    /// A supervised node.
    Node(NodeModel),
    /// An elastic pool.
    Pool(PoolModel),
    /// A named executor.
    Executor(ExecutorModel),
}

/// One graph declaration, fully mapped.
#[derive(Clone)]
pub struct GraphModel {
    /// The macro that introduced the declaration.
    pub macro_name: &'static str,
    /// The `name:` clause, if any.
    pub name: Option<String>,
    /// The source origin (file path or label).
    pub origin: String,
    /// The declaration's line in its origin.
    pub line: usize,
    /// The declared items, in order.
    pub items: Vec<ItemModel>,
}

/// Every declaration, fully mapped.
#[derive(Clone)]
pub struct FullModel {
    /// The graphs, in declaration order.
    pub graphs: Vec<GraphModel>,
}

fn dep_model(d: &Dep) -> DepModel {
    DepModel {
        name: d.ident.to_string(),
        ready: d.ready.is_some(),
        bound: d.bound.is_some(),
        cfg: cfg_texts(&d.cfg),
    }
}

fn signal_model(s: &SignalDecl) -> SignalModel {
    SignalModel {
        path: s.display(),
        observed: s.observed.is_some(),
        beat: s.beat.is_some(),
        veto: s.veto.is_some(),
        via: s.via.as_ref().map(expr_text),
        cfg: cfg_texts(&s.cfg),
    }
}

fn resource_model(r: &ResourceDecl) -> ResourceModel {
    ResourceModel {
        name: r.ident.to_string(),
        local: r.local.is_some(),
        consume: r.consume.is_some(),
        shared: r.shared.is_some(),
        divisible: r.divisible.is_some(),
        serialized: r.serialized.is_some(),
        cfg: cfg_texts(&r.cfg),
    }
}

fn state_model(s: &(embassy_supervisor_syntax::kw::state, syn::Type, StateInit)) -> StateModel {
    StateModel {
        ty: TypeValue::new(&s.1),
        init: match &s.2 {
            StateInit::Expr(e) => expr_text(e),
            StateInit::Zeroed(_) => "zeroed".into(),
        },
    }
}

fn dataflow_paths(fns: &[embassy_supervisor_syntax::AdoptedFn]) -> Vec<String> {
    fns.iter()
        .map(|f| {
            f.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::")
        })
        .collect()
}

/// Map every declaration into a [`FullModel`].
pub fn full_model(decls: &[Decl]) -> FullModel {
    let graphs = decls
        .iter()
        .map(|d| {
            let items = d
                .spec
                .items
                .iter()
                .map(|item| match item {
                    Item::Node(n) => ItemModel::Node(NodeModel {
                        name: n.ident.to_string(),
                        mode: n.mode.to_string(),
                        cfg: cfg_texts(&n.cfg),
                        deps: n.deps.iter().map(dep_model).collect(),
                        task: n.source.as_ref().map(TaskModel::new),
                        pool_size: n.pool_size.as_ref().map(|l| LitValue::new(Vec::new(), l)),
                        resources: n.resources.iter().map(resource_model).collect(),
                        provides: n
                            .provides
                            .iter()
                            .map(|p| ProvideModel {
                                name: p.ident.to_string(),
                                cfg: cfg_texts(&p.cfg),
                            })
                            .collect(),
                        disabled: n.disabled.as_ref().map(|g| cfg_texts(&g.cfg)),
                        executor: n.executor.as_ref().map(|e| e.to_string()),
                        executor_defaulted: n.executor_defaulted,
                        slot_timeout_ms: n
                            .slot_timeout
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        ack_timeout_ms: n
                            .ack_timeout
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        beat_timeout_ms: n
                            .beat_timeout
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        beat_window: n
                            .beat_window
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        ready_on_write: n.ready_on_write.as_ref().map(|g| cfg_texts(&g.cfg)),
                        reads: n.reads.iter().map(signal_model).collect(),
                        writes: n.writes.iter().map(signal_model).collect(),
                        exit: n.exit.as_ref().map(TypeValue::new),
                        state: n.state.as_ref().map(state_model),
                        cancel: n.cancel,
                        discover: n.discover.as_ref().map(|g| cfg_texts(&g.cfg)),
                        dataflow: dataflow_paths(&n.dataflow),
                    }),
                    Item::Pool(p) => ItemModel::Pool(PoolModel {
                        name: p.ident.to_string(),
                        modes: p.modes.iter().map(|m| m.to_string()).collect(),
                        cfg: cfg_texts(&p.cfg),
                        deps: p.deps.iter().map(dep_model).collect(),
                        task: TaskModel::new(&p.source),
                        policy: ExprValue::new(&p.policy),
                        policy_ty: p.policy_ty.as_ref().map(TypeValue::new),
                        executor: p.executor.as_ref().map(|e| e.to_string()),
                        executor_defaulted: p.executor_defaulted,
                        resources: p.resources.iter().map(resource_model).collect(),
                        slot_timeout_ms: p
                            .slot_timeout
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        ack_timeout_ms: p
                            .ack_timeout
                            .as_ref()
                            .map(|g| LitValue::new(cfg_texts(&g.cfg), &g.value)),
                        reads: p.reads.iter().map(signal_model).collect(),
                        writes: p.writes.iter().map(signal_model).collect(),
                        min: ExprValue::new(&p.min),
                        max: ExprValue::new(&p.max),
                        state: p.state.as_ref().map(state_model),
                        cancel: p.cancel,
                        discover: p.discover.as_ref().map(|g| cfg_texts(&g.cfg)),
                        dataflow: dataflow_paths(&p.dataflow),
                    }),
                    Item::Executor(x) => ItemModel::Executor(ExecutorModel {
                        name: x.ident.to_string(),
                        default: x.default,
                        cfg: cfg_texts(&x.cfg),
                    }),
                })
                .collect();
            GraphModel {
                macro_name: d.kind.macro_name(),
                name: d.name(),
                origin: d.origin.clone(),
                line: d.line,
                items,
            }
        })
        .collect();
    FullModel { graphs }
}

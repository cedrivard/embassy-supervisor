#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
mod fmt;

mod wake;

#[cfg(all(
    feature = "log",
    not(target_os = "none"),
    not(all(target_arch = "wasm32", target_os = "unknown"))
))]
mod host_log;
#[cfg(all(
    feature = "log",
    not(target_os = "none"),
    not(all(target_arch = "wasm32", target_os = "unknown"))
))]
pub use host_log::init_host_logging;

/// The reference guide: one chapter per submodule.
#[cfg(doc)]
pub mod guide {
    macro_rules! chapters {
        ($($name:ident),*) => {$(
            #[doc = include_str!(concat!("../docs/", stringify!($name), ".md"))]
            pub mod $name {}
        )*};
    }
    chapters!(tiers, model, tasks, dsl, dataflow, recipes);
    chapters!(pools, executors, composing, observability, testing);
}

// Shared with the submodules, which import the crate root with `use super::*;`.
use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll};

use embassy_executor::{SendSpawner, SpawnError, Spawner};
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
#[cfg(any(
    feature = "control",
    feature = "liveness-monitor",
    feature = "lifecycle-events"
))]
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use portable_atomic::AtomicBool;
#[cfg(any(
    feature = "liveness-monitor",
    feature = "fault-inject",
    feature = "breadcrumb"
))]
use portable_atomic::AtomicU8;
use portable_atomic::AtomicU16;
#[cfg(any(
    feature = "trace",
    feature = "liveness",
    feature = "epochs",
    feature = "fault-inject",
    feature = "lifecycle-events",
    feature = "breadcrumb",
    feature = "restart-intensity"
))]
use portable_atomic::AtomicU32;
use wake::WakerSet;

#[cfg(feature = "pool")]
static SCALE_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal that an elastic pool should re-evaluate its scaling decision.
///
/// This is a no-op when the `pool` feature is disabled.
pub fn request_scale() {
    #[cfg(feature = "pool")]
    SCALE_REQ.signal(());
}

#[cfg(feature = "pool")]
/// Wait until something requests a pool scaling re-evaluation.
pub async fn wait_scale() {
    SCALE_REQ.wait().await;
}

/// Some gate moved: a slot filled, a node asserted readiness, a resident shell
/// parked, a fence pass ran. A broadcast, so every supervisor in the binary
/// that waits on a gate (a wave, or a single node's pre-spawn gates)
/// re-checks. Four slots cover a driver plus three more concurrent waiters; a
/// fifth waits out its own deadline.
static GATE_EVT: WakerSet<4> = WakerSet::new();

/// Some node acked a stop or exit. A broadcast, like `GATE_EVT`.
static STOP_EVT: WakerSet<4> = WakerSet::new();

#[doc(hidden)]
#[inline]
pub fn __sv_gate_event() {
    GATE_EVT.signal();
}

#[cfg(feature = "heap-state")]
pub use bytemuck::Zeroable;

// ─── Bound readiness edges (task → supervisor) ────────────────────────────
//
// A dedicated mailbox rather than a widening of CONTROL_REQ: a flapping link
// can produce readiness transitions far faster than an operator produces
// control commands, and the two must not be able to starve each other.

/// "Some node's readiness changed." A coalescing `Signal`, not a queue, and
/// deliberately so: the cascade handler re-reads live `is_ready` state rather
/// than trusting a message, so several transitions collapsing into one wake is
/// not a loss — it is the whole point. A flapping link cannot flood the
/// supervisor, and no readiness change can ever be dropped for lack of queue
/// space. Same single-consumer shape as `SCALE_REQ`: many tasks `signal()`,
/// only the supervisor `wait()`s.
#[cfg(feature = "bound-deps")]
static BIND_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Post a readiness transition. Sync and non-blocking — called from task
/// context inside `set_ready`/`clear_ready`, which must never park.
#[cfg(feature = "bound-deps")]
fn notify_bind() {
    BIND_REQ.signal(());
}

/// Await the next readiness transition of any node. The supervisor's driver
/// loop selects this alongside pool scaling and control.
#[cfg(feature = "bound-deps")]
pub async fn wait_bind() {
    BIND_REQ.wait().await
}

mod combinators;
#[cfg(feature = "control")]
mod control;
mod coupling;
mod fault;
#[cfg(feature = "fault-inject")]
mod fault_inject;
mod graph;
#[cfg(feature = "liveness-monitor")]
mod health;
mod lifecycle;
mod node;
mod slot;
mod supervisor;

pub use combinators::*;
#[cfg(feature = "control")]
pub use control::*;
pub use fault::*;
#[cfg(feature = "fault-inject")]
pub use fault_inject::*;
pub use graph::*;
#[cfg(feature = "liveness-monitor")]
pub use health::*;
pub use node::*;
pub use slot::*;
pub use supervisor::*;

// `coupling` and `lifecycle` are not gated as a whole, so docs.rs shows each
// item's own feature badge. Without `coupling` the first is empty; without
// `lifecycle-events`, `lifecycle-sink` or `breadcrumb` the second holds only
// crate-internal items.
#[allow(unused_imports)]
pub use coupling::*;
#[allow(unused_imports)]
pub use lifecycle::*;

#[cfg(feature = "pool")]
mod pool;
#[cfg(feature = "pool")]
pub use pool::*;

#[cfg(feature = "budget")]
mod budget;
#[cfg(feature = "budget")]
pub use budget::*;

#[cfg(feature = "veto")]
mod veto;
#[cfg(feature = "veto")]
pub use veto::*;

#[cfg(feature = "coupling")]
mod stamped;
#[cfg(feature = "coupling")]
pub use stamped::Stamped;

#[cfg(feature = "dataflow")]
mod dataflow;
#[cfg(feature = "dataflow")]
pub use dataflow::*;

#[cfg(feature = "data-deps")]
mod data_deps;
#[cfg(feature = "data-deps")]
pub use data_deps::*;

#[cfg(feature = "graph-ref")]
mod graph_ref;
#[cfg(feature = "graph-ref")]
pub use graph_ref::*;

#[cfg(feature = "trace")]
/// Runtime tracing hooks and task introspection helpers.
pub mod trace;

#[cfg(feature = "trace")]
#[doc(hidden)]
pub use embassy_executor as __executor;

/// The executor's trace hooks, forwarding to the [`trace`] recorders.
///
/// `supervisor_graph!` expands this once, at the unnamed graph's declaration
/// site, under `trace-hooks`; it lives here rather than in the proc-macro so
/// the hook *bodies* follow this crate's build, not the macro crate's. The
/// executor's hook API is an implementation detail that has already changed
/// shape once: embassy-executor 0.10 links seven `_embassy_trace_*` symbols
/// taking `u32` ids, while its git main (the next release) takes a
/// `raw::trace::Trace` impl registered with `trace_impl!` and passes
/// `ExecutorId`/`TaskRef`. `--cfg embassy_supervisor_trace_v2` (RUSTFLAGS)
/// selects the latter; the recorders keep their `u32` keys either way, ids
/// narrowing through [`trace::task_key`] / [`trace::executor_key`] at the hook.
#[cfg(all(feature = "trace", not(embassy_supervisor_trace_v2)))]
#[doc(hidden)]
#[macro_export]
macro_rules! __sv_trace_hooks {
    () => {
        #[unsafe(no_mangle)]
        fn _embassy_trace_poll_start(executor_id: u32) {
            $crate::trace::on_poll_start(executor_id);
        }
        #[unsafe(no_mangle)]
        fn _embassy_trace_task_new(_executor_id: u32, _task_id: u32) {}
        #[unsafe(no_mangle)]
        fn _embassy_trace_task_end(executor_id: u32, task_id: u32) {
            $crate::trace::on_task_end(executor_id, task_id);
        }
        #[unsafe(no_mangle)]
        fn _embassy_trace_task_exec_begin(executor_id: u32, task_id: u32) {
            $crate::trace::on_task_exec_begin(executor_id, task_id);
        }
        #[unsafe(no_mangle)]
        fn _embassy_trace_task_exec_end(executor_id: u32, task_id: u32) {
            $crate::trace::on_task_exec_end(executor_id, task_id);
        }
        #[unsafe(no_mangle)]
        fn _embassy_trace_task_ready_begin(_executor_id: u32, _task_id: u32) {}
        #[unsafe(no_mangle)]
        fn _embassy_trace_executor_idle(executor_id: u32) {
            $crate::trace::on_executor_idle(executor_id);
        }
    };
}

#[cfg(all(feature = "trace", embassy_supervisor_trace_v2))]
#[doc(hidden)]
#[macro_export]
macro_rules! __sv_trace_hooks {
    () => {
        const _: () = {
            // `trace_impl!` spells its shims' parameter types unqualified, so
            // both must be in scope where it expands.
            use $crate::__executor::ExecutorId;
            use $crate::__executor::raw::TaskRef;

            struct __SvTraceHooks;
            impl $crate::__executor::raw::trace::Trace for __SvTraceHooks {
                fn poll_start(executor: ExecutorId) {
                    $crate::trace::on_poll_start($crate::trace::executor_key(executor));
                }
                fn task_new(_executor: ExecutorId, _task: TaskRef) {}
                fn task_end(executor: ExecutorId, task: TaskRef) {
                    $crate::trace::on_task_end(
                        $crate::trace::executor_key(executor),
                        $crate::trace::task_key(task.id()),
                    );
                }
                fn task_exec_begin(executor: ExecutorId, task: TaskRef) {
                    $crate::trace::on_task_exec_begin(
                        $crate::trace::executor_key(executor),
                        $crate::trace::task_key(task.id()),
                    );
                }
                fn task_exec_end(executor: ExecutorId, task: TaskRef) {
                    $crate::trace::on_task_exec_end(
                        $crate::trace::executor_key(executor),
                        $crate::trace::task_key(task.id()),
                    );
                }
                fn task_ready_begin(_executor: ExecutorId, _task: TaskRef) {}
                fn executor_idle(executor: ExecutorId) {
                    $crate::trace::on_executor_idle($crate::trace::executor_key(executor));
                }
                fn idle() {}
                fn task_name_set(_task: TaskRef, _name: &'static str) {}
                fn task_priority_set(_task: TaskRef, _priority: u8) {}
                fn task_deadline_set(_task: TaskRef, _deadline: u64) {}
            }
            $crate::__executor::trace_impl!(__SvTraceHooks);
        };
    };
}

#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_fragment;
#[cfg(feature = "macros")]
pub use embassy_supervisor_macros::supervisor_graph;

#[cfg(feature = "macros")]
#[macro_export]
/// Compose a graph out of one or more `supervisor_fragment!` declarations.
///
/// The fragments are spliced into the final graph at the compose site. This
/// macro is re-exported by `embassy-supervisor` when the `macros` feature is
/// enabled.
macro_rules! compose_graph {
    (name: $n:ident, fragments: [$f:path $(, $r:path)* $(,)?], graph: {$($g:tt)*}) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {name: $n;}, {$($g)*} }
    };
    (fragments: [$f:path $(, $r:path)* $(,)?], graph: {$($g:tt)*}) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {}, {$($g)*} }
    };
    (@next [], {$($acc:tt)*}, {$($g:tt)*}) => {
        $crate::supervisor_graph! { $($acc)* $($g)* }
    };
    (@next [$f:path $(, $r:path)*], {$($acc:tt)*}, $g:tt) => {
        $f! { @emit $crate::compose_graph, [$($r),*], {$($acc)*}, $g }
    };
}

#[doc(hidden)]
pub mod _export {
    pub use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
    pub use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    pub use embassy_time::Duration;
}

/// Compile-fail contracts of the generated code that the macro crate's UI
/// suite cannot reach: its scratch projects always build with `fault-inject`
/// and never with `heap-state`.
#[cfg(all(doctest, feature = "macros"))]
mod contracts {
    /// A diverging worker never fills an `exit:` slot, so declaring one is an
    /// error: the shell's provide of the exit value is unreachable, and denied.
    ///
    /// ```compile_fail
    /// use embassy_supervisor::{TaskNode, supervisor_graph};
    ///
    /// async fn diverging(_node: &'static TaskNode) -> ! {
    ///     loop {
    ///         core::future::pending::<()>().await;
    ///     }
    /// }
    ///
    /// supervisor_graph! {
    ///     node D = Terminate, deps: [], task: diverging, exit: u32;
    /// }
    ///
    /// fn main() {}
    /// ```
    pub struct DivergingExit;

    /// `state: zeroed T` needs `T: Zeroable`: zero-filled memory is a valid
    /// `T` only for such a type.
    ///
    /// ```compile_fail,E0277
    /// use embassy_supervisor::{TaskNode, supervisor_graph};
    ///
    /// pub struct NotZeroable(core::num::NonZeroU8);
    ///
    /// async fn worker(_node: &'static TaskNode, _state: &mut NotZeroable) {}
    ///
    /// supervisor_graph! {
    ///     node N = Terminate, deps: [], task: worker, state: zeroed NotZeroable;
    /// }
    ///
    /// fn main() {}
    /// ```
    #[cfg(feature = "heap-state")]
    pub struct ZeroedNeedsZeroable;
}

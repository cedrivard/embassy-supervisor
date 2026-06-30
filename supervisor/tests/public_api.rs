//! Integration smoke test: exercise the public surface the way a consumer does —
//! declare `TaskNode`s, wire them with the `task_graph!` macro, and build a
//! `Supervisor`. No executor is started; the node `spawn` fns are never called.
//!
//! Like the unit tests, this runs on the host. The workspace `.cargo/config.toml`
//! pins the embedded target, so an explicit host target is required:
//!
//! ```text
//! cargo test -p embassy-supervisor --target x86_64-unknown-linux-gnu
//! ```

use embassy_supervisor::{BuildError, Mode, Supervisor, TaskNode, task_graph};

// A small two-node graph: `app` depends on `net`. The non-capturing `|_| Ok(())`
// closures coerce to the node `spawn` fn pointer, so the test needs no embassy
// executor types in scope.
static NET: TaskNode = TaskNode::new("net", Mode::Terminate, &[], |_| Ok(()));
static APP: TaskNode = TaskNode::new("app", Mode::Terminate, &[&NET], |_| Ok(()));

task_graph! { &NET, &APP } // emits `pub const NODE_COUNT` + `pub static ALL_NODES`

#[test]
fn task_graph_macro_emits_the_node_set() {
    assert_eq!(NODE_COUNT, 2);
    assert_eq!(ALL_NODES.len(), 2);
}

#[test]
fn supervisor_builds_from_an_acyclic_graph() {
    assert!(Supervisor::new(&ALL_NODES).is_ok());
}

#[test]
fn supervisor_rejects_a_dependency_cycle() {
    static A: TaskNode = TaskNode::new("a", Mode::Terminate, &[&B], |_| Ok(()));
    static B: TaskNode = TaskNode::new("b", Mode::Terminate, &[&A], |_| Ok(()));
    static NODES: [&TaskNode; 2] = [&A, &B];

    assert!(matches!(Supervisor::new(&NODES), Err(BuildError::Cycle)));
}

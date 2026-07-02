//! UI tests for `supervisor_graph!`, via `trybuild`.
//!
//! - `tests/ui/compile_fail/` — locks the macro's compile-error contracts (a
//!   dependency cycle, an unknown dep, a closure in a pool `spawn:`, a malformed
//!   entry). Each has a `.stderr` snapshot; regenerate them after an intentional
//!   message change with `TRYBUILD=overwrite`.
//! - `tests/ui/compile_pass/` — exercises the generated code end-to-end. `trybuild`
//!   compiles *and runs* these, so their `main` asserts the graph invariants
//!   (`GRAPH.order` is a valid topological order, `#[cfg]`-ed-out nodes become `None`,
//!   all `spawn:` forms compile).
//!
//! Run on the host: `cargo test -p embassy-supervisor-macros --target x86_64-unknown-linux-gnu`.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/compile_fail/*.rs");
    t.pass("tests/ui/compile_pass/*.rs");
}

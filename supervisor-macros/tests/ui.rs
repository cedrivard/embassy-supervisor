//! UI tests for `supervisor_graph!`, via `trybuild`.
//!
//! - `tests/ui/compile_fail/` — locks the macro's compile-error contracts (a
//!   dependency cycle, an unknown dep, a closure in a pool `spawn:`, a malformed
//!   entry). Each has a `.stderr` snapshot; regenerate them after an intentional
//!   message change with `TRYBUILD=overwrite`. The cycle snapshots depend on the
//!   `rust-src` component being installed (rustc renders the const-eval panic's
//!   core frame differently without it) — CI installs it to match.
//! - `tests/ui/compile_pass/` — exercises the generated code end-to-end. `trybuild`
//!   compiles *and runs* these, so their `main` asserts the graph invariants
//!   (`GRAPH.order` is a valid topological order, `#[cfg]`-ed-out nodes become `None`,
//!   all `spawn:` forms compile).
//! - `tests/ui/*_local/` — the `local` resource kind's cases, split out because the
//!   kind is gated on the (non-default) `local-resources` feature: its pass/fail
//!   cases only compile WITH the feature, and the "requires the feature" rejection
//!   can only fire WITHOUT it. CI runs the suite in both states.
//!
//! Run on the host: `cargo test -p embassy-supervisor-macros --target x86_64-unknown-linux-gnu`
//! (and once more with `--features local-resources`).

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/compile_fail/*.rs");
    t.pass("tests/ui/compile_pass/*.rs");
    #[cfg(feature = "local-resources")]
    {
        t.compile_fail("tests/ui/compile_fail_local/*.rs");
        t.pass("tests/ui/compile_pass_local/*.rs");
    }
    #[cfg(not(feature = "local-resources"))]
    t.compile_fail("tests/ui/compile_fail_no_local/*.rs");
}

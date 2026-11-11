#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/compile_fail/*.rs");
    t.pass("tests/ui/compile_pass/*.rs");
    // The scratch projects take their supervisor features from this crate's
    // `embassy-supervisor` dev-dependency (Cargo.toml), which turns these on in
    // every build: their directories run in every row.
    t.compile_fail("tests/ui/compile_fail_readiness/*.rs");
    t.pass("tests/ui/compile_pass_readiness/*.rs");
    t.compile_fail("tests/ui/compile_fail_dataflow/*.rs");
    t.pass("tests/ui/compile_pass_dataflow/*.rs");
    t.compile_fail("tests/ui/compile_fail_budget/*.rs");
    t.pass("tests/ui/compile_pass_budget/*.rs");
    t.compile_fail("tests/ui/compile_fail_veto/*.rs");
    t.pass("tests/ui/compile_pass_veto/*.rs");
    t.pass("tests/ui/compile_pass_enabled_if/*.rs");
    t.pass("tests/ui/compile_pass_restart_intensity/*.rs");
    // The dev-dependency leaves these two off, so a row picks a side.
    #[cfg(feature = "local-resources")]
    {
        t.compile_fail("tests/ui/compile_fail_local/*.rs");
        t.pass("tests/ui/compile_pass_local/*.rs");
    }
    #[cfg(not(feature = "local-resources"))]
    t.compile_fail("tests/ui/compile_fail_no_local/*.rs");
    #[cfg(feature = "heap-state")]
    {
        t.compile_fail("tests/ui/compile_fail_heap_state/*.rs");
        t.pass("tests/ui/compile_pass_heap_state/*.rs");
    }
    #[cfg(not(feature = "heap-state"))]
    t.compile_fail("tests/ui/compile_fail_no_heap_state/*.rs");
}

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
    #[cfg(feature = "readiness")]
    {
        t.compile_fail("tests/ui/compile_fail_readiness/*.rs");
        t.pass("tests/ui/compile_pass_readiness/*.rs");
    }
    #[cfg(feature = "heap-state")]
    {
        t.compile_fail("tests/ui/compile_fail_heap_state/*.rs");
        t.pass("tests/ui/compile_pass_heap_state/*.rs");
    }
    #[cfg(feature = "dataflow")]
    {
        t.compile_fail("tests/ui/compile_fail_dataflow/*.rs");
        t.pass("tests/ui/compile_pass_dataflow/*.rs");
    }
    #[cfg(not(feature = "heap-state"))]
    t.compile_fail("tests/ui/compile_fail_no_heap_state/*.rs");
}

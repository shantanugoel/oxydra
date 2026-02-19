#[test]
fn tool_macro_compile_checks() {
    let test_cases = trybuild::TestCases::new();
    test_cases.pass("tests/fixtures/pass_async_tool.rs");
    test_cases.compile_fail("tests/fixtures/fail_non_async.rs");
    test_cases.compile_fail("tests/fixtures/fail_unsupported_argument_type.rs");
}

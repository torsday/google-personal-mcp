// Layer 3 search-excellence verification suite (issue #193, read-only).
//
// All tests in this file are `#[ignore]` — they require:
//   GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config
// Some claims gate on additional env vars (see module docs). See tests/README.md.
//
// Run with: cargo nextest run -E 'test(claim)' -- --ignored

mod e2e {
    mod harness;
    mod search_excellence;
}

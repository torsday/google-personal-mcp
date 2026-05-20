// Layer 3 end-to-end destructive tests.
//
// All tests in this file are `#[ignore]` — they require:
//   GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config
//   GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1
// See tests/README.md for setup.
//
// Run with: cargo nextest run -- --ignored

mod e2e {
    mod harness;
    mod destructive;
}

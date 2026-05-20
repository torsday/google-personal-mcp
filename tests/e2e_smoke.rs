// Layer 3 end-to-end smoke tests (read-only).
//
// All tests in this file are `#[ignore]` — they require:
//   GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config
// See tests/README.md for setup.
//
// Run with: cargo nextest run -- --ignored

mod e2e {
    mod harness;
    mod smoke;
}

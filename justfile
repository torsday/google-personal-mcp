# google-personal-mcp — common dev commands
#
# Most commands assume v0.2 implementation is in place. Until then,
# anything that invokes `cargo` will fail (no Cargo.toml yet); the
# justfile is committed now so v0.2 tickets can build against it.
#
# Run `just` (no args) to list available recipes.

default:
    @just --list

# Run all checks (fmt, clippy, tests, deny) — matches CI
check: fmt-check lint test deny

# Format check (CI-style)
fmt-check:
    cargo fmt --all -- --check

# Format in place
fmt:
    cargo fmt --all

# Lint (clippy with warnings as errors)
lint:
    cargo clippy --all-targets --all-features --locked -- -D warnings

# Run tests via nextest (per ADR-0007)
test:
    cargo nextest run --all-features --locked

# Run a single test by name
test-one name:
    cargo nextest run --all-features --locked {{name}}

# Run the ignored e2e smoke tests against GOOGLE_MCP_TEST_CONFIG_DIR
test-e2e:
    @test -n "$GOOGLE_MCP_TEST_CONFIG_DIR" || (echo "set GOOGLE_MCP_TEST_CONFIG_DIR first (see ADR-0007)" && exit 1)
    cargo nextest run --all-features --locked -- --ignored

# License + advisory check via cargo-deny
deny:
    cargo deny check

# Build release binary
build:
    cargo build --release --locked

# Run the MCP daemon (stdio transport) — assumes auth already ran
run:
    cargo run --release -- serve

# OAuth flow for a new account alias (single-account in v0.2)
auth alias:
    cargo run --release -- auth add --alias {{alias}}

# List configured accounts
accounts:
    cargo run --release -- auth list

# Print Gmail quota cheat sheet (mirrors CLAUDE.md)
quota:
    @echo "Gmail API quota costs (per-method):"
    @echo "  threads.list = 10        threads.get = 40        threads.modify = 10"
    @echo "  threads.trash = 20       messages.get = 20       messages.send = 100"
    @echo "  messages.batchModify=50  history.list = 2        getProfile = 1"
    @echo ""
    @echo "Per-user-per-minute cap: 6,000 units"
    @echo "Per-project-per-minute cap: 1,200,000 units"

# Coverage (local only — no CI gate per ADR-0007)
coverage:
    cargo llvm-cov --all-features --workspace

# Clean build artifacts
clean:
    cargo clean

# Layer 3 — End-to-End Smoke Tests

These tests hit real Gmail to catch catastrophic regressions (wrong
`Authorization` header, response-shape change). They are `#[ignore]` and
never run in CI — run them manually before tagging a release.

---

## Quick start

```sh
export GOOGLE_MCP_TEST_CONFIG_DIR=/path/to/test-config
cargo nextest run -- --ignored
```

For destructive tests (archive, trash, label, send):

```sh
export GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1
cargo nextest run -- --ignored
```

---

## One-time setup

### 1. Create a dedicated test Google account

Use a fresh Google account — **not** your personal one. Tests mutate state
(archive, trash, send); using a personal account risks data loss.

### 2. Create a test GCP project

1. Go to [console.cloud.google.com](https://console.cloud.google.com).
2. Create a new project (e.g. `google-personal-mcp-test`).
3. Enable the **Gmail API** for the project.
4. Create an **OAuth 2.0 Client ID** (application type: Desktop app).
5. Download the credentials JSON.

### 3. Set up the test config directory

```sh
export GOOGLE_MCP_TEST_CONFIG_DIR=$HOME/.config/google-personal-mcp-test
mkdir -p "$GOOGLE_MCP_TEST_CONFIG_DIR/credentials"

# Copy your downloaded credentials JSON:
cp ~/Downloads/client_secret_*.json \
  "$GOOGLE_MCP_TEST_CONFIG_DIR/credentials/google.json"
```

Create `$GOOGLE_MCP_TEST_CONFIG_DIR/config.toml`:

```toml
[google]
credentials_path = "credentials/google.json"

[google.oauth]
redirect_port = 9876

[services.gmail]
enabled = true
profile = "modify+send"
scopes = [
  "https://www.googleapis.com/auth/gmail.modify",
  "https://www.googleapis.com/auth/gmail.send",
]
```

### 4. Authorize the test account

```sh
GOOGLE_PERSONAL_MCP_CONFIG_DIR="$GOOGLE_MCP_TEST_CONFIG_DIR" \
  google-personal-mcp auth add --alias test
```

Sign in with the dedicated test Google account when the browser opens.

### 5. Set optional env vars for destructive tests

```sh
# A thread ID in the test account's inbox:
export GOOGLE_MCP_TEST_THREAD_ID=<thread_id>

# The test account's Gmail address (for self-mail send test):
export GOOGLE_MCP_TEST_EMAIL=test-account@gmail.com

# A user-created label ID in the test account:
export GOOGLE_MCP_TEST_LABEL_ID=<label_id>
```

You can find thread IDs and label IDs by temporarily enabling verbose logging
or by using the Gmail API Explorer against the test account.

---

## Test files

| File | Contents |
|------|----------|
| `tests/e2e/smoke.rs` | Read-only tests: `list_accounts`, `list_labels`, `get_thread`, `search_threads` |
| `tests/e2e/destructive.rs` | Write tests: `archive_thread`, `trash_thread`, `modify_thread_labels`, `send_email` |
| `tests/e2e/harness.rs` | Shared subprocess harness (spawns the binary, talks MCP over stdio) |

---

## Running individual tests

```sh
# Run only smoke tests:
cargo nextest run -E 'test(smoke_)' -- --ignored

# Run only destructive tests (requires GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1):
cargo nextest run -E 'test(destructive_)' -- --ignored
```

---

## Notes

- The `just test-e2e` recipe in the project `justfile` checks that
  `GOOGLE_MCP_TEST_CONFIG_DIR` is set before running `cargo nextest run -- --ignored`.
- These tests never run in CI (`just check` does not pass `--ignored`).
- Run them manually before tagging each release.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::needless_pass_by_value,
    unreachable_pub,
    dead_code
)]
//! Shared harness for Layer 3 end-to-end smoke tests.
//!
//! Tests spawn the `google-personal-mcp` binary, talk MCP JSON-RPC 2.0
//! over stdio, and verify real Gmail responses. Each test is `#[ignore]`
//! and requires `GOOGLE_MCP_TEST_CONFIG_DIR` pointing at a dedicated test
//! installation (see `tests/README.md`).
//!
//! Destructive tests additionally require `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

// ── Environment helpers ───────────────────────────────────────────────────────

/// Return the test config dir from `GOOGLE_MCP_TEST_CONFIG_DIR`, or panic with
/// a clear message. Call this in every e2e test body to enforce the guard.
pub fn require_test_config_dir() -> String {
    std::env::var("GOOGLE_MCP_TEST_CONFIG_DIR").expect(
        "e2e tests require GOOGLE_MCP_TEST_CONFIG_DIR pointing at a dedicated test installation\n\
         See tests/README.md for setup instructions.",
    )
}

/// Guard for destructive tests. Panics unless `GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1`.
pub fn require_destructive_gate() {
    let val = std::env::var("GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E").unwrap_or_default();
    assert_eq!(
        val, "1",
        "destructive e2e tests require GOOGLE_MCP_ALLOW_DESTRUCTIVE_E2E=1\n\
         Set it explicitly; never default-true."
    );
}

// ── MCP subprocess handle ─────────────────────────────────────────────────────

/// A running `google-personal-mcp` process with open stdin/stdout pipes.
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    /// Spawn the binary, send `initialize`, and return a ready handle.
    pub fn start(config_dir: &str) -> Self {
        // Use the binary built by the test harness (CARGO_BIN_EXE_*).
        let bin = env!("CARGO_BIN_EXE_google-personal-mcp");
        let mut child = Command::new(bin)
            .env("GOOGLE_PERSONAL_MCP_CONFIG_DIR", config_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // suppress daemon tracing output
            .spawn()
            .expect("failed to spawn google-personal-mcp binary");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let reader = BufReader::new(stdout);

        let mut proc = Self {
            child,
            stdin,
            reader,
            next_id: 1,
        };

        // Initialize handshake — required before any tool call.
        let init_resp = proc.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "e2e-test", "version": "0.0.0" }
            }),
        );
        assert_eq!(
            init_resp["result"]["protocolVersion"], "2024-11-05",
            "unexpected protocolVersion in initialize response"
        );

        // Send initialized notification (no response expected).
        proc.notify("notifications/initialized", json!({}));

        proc
    }

    /// Send a JSON-RPC request and return the parsed response object.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_line(&msg.to_string());
        loop {
            let resp = self.read_line_json();
            // Skip notifications the server might emit.
            if resp.get("id") == Some(&Value::Number(id.into())) {
                return resp;
            }
        }
    }

    /// Send a JSON-RPC notification (no response).
    pub fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.send_line(&msg.to_string());
    }

    /// Call a tool and return the parsed `result.content[0].text` as a
    /// `serde_json::Value`. Panics on RPC error.
    pub fn call_tool(&mut self, name: &str, args: Value) -> Value {
        let resp = self.request("tools/call", json!({ "name": name, "arguments": args }));
        if let Some(err) = resp.get("error") {
            panic!("tool call `{name}` returned RPC error: {err}");
        }
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("unexpected tool response shape for `{name}`: {resp}"));
        serde_json::from_str(text)
            .unwrap_or_else(|_| panic!("tool `{name}` returned non-JSON text: {text}"))
    }

    fn send_line(&mut self, line: &str) {
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to stdin");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush stdin");
    }

    fn read_line_json(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read from stdout");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("could not parse server output as JSON: {e}\nLine: {line}"))
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

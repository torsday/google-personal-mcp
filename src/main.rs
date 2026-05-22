//! Thin binary shim. All wiring lives in the lib crate so external test
//! harnesses (criterion benches per #90, future integration tests) can link
//! against the same module tree the binary uses.

use std::process::ExitCode;

fn main() -> ExitCode {
    google_personal_mcp::main_entry()
}

//! HTTP-transport bearer authentication per
//! [ADR-0020](../../docs/adr/0020-http-transport-authentication.md).
//!
//! Loads `http_auth.toml` (mode 0600, enforced via [`crate::perm_check`]),
//! holds a [`BearerValidator`] used by the [`middleware`] layer mounted in
//! front of the rmcp `StreamableHttpService`, and supports SIGHUP-driven
//! reload so operators can rotate tokens without restarting the daemon.
//!
//! Scope per #162:
//!
//! - Bearer-only validation; no per-account / per-tool authorization.
//! - Fail-closed startup invariant for non-loopback binds (enforced by the
//!   caller — see [`crate::lib::run_serve_blocking`]).
//! - Constant-time validation with wrong-length-padded comparison.
//! - Multiple active tokens in [`HttpAuthConfig::tokens`] for rotation.
//! - WARN logging on failed auth; no audit record (ADR-0011 §"What is NOT
//!   in this audit log").
//!
//! Out of scope (separately tracked):
//!
//! - Per-source-IP throttle on failed auth (#170, ADR-0020 §Failed-auth
//!   treatment).
//! - `gmcp_http_auth_failures_total` Prometheus counter (#170).
//! - nginx mTLS template (separate ticket).

pub(crate) mod middleware;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use serde::Deserialize;

use crate::error::Error;
use crate::perm_check::{check, Kind, Subject};

/// `http_auth.toml` file name under the daemon's config dir
/// (`~/.config/google-personal-mcp/`). Path resolution lives in
/// [`crate::config::http_auth_path`] to keep all path helpers in one place.
pub(crate) const HTTP_AUTH_FILE: &str = "http_auth.toml";

/// On-disk shape of `http_auth.toml`:
///
/// ```toml
/// tokens = [
///   "long-random-opaque-string-1",
///   "long-random-opaque-string-2",
/// ]
/// ```
///
/// `#[serde(deny_unknown_fields)]` matches the rest of the codebase — a
/// typo in the field name surfaces loudly rather than silently disabling
/// every token. The struct is intentionally tiny; ADR-0020 §"What this ADR
/// does NOT do" rules out per-token metadata (labels, expiry, scopes).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpAuthConfig {
    /// Active bearer tokens. Any token in this list authorizes a request.
    /// Empty is rejected by [`Self::from_file`] — fail-closed for the
    /// "operator deleted everything but left the file" case.
    pub(crate) tokens: Vec<String>,
}

impl HttpAuthConfig {
    /// Load and validate `path`. Refuses to return if:
    ///
    /// - the file is missing (caller should detect non-existence ahead of
    ///   time via [`Path::exists`] for the fail-closed startup branch);
    /// - mode is wider than 0600 (delegated to [`crate::perm_check`]);
    /// - parse fails;
    /// - `tokens` is empty;
    /// - any token is the empty string;
    /// - any two tokens are identical (operator footgun — would silently
    ///   shrink the rotation window).
    pub(crate) fn from_file(path: &Path) -> Result<Self, Error> {
        // Enforce mode 0600 before reading — symmetric with how every
        // other secret file in the daemon is gated at startup (tokens/*,
        // credentials/google.json). [`crate::perm_check`] rejects
        // symlinks and refuses to traverse them, so a symlinked
        // `http_auth.toml` is caught here, not after we've already read
        // its bytes.
        check(&[Subject::new(path.to_owned(), Kind::SecretFile)])?;

        let body = std::fs::read_to_string(path).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("could not read http_auth.toml: {e}"),
        })?;

        let parsed: Self = toml::from_str(&body).map_err(|e| Error::Config {
            path: path.display().to_string(),
            message: format!("could not parse http_auth.toml: {e}"),
        })?;
        parsed.validate(path)?;
        Ok(parsed)
    }

    /// Structural validation of the parsed config. Split from
    /// [`Self::from_file`] so tests can exercise it without touching
    /// disk.
    pub(crate) fn validate(&self, path: &Path) -> Result<(), Error> {
        if self.tokens.is_empty() {
            return Err(Error::Config {
                path: path.display().to_string(),
                message: "tokens array is empty — at least one bearer token \
                    required for non-loopback HTTP transport. Run \
                    `google-personal-mcp auth bearer generate` to create one."
                    .to_owned(),
            });
        }
        for (i, t) in self.tokens.iter().enumerate() {
            if t.is_empty() {
                return Err(Error::Config {
                    path: path.display().to_string(),
                    message: format!(
                        "tokens[{i}] is the empty string — generate a fresh \
                         token with `google-personal-mcp auth bearer generate`"
                    ),
                });
            }
        }
        // Detect duplicates without sorting (which would change order and
        // make error messages less useful) — quadratic in the token count,
        // but the token count is bounded by the operator's rotation
        // discipline (typically 1-2 active).
        for (i, a) in self.tokens.iter().enumerate() {
            for b in &self.tokens[..i] {
                if a == b {
                    return Err(Error::Config {
                        path: path.display().to_string(),
                        message: format!(
                            "tokens[{i}] duplicates an earlier entry — \
                             remove the duplicate"
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Runtime validator. Wraps an `Arc<RwLock<Arc<...>>>` so reads are
/// lock-free after the inner `Arc::clone`, and SIGHUP-driven reloads
/// swap the inner state atomically. Constructed once by
/// [`crate::lib::run_serve_blocking`]; cloned into the axum
/// middleware layer.
///
/// Loopback binds skip the validator entirely — see
/// [`crate::lib::run_serve_blocking`] for the gating logic. The validator
/// itself doesn't know about bind addresses; the caller decides whether
/// to mount the middleware.
pub(crate) struct BearerValidator {
    state: RwLock<Arc<ValidatorState>>,
    path: PathBuf,
}

/// Token set snapshot. Cheap to clone (`Arc` outside). The byte-array form
/// is what [`ct_eq`] consumes; we precompute it at load time so the
/// hot path doesn't pay `String::as_bytes()` per-request — vanishing in
/// optimized builds, but keeps the validate hot loop visibly the only
/// thing happening.
#[derive(Debug)]
struct ValidatorState {
    tokens: Vec<Vec<u8>>,
}

impl ValidatorState {
    fn from_config(cfg: &HttpAuthConfig) -> Self {
        Self {
            tokens: cfg.tokens.iter().map(|t| t.as_bytes().to_vec()).collect(),
        }
    }
}

impl BearerValidator {
    /// Build a validator from a pre-loaded config and the path it came
    /// from. Path is retained so [`Self::reload`] knows what to re-read.
    pub(crate) fn new(cfg: &HttpAuthConfig, path: PathBuf) -> Self {
        Self {
            state: RwLock::new(Arc::new(ValidatorState::from_config(cfg))),
            path,
        }
    }

    /// Snapshot of the path the validator was constructed against. Used
    /// in log lines so a reload event is unambiguous about which file
    /// changed.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Validate a presented bearer token. Returns `true` iff `presented`
    /// matches any active token under constant-time comparison.
    ///
    /// Iterates **every** active token even after a match so the
    /// number-of-active-tokens and position-of-match aren't leaked via
    /// timing. Per-token compare is constant-time and wrong-length-
    /// padded — see [`ct_eq`].
    pub(crate) fn validate(&self, presented: &[u8]) -> bool {
        // Snapshot the Arc under the read lock; release the lock before
        // running the compare loop so a concurrent reload can't be
        // starved by a long stream of validations.
        let state = {
            let guard = self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(&*guard)
        };
        let mut matched: u8 = 0;
        for tok in &state.tokens {
            matched |= u8::from(ct_eq(presented, tok));
        }
        matched != 0
    }

    /// Re-read [`Self::path`] from disk and atomically replace the
    /// active token set. In-flight validations complete with their
    /// pre-swap state — the `RwLock<Arc<_>>` pattern is wait-free for
    /// readers.
    ///
    /// Returns the new token count on success. On failure, the validator
    /// retains its previous state and surfaces the error for the caller
    /// to log; reload failures **do not** drop the daemon to anonymous
    /// access.
    pub(crate) fn reload(&self) -> Result<usize, Error> {
        let cfg = HttpAuthConfig::from_file(&self.path)?;
        let count = cfg.tokens.len();
        let next = Arc::new(ValidatorState::from_config(&cfg));
        {
            let mut guard = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = next;
        }
        Ok(count)
    }

    /// Test-only: install an arbitrary token set without touching disk.
    /// Lets unit tests exercise rotation behavior deterministically.
    #[cfg(test)]
    fn replace_for_test(&self, tokens: Vec<&str>) {
        let state = ValidatorState {
            tokens: tokens.into_iter().map(|t| t.as_bytes().to_vec()).collect(),
        };
        let mut guard = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Arc::new(state);
    }
}

/// Constant-time bytewise equality. Always processes both inputs to
/// their max length so timing leaks neither length nor first-mismatch
/// position — ADR-0020 §"Validation":
///
/// > Wrong-length tokens are constant-time-padded before comparison to
/// > avoid leaking active token lengths via timing.
///
/// The XOR-fold pattern is the standard constant-time equality shape
/// (the same one `subtle::ConstantTimeEq` uses); inlined here to avoid
/// pulling a new dep for two dozen lines of code. `unwrap_or(0)` past the
/// shorter buffer matches the input against zero bytes, which still folds
/// nonzero into `diff` because the longer buffer's bytes are XOR'd against
/// 0 (a no-op for byte 0x00 — but the length-mismatch byte below is
/// guaranteed nonzero, so the overall result is still "unequal").
#[inline]
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let max = a.len().max(b.len());
    let mut diff: u8 = u8::from(a.len() != b.len());
    for i in 0..max {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        diff |= av ^ bv;
    }
    diff == 0
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_mode(path: &Path, contents: &str, mode: u32) {
        fs::write(path, contents).unwrap();
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms).unwrap();
    }

    fn write_0600(path: &Path, contents: &str) {
        write_mode(path, contents, 0o600);
    }

    // ── ct_eq ────────────────────────────────────────────────────────────────

    #[test]
    fn ct_eq_returns_true_for_equal_inputs() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(ct_eq(b"", b""));
        // 256-bit-equivalent comparison
        let long: Vec<u8> = (0..32).collect();
        assert!(ct_eq(&long, &long));
    }

    #[test]
    fn ct_eq_returns_false_for_different_lengths() {
        // Wrong-length tokens must compare false (and we still walk all
        // bytes, but the test only verifies the boolean outcome —
        // statistical timing tests are out of scope for unit testing).
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
        assert!(!ct_eq(b"a", b""));
    }

    #[test]
    fn ct_eq_returns_false_for_one_byte_differences() {
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abcd", b"Xbcd"));
        assert!(!ct_eq(b"abcd", b"abXd"));
    }

    // ── HttpAuthConfig::from_file ────────────────────────────────────────────

    #[test]
    fn from_file_loads_valid_config() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"tokens = ["abc123", "def456"]"#);
        let cfg = HttpAuthConfig::from_file(&p).unwrap();
        assert_eq!(cfg.tokens, vec!["abc123", "def456"]);
    }

    #[test]
    fn from_file_refuses_mode_0644() {
        // ADR-0017 enforces mode 0600 on every secret file; http_auth.toml
        // gets the same gating treatment per ADR-0020 line referenced in
        // the acceptance criteria.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_mode(&p, r#"tokens = ["abc"]"#, 0o644);
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("0600"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_refuses_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("actual.toml");
        write_0600(&real, r#"tokens = ["abc"]"#);
        let link = tmp.path().join("http_auth.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = HttpAuthConfig::from_file(&link).unwrap_err();
        match err {
            Error::InsecurePermissions { message, .. } => {
                assert!(message.contains("symlink"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_refuses_empty_tokens_array() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, "tokens = []");
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("empty"), "got: {message}");
                assert!(
                    message.contains("auth bearer generate"),
                    "remediation must name the subcommand: {message}"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_refuses_empty_string_token() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"tokens = ["abc", ""]"#);
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("empty"), "got: {message}");
                assert!(message.contains("tokens[1]"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_refuses_duplicate_tokens() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"tokens = ["abc", "abc"]"#);
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("duplicate"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_rejects_unknown_field() {
        // `deny_unknown_fields` is the codebase convention — a typo like
        // `token = [...]` must fail loudly, not silently disable auth.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"token = ["abc"]"#);
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        match err {
            Error::Config { message, .. } => {
                assert!(message.contains("parse"), "got: {message}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_file_missing_file_is_config_error() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("does-not-exist.toml");
        let err = HttpAuthConfig::from_file(&p).unwrap_err();
        // perm_check::check returns Ok for missing paths, so we fall
        // through to fs::read_to_string which surfaces Config.
        match err {
            Error::Config { .. } => {}
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── BearerValidator ──────────────────────────────────────────────────────

    fn make_validator(tokens: &[&str]) -> BearerValidator {
        let cfg = HttpAuthConfig {
            tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
        };
        BearerValidator::new(&cfg, PathBuf::from("/dev/null"))
    }

    #[test]
    fn validator_accepts_listed_token() {
        let v = make_validator(&["correct-horse"]);
        assert!(v.validate(b"correct-horse"));
    }

    #[test]
    fn validator_rejects_unknown_token() {
        let v = make_validator(&["correct-horse"]);
        assert!(!v.validate(b"battery-staple"));
    }

    #[test]
    fn validator_rejects_empty_presented_token() {
        let v = make_validator(&["correct-horse"]);
        assert!(!v.validate(b""));
    }

    #[test]
    fn validator_rejects_wrong_length_token() {
        // Sub-string of a valid token is the classic mistake; must be
        // rejected, and must NOT short-circuit (covered by the ct_eq
        // tests above — here we just verify the boolean).
        let v = make_validator(&["correct-horse"]);
        assert!(!v.validate(b"correct-hors"));
        assert!(!v.validate(b"correct-horsex"));
    }

    #[test]
    fn validator_accepts_either_token_during_rotation() {
        // ADR-0020 §Rotation: multiple active tokens during the rotation
        // window. Both must validate.
        let v = make_validator(&["old-token", "new-token"]);
        assert!(v.validate(b"old-token"));
        assert!(v.validate(b"new-token"));
        assert!(!v.validate(b"middle-token"));
    }

    #[test]
    fn validator_iterates_all_tokens_after_match() {
        // We can't easily verify timing properties in a unit test, but
        // we can at least verify the *behavioral* property that a match
        // against a later-listed token still succeeds — the loop
        // doesn't short-circuit on the first miss.
        let v = make_validator(&["nope", "also-nope", "the-real-one"]);
        assert!(v.validate(b"the-real-one"));
    }

    #[test]
    fn validator_reload_swaps_active_set() {
        // Rotation discipline: add new token, reload, then later remove
        // old token and reload again. Each reload reflects in
        // subsequent validate() calls.
        let v = make_validator(&["token-a"]);
        assert!(v.validate(b"token-a"));
        assert!(!v.validate(b"token-b"));

        v.replace_for_test(vec!["token-a", "token-b"]);
        assert!(v.validate(b"token-a"));
        assert!(v.validate(b"token-b"));

        v.replace_for_test(vec!["token-b"]);
        assert!(!v.validate(b"token-a"));
        assert!(v.validate(b"token-b"));
    }

    #[test]
    fn validator_reload_from_disk_picks_up_new_tokens() {
        // End-to-end of the reload path: write a file, build a
        // validator from it, edit the file, call reload, observe the
        // new tokens take effect.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"tokens = ["one"]"#);
        let cfg = HttpAuthConfig::from_file(&p).unwrap();
        let v = BearerValidator::new(&cfg, p.clone());
        assert!(v.validate(b"one"));
        assert!(!v.validate(b"two"));

        // Operator edits the file to add a second active token (rotation
        // window). Mode must stay 0600.
        write_0600(&p, r#"tokens = ["one", "two"]"#);
        let count = v.reload().expect("reload");
        assert_eq!(count, 2);
        assert!(v.validate(b"one"));
        assert!(v.validate(b"two"));
    }

    #[test]
    fn validator_reload_failure_preserves_previous_state() {
        // Reload reading an invalid file must NOT silently flush the
        // active token set — the daemon stays auth-enforced with the
        // pre-reload tokens.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("http_auth.toml");
        write_0600(&p, r#"tokens = ["good-token"]"#);
        let cfg = HttpAuthConfig::from_file(&p).unwrap();
        let v = BearerValidator::new(&cfg, p.clone());
        assert!(v.validate(b"good-token"));

        // Operator corrupts the file (typo, empty array, bad TOML).
        write_0600(&p, "tokens = []");
        let err = v.reload().expect_err("reload must reject empty tokens");
        assert!(matches!(err, Error::Config { .. }));

        // Active set unchanged.
        assert!(
            v.validate(b"good-token"),
            "pre-reload tokens must remain valid after a failed reload"
        );
    }
}

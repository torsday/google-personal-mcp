# ADR-0017: Secrets at rest — restrictive file permissions, no in-app encryption in v1

**Date:** 2026-05-15
**Status:** Accepted

---

## Context

[ADR-0004](0004-oauth-token-refresh.md) stores `access_token`, `refresh_token`, and `client_secret` as plain JSON at `~/.config/google-personal-mcp/tokens/<alias>.json`. [ADR-0008](0008-observability-and-deployment.md) targets a personal VPS as a deployment environment. The combination means the token files **are the entire attack surface** for 10+ Google accounts: anyone with read access to that directory can read every email, send mail as the operator, and modify calendars across all registered accounts. The refresh token is long-lived; revocation requires logging into each Google account.

Prior ADRs hand-wave this. [ADR-0004] notes that the Google "Desktop" OAuth `client_secret` "is not really secret" and suggests `chmod 600`. [ADR-0008] hardens the systemd unit's runtime memory (`ProtectSystem`, `NoNewPrivileges`, `PrivateTmp`) but says nothing about the on-disk secrets the daemon reads at startup.

Three threats to consider:

1. **Filesystem read by another local process or user** — another user account on the VPS, a misconfigured backup tool, a logrotate script that follows symlinks, a borked permissions umask. This is the realistic threat for a personal VPS.
2. **Disk image exfiltration** — VPS provider snapshot leaked, stolen laptop, decommissioned drive not wiped. Defended by full-disk encryption, not by the application.
3. **Live process memory access** — root on the machine, or a debugger attached to the daemon. Defended by OS process boundaries; in-app `zeroize` is theater against a root-level attacker.

Open-source consumers will deploy this in environments we cannot predict. The design must be safe on a shared VPS by default, and document the residual risks the operator owns.

If no decision were made, v1 ships token files at the default umask (often 0644) on a multi-user host, with no startup check, and the first malicious cron job under another local user reads every account's refresh token.

## Decision

### File and directory permissions, enforced at startup

The daemon refuses to start unless permissions are tight enough.

| Path | Required mode | Owner | Enforcement |
| --- | --- | --- | --- |
| `~/.config/google-personal-mcp/` | `0700` | invoking user | Startup check; refuse to serve if wider |
| `~/.config/google-personal-mcp/tokens/` | `0700` | invoking user | Startup check; refuse to serve if wider |
| `~/.config/google-personal-mcp/tokens/<alias>.json` | `0600` | invoking user | Startup check per-file; refuse to serve if any token file is wider |
| `~/.config/google-personal-mcp/credentials/google.json` | `0600` | invoking user | Startup check; refuse to serve if wider |
| `~/.config/google-personal-mcp/accounts.toml` | `0600` | invoking user | Warn-only — file references aliases, not secrets |
| `~/.config/google-personal-mcp/config.toml` | `0600` | invoking user | Warn-only |

Atomic writes ([ADR-0004]'s tmpfile + rename pattern) set the destination mode to `0600` explicitly via `OpenOptions::mode(0o600)` before write. The `auth add` / `auth grant` flows create the directory hierarchy with the required modes; existing-too-wide perms produce a startup error pointing at the path and the expected mode.

**Symlink rejection.** If any of the above paths is a symlink, startup fails. Token files are never followed across links.

**Rationale for refuse-to-start vs. warn-and-continue.** A daemon designed to run on a VPS with 10+ accounts of personal data cannot quietly serve from world-readable token files. Loud-fail is the correct default; an escape hatch is available via `GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK=1` for the rare legitimate case (e.g., an OS where ownership semantics differ — Windows under WSL on a mounted drive). The escape hatch logs WARN at startup and is surfaced by `mcp_status` when implemented ([ADR-0014](0014-status-introspection-tool.md)).

### Storage location

Tokens live on the filesystem under `~/.config/google-personal-mcp/tokens/<alias>.json`, JSON-encoded, plaintext. No in-app encryption in v1. Rationale:

- A daemon that runs unattended must be able to refresh tokens without a passphrase prompt. Encryption-at-rest with no key derivation source means the key is also on disk — security theater.
- Keyring backends (macOS Keychain, Linux Secret Service / libsecret, Windows Credential Manager) **do** solve this — the OS owns the key and gates access by process/user. **macOS Keychain shipped in v0.2** ([#20](https://github.com/torsday/google-personal-mcp/issues/20)) behind the `macos-keychain` Cargo feature flag (`src/auth/secrets/keychain.rs`); macOS CI covers the code path (#33). Linux Secret Service and Windows Credential Manager remain deferred, because:
  - macOS Keychain access from a launchd-managed daemon requires keychain partitioning rules that materially complicate first-run UX — shipped, but the keychain-partitioning workflow is documented in INSTALL.md rather than auto-handled.
  - Linux Secret Service requires a desktop session by default; on a headless VPS the daemon would have to use a keyring backend that is itself unlocked at boot — back to "key is on disk."
  - The Linux/Windows keyring path is real and worth doing later. It is not what gates v1.

### What v1 explicitly does not do

- No in-process encryption of token files (gpg, age, libsodium). Defer until a keyring backend is wired in; encryption without OS-protected key material is theater.
- No `zeroize` / `secrecy` wrappers on `TokenState` fields in memory. The daemon runs as the user; anyone who can read its memory can read its token files. ADR-0004 already documents this as out-of-scope.
- No HSM, no remote secret manager, no Vault. Wrong scale.

### Deployment guidance (documented, not code)

The operator owns two layers this ADR can only document:

1. **Disk encryption.** On a personal VPS, enable provider-level full-disk encryption (LUKS for self-managed, EBS encryption on AWS, etc.). On a laptop, enable FileVault / dm-crypt. This defends against threat 2 (image exfiltration).
2. **User isolation.** The daemon should run as a dedicated unprivileged user (`google-personal-mcp` or similar), not the operator's interactive login user. The systemd unit in [ADR-0008](0008-observability-and-deployment.md) is updated to use `DynamicUser=yes` or an explicit `User=google-personal-mcp`. The `~` in the config path resolves to that user's home — typically `/var/lib/google-personal-mcp/` under systemd's `StateDirectory=`.

These two layers, plus the enforced 0600/0700 perms, are the v1 security posture.

### Logging hygiene

The structured-logging facility ([ADR-0008]) **must never** log `access_token`, `refresh_token`, or `client_secret` values, even at TRACE. Two enforcement mechanisms:

1. The `TokenState` struct implements `Debug` manually, redacting the token fields to `"<redacted>"`. Same for `Display`. Derived `Debug` is forbidden via clippy lint allowlist comment in the source file.
2. A small unit test loads a fake `TokenState` and asserts that `format!("{state:?}")` does not contain the access-token value. Test runs in CI; regression-prevents the next contributor from deriving `Debug` to "make it easier to debug."

Same rule applies to anything that wraps `TokenState`: `TokenManager`, `Account`, and any error variant that captures upstream OAuth response bodies (the `body.contains("invalid_grant")` path in [ADR-0004]). The OAuth response body **may** contain a fresh access token on the error path of a partial refresh; redact before logging or attaching to an `Error::Upstream`.

Enforced as of [#103](https://github.com/torsday/google-personal-mcp/issues/103) by two complementary guards in `src/error.rs`:

- `Error::AuthRequired.reason` is constructed from stable strings only — never spliced with the raw response body. If a structured `error_description` is useful, parse it out (`serde_json::Value::get("error_description")`) and reference the parsed field, *not* the body.
- `Error::upstream(service, status, body)` scrubs `access_token`, `refresh_token`, and `id_token` JSON fields when `service == "google-oauth"` via `redact_oauth_token_fields`. Scrub runs *before* the 4 KiB truncation so a token sitting deep in an oversized body can't leak through the truncated prefix.

Format-output tests in `src/error.rs` and `src/auth/tokens.rs` assert that `format!("{e:?}")` / `format!("{e}")` on the constructed errors never contains a synthetic token literal — regression-prevents the splice from coming back.

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Status quo: plain JSON, no perm check, document chmod in README | Zero implementation work | First-run misconfig leaks every account |
| **(b) Plain JSON + enforced 0600/0700 + redacted Debug + dedicated-user deployment** (chosen) | Defends realistic threat (other local processes); no false sense of security from theater-encryption; keyring path open for later | Operator owns disk-encryption and dedicated-user choices |
| (c) Encrypt token files with a key derived from a daemon-bootstrap secret (file on disk) | Looks more secure | Key is on the same disk as the ciphertext — the attacker reads both. Wastes development effort to no benefit |
| (d) Keyring backend (Keychain / Secret Service) in v1 | Real defense against threat 1, even without 0600 | Materially complicates first-run UX on every supported OS; v1 unblockable on headless VPS without further design |
| (e) Encrypted token store with operator passphrase prompted at daemon start | Real key not on disk | Defeats the always-on-daemon model; daemon can't refresh tokens unattended after reboot |

We choose (b). It defends the realistic threat (other local processes, world-readable files) with a startup gate; it does not pretend to defend threats that need OS-level mechanisms (full-disk encryption, process-memory protection); and it leaves the keyring path open as a v1.x feature without making it block v1.

## Consequences

**Positive:**

- The daemon won't start with leaky perms. The first deployment mistake every Linux user makes (default umask, copied tokens via `scp` that lost perms) becomes a clear startup error, not a silent compromise.
- Redacted `Debug` plus the format-output unit test prevents future contributors from leaking tokens into logs by accident.
- The deferred keyring path is real engineering work, not vaporware — when v1.x ships it, the startup-perm-check stays valid as a defense-in-depth.

**Negative:**

- The `DynamicUser=yes` / dedicated-user deployment shape is more annoying for first-run than "just run it as me." [ADR-0008] needs an update to the systemd unit and the `auth add` flow needs to handle the dedicated-user home path.
- The startup escape hatch (`GOOGLE_PERSONAL_MCP_SKIP_PERM_CHECK=1`) exists; sufficiently determined contributors will find it. Acceptable cost of running on heterogeneous filesystems.

**Risks:**

- *Risk:* Operator runs as their interactive user (ignoring the dedicated-user guidance), tokens end up readable by every process that user runs.
  *Mitigation:* Documentation; `mcp_status` (when implemented per [ADR-0014]) surfaces "running as $USER, recommend dedicated user." Cannot enforce.
- *Risk:* Open-source consumer's distro uses an unusual umask (`077` is common; `022` is also common) and finds the startup check noisy.
  *Mitigation:* The check is on existing perms, not umask; `auth add` creates files with the right mode. Pre-existing too-wide files produce a clear remediation message: "`chmod 600 ~/.config/google-personal-mcp/tokens/<alias>.json`."
- *Risk:* A future keyring-backend implementation changes the storage format and the startup-perm-check no longer applies to the new path.
  *Mitigation:* The check is on the configured storage path; the keyring-backend version skips the file-perm check and runs the keyring-specific equivalent.
- *Risk:* The redaction unit test passes but a custom `Display` impl on a wrapping type still leaks the token.
  *Mitigation:* Add the redaction assertion to the wrapping types' tests as they're added. Hard to lint generically.

## References

- [ADR-0004](0004-oauth-token-refresh.md) — defines the on-disk token format that this ADR governs
- [ADR-0006](0006-config.md) — config-directory layout that this ADR's perm rules apply to
- [ADR-0008](0008-observability-and-deployment.md) — systemd unit; needs `DynamicUser=yes` (or `User=`) + `StateDirectory=` update
- [ADR-0014](0014-status-introspection-tool.md) — `mcp_status` reports on permission posture and escape-hatch usage
- Filesystem semantics: `OpenOptions::mode(0o600)` — `std::os::unix::fs::OpenOptionsExt`

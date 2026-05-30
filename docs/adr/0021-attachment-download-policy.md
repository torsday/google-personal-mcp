# ADR-0021: Attachment download policy — path constraints, extension blocklist, size limits, MIME trust

**Date:** 2026-05-22
**Status:** Accepted (shipped in v0.3)

---

## Context

[ADR-0010](0010-mime-and-encoding.md) defines `list_attachments` and `download_attachment` and outlines the `save_to` parameter, but defers the security policy details. [ADR-0018 §5](0018-email-content-trust.md) establishes the rule that `download_attachment` "must reject path-traversal patterns in `save_to` and refuse executable extensions by default (`.command`, `.app`, `.exe`, `.scpt`, `.workflow`)" — and explicitly says the detailed policy lives in a future attachment ADR. [#62](https://github.com/torsday/google-personal-mcp/issues/62) (`list_attachments`) and [#63](https://github.com/torsday/google-personal-mcp/issues/63) (`download_attachment`) are filed for v0.3, so the trigger is now active.

The attack surface is real:

- **Path traversal:** an attacker (the email sender) sets the attachment filename to `../../../.ssh/authorized_keys`. If the host LLM passes that filename through into `save_to` without sanitization, the daemon writes attacker-controlled content over the operator's SSH keys. Filenames from email are attacker-controllable per [ADR-0018](0018-email-content-trust.md)'s untrusted-content model.
- **Executable extensions:** an attacker emails `invoice.exe` (or `invoice.command` on macOS); the host LLM names it the default filename and saves it; the operator double-clicks. The MCP cannot guarantee what happens next, but it can refuse to materialize executables silently.
- **Size:** Gmail caps message size at 25 MB, but `attachment_id` resolution may produce a `messages.attachments.get` body of up to that. A 25 MB attachment returned as base64 inline is ~33 MB in the MCP response — enough to OOM a small daemon or thrash the host LLM's context.
- **MIME trust:** Gmail returns a `mimeType` in the attachment metadata. We can either trust it (fast, simple, but allows an attacker to label `evil.exe` as `image/png`) or sniff from content bytes (slower, requires a sniffing dep, but defends against label/content mismatch).
- **Filename hygiene:** what if the sender omits the filename entirely? What about filenames with null bytes, control characters, or names that resolve to special device files (`/dev/...`) on a Unix host?

If no decision were made, [#62](https://github.com/torsday/google-personal-mcp/issues/62) and [#63](https://github.com/torsday/google-personal-mcp/issues/63) implementation would have to invent these rules in the PR, leading to either over-cautious "refuse everything" UX or under-cautious "trust Gmail" footguns. The forward-reference in [ADR-0018 §5](0018-email-content-trust.md) deliberately deferred the decision; this ADR resolves it.

## Decision

`download_attachment` enforces a tight, predictable security envelope:

1. **`save_to` is restricted to an operator-allowlisted directory** via `[messages] attachment_save_dir` (already named in [ADR-0010](0010-mime-and-encoding.md)). The daemon resolves `save_to` to an absolute path with symlinks expanded, then verifies the resolved path is a strict prefix-match descendant of the resolved allowlist directory. No `..`, no symlink hop out.
2. **Executable extensions are refused by default**, with a config opt-in for operators who want to download them anyway. Extension match is case-insensitive against the final extension only (not double-extension like `report.pdf.exe`).
3. **Size limits are enforced at three points:** inline base64 cap (per [ADR-0010](0010-mime-and-encoding.md), default 1 MiB), `save_to` per-call cap (default 50 MiB — above the Gmail 25 MB message limit so it's a defense in depth, not a real-world ceiling), and the Gmail API itself.
4. **MIME type is taken from Gmail's `mimeType` field, not sniffed.** Sniffing is a complexity trap that doesn't actually defend the threat model.
5. **Missing or attacker-controlled filenames force the operator to supply `save_to`.** The daemon never auto-derives a save path from an email's filename.

stdio and HTTP transports apply the same policy.

**v0.3 scope.** This ADR's policy ships with the attachment tools in v0.3. The configuration knobs (`[messages]` section keys) land alongside the tools; their default values are conservative enough that an operator who reads INSTALL.md and accepts defaults gets the protections without intervention.

### `save_to` path constraints

The daemon implements a four-step validation:

```rust
fn resolve_save_to(save_to: &Path, allowlist: &Path) -> Result<PathBuf> {
    // 1. The path must be absolute. Reject relative paths outright.
    if !save_to.is_absolute() {
        return Err(Error::InvalidArgument {
            field: "save_to",
            reason: "must be an absolute path",
        });
    }

    // 2. Canonicalize the allowlist root once. (Cached at startup.)
    let allowlist_canonical = std::fs::canonicalize(allowlist)?;

    // 3. The parent directory of save_to must already exist (so we can
    //    canonicalize through it). We do NOT create parent directories.
    //    The operator pre-creates the allowlisted directory.
    let parent = save_to.parent().ok_or(Error::InvalidArgument {
        field: "save_to",
        reason: "has no parent directory",
    })?;
    let parent_canonical = std::fs::canonicalize(parent)?;

    // 4. parent_canonical must be allowlist_canonical or a descendant.
    if !parent_canonical.starts_with(&allowlist_canonical) {
        return Err(Error::InvalidArgument {
            field: "save_to",
            reason: "path is outside [messages] attachment_save_dir",
        });
    }

    // 5. The final filename (separately validated; see filename rules) is
    //    re-appended after parent canonicalization.
    Ok(parent_canonical.join(save_to.file_name().unwrap()))
}
```

Key properties:

- **Symlinks are resolved before the prefix check.** A symlink inside the allowlist that points outside is treated as outside — the check follows it. This defends against the "operator created a convenience symlink" pattern that would otherwise let attackers escape via that symlink.
- **The parent must exist.** We do not auto-create directories. If `save_to` points into a non-existent subdirectory, the daemon returns an `InvalidArgument` error pointing at the missing directory. This is conservative — an attacker filename that contains `/` cannot create surprising directories.
- **The filename component is validated separately** (see below) and re-appended after the parent canonicalization.
- **Absolute paths only.** Relative paths are rejected. The host LLM can't accidentally pass through an attacker-named relative path like `../../etc/cron.daily/evil`.

If `save_to` is omitted, no path validation happens (no file is written). The base64 inline path applies its own size cap.

### Executable-extension blocklist

A configured set of extensions is refused by default:

| Category | Extensions | Rationale |
| --- | --- | --- |
| Native executables | `.exe`, `.com`, `.bat`, `.cmd`, `.msi` | Windows attack vector |
| macOS executables and bundles | `.app`, `.command`, `.workflow`, `.dmg`, `.pkg` | Per [ADR-0018 §5](0018-email-content-trust.md) |
| Scripts | `.sh`, `.bash`, `.zsh`, `.fish`, `.ps1`, `.scpt`, `.applescript`, `.vbs`, `.wsf`, `.scr` | Cross-platform script execution |
| Archives with executable potential | `.jar` | JVM auto-launch on click |

Match logic:

- **Final extension only.** `report.pdf.exe` → `.exe` (refused). `archive.exe.bak` → `.bak` (passed; not on the list). This is the conservative choice: `report.pdf.exe` is the documented prefix-then-real-extension attack, while `archive.exe.bak` is a probable backup file.
- **Case-insensitive.** `.EXE`, `.Exe`, `.eXe` all match.
- **Apply against the email-side filename** (the one the daemon would use if `save_to` is omitted) and **against the `save_to` filename** if provided. If they disagree on extension, both must pass.
- **Configurable via `[messages] executable_extension_action`:** `"refuse"` (default — return `Error::InvalidArgument`), `"warn"` (proceed with a tracing-log WARN and a flag in the response: `{ ..., warning: "executable_extension" }`), or `"allow"` (proceed silently, for operators who deliberately download executables). No environment-variable or per-call override — the policy is set in config, not on the tool surface.

The blocklist itself is **not configurable**. Operators can change the action (refuse/warn/allow); they cannot add or remove specific extensions. Two reasons: (a) editing a blocklist piecemeal invites overconfidence ("I removed `.scr` because it broke a workflow"); (b) the canonical list lives in the source for review-by-grep, not in an operator config file that can drift.

### Size limits

Three layered caps, each with a clear purpose:

| Cap | Config key | Default | Applies to | What happens when exceeded |
| --- | --- | --- | --- | --- |
| Inline base64 | `[messages] max_attachment_inline_bytes` | 1 MiB (1,048,576) | `download_attachment` with no `save_to` | `Error::PayloadTooLarge { size, max }`; operator retries with `save_to` |
| Save-to file | `[messages] max_attachment_save_bytes` | 50 MiB (52,428,800) | `download_attachment` with `save_to` | `Error::PayloadTooLarge { size, max }`; no file written |
| Gmail itself | n/a | 25 MB (Gmail's hard cap) | every attachment | the API returns the bytes; we don't see more |

The save-to cap is intentionally well above Gmail's own 25 MB message limit, because (a) it's defense in depth against future API quirks and (b) an operator wanting to ratchet it down can do so. We do not allow `max_attachment_save_bytes = 0` to disable the cap — values below 1 MiB are rejected at config-parse time, ensuring there's always a meaningful upper bound.

The size check happens **before** writing to disk: the daemon fetches the attachment, then if its decoded size exceeds the cap, returns the error without ever opening the output file. This is wasteful for the bytes-already-fetched, but the alternative (stream + truncate) leaves partial files on disk that the operator then has to clean up. Refuse-before-write is cleaner.

For `save_to`-mode the cap is enforced after base64 decode (the on-disk size, not the wire size). For inline-mode the cap is enforced on the base64-encoded size that will appear in the response (because that's what bounds the MCP payload and the host LLM's context).

### MIME type: trust Gmail's `mimeType`, do not sniff

The `mimeType` field in Gmail's attachment metadata is reported back verbatim in `AttachmentMeta` (per [ADR-0010](0010-mime-and-encoding.md)). The daemon does **not** sniff content bytes to derive an alternate MIME type.

Rationale:

- The blocklist works on extensions, not MIME types. An attacker labeling `evil.exe` as `image/png` does not get past the extension check.
- MIME sniffing is a heuristic with known false positives and negatives. A sniffer that reports "this `.docx` is actually a ZIP" is correct (the OOXML format is ZIP-based) and not useful.
- Adding a sniffing dep (`infer`, `mime_guess`, or similar) expands the dependency surface for a heuristic that the rest of the security model doesn't rely on.
- The MIME type is purely informational in the response. The host LLM uses it for display ("this is an image"), not for trust decisions.

We document in the tool description that `mime_type` is sender-controlled and should not be used for security decisions by the host. This is consistent with [ADR-0018](0018-email-content-trust.md)'s general "every attacker-controllable field is untrusted" stance.

### Filename rules

**The daemon never auto-derives `save_to` from the email's filename.** If `save_to` is omitted, the response is inline base64; there is no "write to a default name" path.

When `save_to` is provided, the **filename component of `save_to`** is independently validated:

| Rule | Reason |
| --- | --- |
| Must be non-empty | Empty filename would write to the parent directory itself |
| Must not contain `/` or `\0` (null byte) | Path-traversal / API abuse |
| Must not contain ASCII control characters (`\x00`-`\x1F`, `\x7F`) | Terminal-injection / display-confusion risk |
| Must not be `.` or `..` | Special directory references |
| Length ≤ 255 bytes (UTF-8) | POSIX `NAME_MAX` on most filesystems |
| Final extension passes the blocklist (above) | Executable refusal |

The email-side filename (from `AttachmentMeta.filename_untrusted`) is **never** used to write a file. It appears in the response only as an `untrusted_content`-wrapped suggestion. The host LLM is responsible for picking a `save_to` filename — typically by sanitizing the suggested one.

This is a deliberate UX cost. The alternative — let the daemon auto-sanitize the email-side filename — invites the host to trust the sanitization, which is a brittle place for the trust boundary. Making `save_to` mandatory-when-saving forces the host to construct an intentional path.

### What this ADR does NOT do

- It does not introduce content-sniffing or virus-scanning. The daemon writes the bytes Gmail returns. Operators who need AV scanning run it on the allowlist directory out-of-band.
- It does not encrypt attachments at rest. The allowlist directory is governed by the operator's filesystem permissions (per [ADR-0017](0017-secrets-at-rest.md)'s mode-600 conventions for sensitive paths).
- It does not introduce a per-account allowlist. `attachment_save_dir` is daemon-wide; attachments from any account land there. Per-account split is a future enhancement if it surfaces a real use case.
- It does not implement `parse_forwarded_attachment` from [ADR-0010](0010-mime-and-encoding.md) §"future enhancement" — that's a separate ADR if needed.
- It does not change attachment behavior in `send_email`. Outbound multipart with attachments is the open-question entry in [ADR-0000](0000-adr-process.md) that this ADR does not resolve.

## Options Considered

### Path-traversal defense

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Reject any `save_to` containing `..` literally | Cheap; obvious | Bypassable via symlinks; does not defend `/etc/passwd` if symlinked into allowlist |
| (b) Reject `save_to` not starting with `$HOME` | Defends the operator's home | Defeats the operator who wants `/var/spool/` or `/mnt/...` |
| **(c) Allowlist directory + canonicalize-then-prefix-check** (chosen) | Defends symlink escapes; operator-extensible; clear failure mode | Slightly more code; parent directory must exist |
| (d) Chroot / `openat()` syscall family | Strongest; can't escape even with symlink races | Linux-specific; portability nightmare for the macOS-friendly daemon |
| (e) Validate filename only, accept any `save_to` | Trivial | Defeats the path-traversal threat entirely |

We choose (c). The canonicalize-then-prefix pattern is well-understood, defends symlink escapes (because canonicalize follows them), and gives the operator a single intuitive lever (`[messages] attachment_save_dir`).

### Executable extension policy

| Option | Pros | Cons |
| --- | --- | --- |
| (f) No blocklist | Simplest | Default-unsafe; contradicts [ADR-0018 §5](0018-email-content-trust.md) |
| **(g) Curated blocklist + config-driven action (refuse/warn/allow)** (chosen) | Operator can downgrade per their workflow; canonical list lives in source for review | Operator must understand the action vs. blocklist distinction |
| (h) Curated blocklist, operator-editable | Maximum flexibility | Edit invites overconfidence; per-deploy drift; harder to audit policy |
| (i) Blocklist by MIME type instead of extension | Defends against renamed `.txt → .exe` | MIME is sender-controlled; the threat model puts no trust there |

We choose (g). The blocklist is canonical (single source of truth); the operator's lever is action-level, not entry-level.

### MIME trust

| Option | Pros | Cons |
| --- | --- | --- |
| (j) Trust Gmail's `mimeType` | Free; consistent with [ADR-0018](0018-email-content-trust.md)'s "untrusted but reported" stance | Sender can mislabel — but the blocklist works on extensions, so this is moot |
| (k) Sniff content bytes via `infer` crate | Defends against `evil.exe` labeled `image/png` | The blocklist already catches `.exe`; adds a dep for negligible gain |
| (l) Both, refuse on mismatch | Maximum signal | Adds dep + complexity + false positives (legit ZIP-based formats) for no real defense |

We choose (j). The defense is at the extension layer; MIME is purely informational.

### Missing-filename behavior

| Option | Pros | Cons |
| --- | --- | --- |
| (m) Auto-derive `save_to` from the attachment's email-side filename if `save_to` is omitted | Convenient for the host | Email filename is attacker-controlled; sanitization belongs in the host, not the MCP |
| **(n) `save_to` is the operator's responsibility; omit → base64 inline** (chosen) | Trust boundary at the host; daemon doesn't sanitize | Slightly more boilerplate for the host LLM |
| (o) Use the message_id as the default filename | Predictable; safe | Unhelpful to the operator; they wanted a meaningful name |

We choose (n). The host is the layer that decides intent; the daemon executes the intent.

## Consequences

**Positive:**

- The attachment tools land with a complete security envelope rather than an "we'll figure it out in PR review" gap.
- The `[messages] executable_extension_action` knob lets operators with legitimate executable-download workflows opt into them without per-call hacks.
- Path-traversal defense is symlink-aware, which is the failure mode naive `..`-rejection misses.
- Three-layered size caps prevent both runtime resource exhaustion (inline cap) and disk exhaustion (save-to cap), with the Gmail-side cap as the outer envelope.
- The MIME-trust decision keeps the dependency tree small and aligns with [ADR-0018](0018-email-content-trust.md)'s overall "report attacker-controlled fields untrusted, don't pretend to sanitize" stance.

**Negative:**

- The host LLM must construct `save_to` explicitly when saving. There is no "just save it" tool ergonomic.
- The pre-creating-directory requirement is one more INSTALL step (`mkdir -p ~/.config/google-personal-mcp/downloads/`) and one more documented failure mode.
- Three new config keys in `[messages]`. Documented in [ADR-0006](0006-config.md) updates.
- Operators with legitimate `.exe` downloads have to set `executable_extension_action = "warn"` or `"allow"` to use them; default-refuse means a small first-time-use friction.

**Risks:**

- *Risk:* Symlink TOCTOU — between the daemon canonicalizing `save_to` and writing the file, the parent directory is replaced with a symlink to elsewhere.
  *Mitigation:* The Rust standard library's `File::create` followed by `File::write_all` is two syscalls; a determined attacker with concurrent shell access can race. The daemon trust model (per [ADR-0017](0017-secrets-at-rest.md)) excludes attackers who already have shell on the host — at that point they don't need TOCTOU. The check is sufficient for the actual threat model (malicious email content + cooperative host).

- *Risk:* The blocklist is incomplete; a novel executable extension appears in the wild (e.g., a future macOS bundle format).
  *Mitigation:* The blocklist is a Rust constant. Adding entries is a one-line PR. Documented as a living list. The `warn` mode lets operators add their own out-of-band detection if they need real-time visibility.

- *Risk:* Operator sets `executable_extension_action = "allow"` on a multi-account install where one account is high-trust and another (e.g. a public list) is not.
  *Mitigation:* Documented footgun. Per-account split is deferred to a future ADR if it surfaces. For now, the operator's account topology is their threat-model knob.

- *Risk:* The `save_to`-parent-must-exist rule confuses operators who expect tools to create directories.
  *Mitigation:* The error message names the missing path and says "create the directory and retry." The first-run flow (per the INSTALL.md update from this ADR's implementation) explicitly says "create your downloads directory."

- *Risk:* Filename component contains Unicode that renders deceptively in the operator's shell (RTL override, homoglyphs).
  *Mitigation:* The byte-length cap (255) and control-character ban catch the worst cases. Full Unicode-confusable defense (NFC normalization, script mixing) is out of scope; the operator's terminal renderer is the layer that handles homoglyph display. Documented limitation.

- *Risk:* `[messages] attachment_save_dir` itself is a symlink (operator convenience), and that symlink target changes at runtime.
  *Mitigation:* The allowlist canonicalization is **cached at startup**, not per-call. Changing the symlink target requires a daemon restart. Documented in [ADR-0006](0006-config.md) `[messages]` notes.

## References

- [ADR-0006](0006-config.md) — `[messages]` config section gains `attachment_save_dir`, `max_attachment_inline_bytes`, `max_attachment_save_bytes`, `executable_extension_action`
- [ADR-0010](0010-mime-and-encoding.md) — defines `download_attachment` / `list_attachments` tool shape; amended by this ADR (this ADR concretizes the deferred policy)
- [ADR-0011](0011-audit-log.md) — `download_attachment` audit record includes `attachment_id`, `mime_type`, `size_bytes`, `save_to` ([ADR-0011](0011-audit-log.md) §"Redaction rules per tool")
- [ADR-0016](0016-tool-surface-and-conventions.md) — `download_attachment` tool conventions (parameter names, account-required, etc.)
- [ADR-0017](0017-secrets-at-rest.md) — file-permission baseline; `attachment_save_dir` is an operator path, not a daemon-managed secret directory
- [ADR-0018](0018-email-content-trust.md) — §5 forward-reference this ADR resolves; filename wrapping in `<<<UNTRUSTED:FILENAME ...>>>`
- Issue [#86](https://github.com/torsday/google-personal-mcp/issues/86) — origin
- Issues [#62](https://github.com/torsday/google-personal-mcp/issues/62) (`list_attachments`) and [#63](https://github.com/torsday/google-personal-mcp/issues/63) (`download_attachment`) — implementation work this ADR unblocks
- POSIX `realpath(3)` / Rust [`std::fs::canonicalize`](https://doc.rust-lang.org/std/fs/fn.canonicalize.html) — primitive for the path-canonicalization check

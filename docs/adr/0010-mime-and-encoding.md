# ADR-0010: MIME and character-encoding handling — `mailparse` + HTML fallback

**Date:** 2026-04-25
**Status:** Accepted

---

## Context

The prototype's body extraction (`Message::body_text` in [`src/gmail/types.rs`](../../src/gmail/types.rs)) handles exactly one shape: a `text/plain` MIME part, base64-encoded, UTF-8 inside. For real-world email this is a correctness bug — most Gmail traffic is one or more of:

- **HTML-only** (most marketing, transactional, calendar invites, GitHub notifications). The prototype returns `None`, the model gets only the snippet.
- **`multipart/alternative`** with both `text/plain` and `text/html` parts where the plain part is degenerate ("View this message in your browser") and the meaningful content is in the HTML.
- **Quoted-Printable** transfer encoding (still common in legacy clients and forwarded chains). The prototype only handles base64.
- **Non-UTF-8 charsets** — ISO-8859-1 from older European mail clients, GB18030 from Chinese senders, Shift_JIS from Japanese, Windows-1252 catch-all. Decoding as UTF-8 produces mojibake or `String::from_utf8` failure.
- **`multipart/mixed`** with attachments interleaved with body parts, where the body is nested inside `multipart/alternative` inside `multipart/mixed`.
- **Embedded images** referenced as `cid:foo@example.com` in HTML.
- **PGP / S/MIME signed messages** (`multipart/signed`, `multipart/encrypted`) — body is wrapped.

For the MCP's role as a Gmail data source consumed by knowledge tools, returning `None` for HTML email is unusable. The model has no way to summarize, search-context, or reason about the actual content.

`mailparse` is the Rust ecosystem's standard for this. It handles every MIME pattern above plus charset conversion via `encoding_rs` underneath. `html2text` (or equivalent like `nanohtml2text`) handles HTML → plain-text rendering with reasonable conventions (links become `[text](url)`, lists indent, etc.).

If no decision were made, the rewrite would re-introduce the discarded prototype's broken body extraction — and "summarize my last 50 emails" returns 50 snippets-only when half the inbox is HTML.

## Decision

We will use **`mailparse`** as the MIME / encoding parser, with **`html2text`** as the HTML → plain-text fallback. Bodies are returned as a structured `BodyContent` enum that surfaces both text and HTML when present, so the consumer can pick.

### `BodyContent` shape

```rust
pub struct ParsedMessage {
    pub headers: Headers,
    pub body: BodyContent,
    pub attachments: Vec<AttachmentMeta>,    // metadata only — no content
}

pub struct BodyContent {
    /// Best-effort plain-text representation of the message body.
    /// - If a `text/plain` part exists, it is decoded and used directly.
    /// - Otherwise, the `text/html` part is decoded and stripped via `html2text`.
    /// - If neither exists (rare; signed-only or encrypted messages), `None`.
    pub text: Option<String>,

    /// Raw HTML if a `text/html` part exists. Independent of `text` —
    /// callers that want HTML get HTML; callers that want plain get plain.
    pub html: Option<String>,

    /// Total combined body length in bytes (text + html, post-decoding) before
    /// any truncation. Used by callers to decide whether to fetch with
    /// pagination or accept truncation.
    pub raw_length: usize,

    /// True if the body was truncated to `[messages] max_body_bytes` (per ADR-0006).
    pub truncated: bool,
}

pub struct AttachmentMeta {
    pub attachment_id: String,        // Gmail's identifier; pass to download_attachment
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub content_id: Option<String>,   // for inline `cid:` references
    pub is_inline: bool,              // Content-Disposition: inline
}
```

### Parsing pipeline

1. Receive Gmail's `messages.get(format=raw)` response — base64-encoded RFC 822.
2. Decode base64 → raw RFC 822 bytes.
3. `mailparse::parse_mail(&raw)` → `ParsedMail` tree.
4. Walk the tree:
   - Collect headers from the root.
   - Recurse into multipart subtrees.
   - For each `text/plain` or `text/html` leaf:
     - Apply transfer-encoding decode (base64 / quoted-printable) — `mailparse` does this.
     - Apply charset decode — `mailparse` returns the charset; we convert via `encoding_rs::Encoding::for_label(charset)`. Fallback to UTF-8 with replacement on failure.
   - For each non-text leaf with `Content-Disposition: attachment` or non-empty filename: record `AttachmentMeta`. Do NOT extract the bytes (that's a separate tool).
   - For `multipart/signed` / `multipart/encrypted`: recurse into the protected payload if visible (PGP/S/MIME inline). For opaque encryption, return body as `None` with header note.
5. If `text/plain` part exists: use it as `text`.
6. Else if `text/html` exists: render via `html2text::from_read(...)` to populate `text`.
7. Always retain `text/html` content as `html` if present.

### Truncation

`config.toml` adds `[messages] max_body_bytes = 100_000` (default). Bodies over this limit are truncated per-representation (text and html each capped) with `truncated = true`. Truncation message appended: `"\n\n[truncated by google-personal-mcp at <N> bytes]"`. The full body remains available to the cache (per [ADR-0009](0009-caching-with-sqlite-and-history-api.md)) — only the MCP response is truncated.

This bounds what the LLM consumer sees per-message without losing data.

### Attachments — separate tools

Attachments are surfaced as metadata in every `get_thread` / `get_message` response. Two new tools handle content:

- `list_attachments(message_id, account?)` — returns `Vec<AttachmentMeta>` (also returned inline by `get_message`; this is for explicit cases).
- `download_attachment(message_id, attachment_id, account?, save_to?: PathBuf)` — fetches the attachment.
  - If `save_to` is provided: writes the file to that path (must be within `[messages] attachment_save_dir` allowlist for security), returns `{path, size_bytes, mime_type}`.
  - If `save_to` is omitted: returns `{base64, size_bytes, mime_type}` — base64 in the MCP response (capped at `[messages] max_attachment_inline_bytes`, default 1 MiB; larger requires `save_to`).

Tradeoff: forcing operator allowlist prevents the model from arbitrary file-writes. Allowlist defaults to `~/Downloads/google-personal-mcp/` if unset.

### Forwarded messages

A common email pattern is the inline forwarded message — the body contains both the sender's commentary and the original message embedded as text (typically delimited by `----- Forwarded message -----` or `Begin forwarded message:`). MIME formally supports nested messages via the `message/rfc822` content-type, which `mailparse` does parse recursively, but **most real-world forwards from Gmail / Apple Mail / Outlook do NOT use `message/rfc822`** — they inline the original as quoted text in `text/plain` or `text/html`.

**v1 behavior:** treat forwarded messages as opaque text. The `body_text` returned will include both the wrapper text and the embedded original-message text, in whatever order they appear in the source. The consumer (LLM) handles disambiguation — common-pattern recognition like "split on `----- Forwarded message -----`" is the consumer's job, not ours.

**`message/rfc822` parts** (the formal forwarded-message MIME type, used by some MUAs and by mailing-list expansions): these are surfaced as attachments in `AttachmentMeta` with `mime_type = "message/rfc822"`. Future enhancement: a `parse_forwarded_attachment(message_id, attachment_id)` tool that returns a nested `ParsedMessage` for explicit forwarded-MIME-part parsing. Out of scope for v1; flagged as a future ADR.

### Confidential mode

Gmail's "Confidential mode" (set by the sender via the Gmail UI) restricts forward / copy / download. Server-side, the message body in `messages.get` for the recipient is **either** the plain content (if recipient has access) or a redacted notice with a link to view the full content via Gmail's web UI. This is opaque to our parser — `body_text` returns whatever Gmail returned. Consumer behavior on confidential-mode messages is "you get the redacted text, deal with it." Document in tool description for `get_thread`.

### `text/html` rendering choices

`html2text` settings:
- Wrap at 100 columns
- Preserve link URLs (`[anchor text](https://...)`)
- Strip `<style>`, `<script>`, `<head>` entirely
- Render tables as plain-text tables (not collapsed)
- Quoted/reply blocks (`<blockquote>`) prefix with `> ` per line

Specific decisions:
- We do **not** sanitize HTML (we're returning rendered text or raw HTML; no XSS surface — MCP responses are text content delivered to a model, not rendered by a browser)
- We do **not** follow `cid:` references to inline images during HTML→text rendering (link is preserved as `[image: cid-xxx]`)
- We do **not** strip "view in browser" / "unsubscribe" preludes (some heuristic could; out of scope for v1; consumers can post-process)

### Cache integration

Per [ADR-0009], parsed `body_text` and `body_html` are stored in SQLite cache (immutable per message). Re-parsing on every read is wasted work; the SQLite columns hold the post-`mailparse` strings.

This means a `mailparse` upgrade producing slightly different output requires a cache version bump (per ADR-0009 `schema_version`).

## Options Considered

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Keep prototype's plain-text-only handling | No new deps | Returns nothing for HTML-only mail (most modern email); broken charset handling; broken Quoted-Printable handling; not viable |
| **(b) `mailparse` + `html2text`** (chosen) | Battle-tested; handles everything we listed; small dep weight (~1 MB of deps total); ecosystem standard | Two new deps; decoding adds ~10ms per message (negligible) |
| (c) `lettre`'s parser | Same author has builder for sending | `lettre` is designed for SMTP send; parsing is secondary; less mature parsing API |
| (d) Roll our own RFC 822 / 2822 / 5322 parser | Total control; minimal deps | Dozens of edge cases (folded headers, encoded-words, charset variants) — high cost, recreate `mailparse` poorly |
| (e) Use Gmail's `format=full` (parsed JSON tree) instead of `format=raw` | No MIME parsing on our side — Gmail returns part tree | `format=full` payload doesn't include the raw transfer-encoded bytes consistently for all parts; charset metadata is patchy; plus `format=raw` returns decoded base64 of the original message which is what we want |
| (f) `mailparse` only, no HTML fallback | Smaller dep set | HTML-only emails return `None` (the original bug we're trying to fix) |
| (g) `scraper` + manual HTML walk instead of `html2text` | More flexible | Reimplementing the rendering layer for a small win; `html2text` covers the common cases |

We choose (b). The combination handles the realistic Gmail surface; both crates are mature; the cost is small.

### HTML stripping crate choice

`html2text` vs alternatives:

| Crate | Notes |
| --- | --- |
| `html2text` | Most popular; preserves links sensibly; handles tables; ~10K downloads/month |
| `nanohtml2text` | Lighter; less complete (tables, links) |
| `scraper` + custom render | Total control, more code |

Going with `html2text`.

### Charset handling

`mailparse` exposes the charset declared in `Content-Type: text/plain; charset=...`. We pipe this through `encoding_rs::Encoding::for_label(charset)`:

- Known charset → decode to UTF-8 with replacement (`encoding_rs::Encoding::decode`)
- Unknown / missing charset → assume UTF-8 with replacement
- Decode never panics; produces best-effort output

This handles: ISO-8859-1, ISO-8859-15, Windows-1252, Shift_JIS, GB18030, EUC-KR, Big5, KOI8-R, and a few dozen more. Covers ~99% of real email charsets.

## Consequences

**Positive:**

- HTML-only emails produce useful `body_text` — avoiding the single biggest correctness bug the discarded prototype had.
- Multipart traversal handles mixed/alternative/related/signed structures.
- Charset conversion handles non-UTF-8 mail correctly (no mojibake, no UTF-8 errors crashing the parse).
- Quoted-Printable and base64 transfer encodings both decoded.
- Attachments are surfaced as metadata without forcing the model to download them.
- Body truncation prevents catastrophic context-window blowups when a 5 MB email arrives.
- Both `text` and `html` available so consumers can pick (an HTML-rendering plugin upstream gets HTML; a token-budget-conscious agent gets text).
- Pure functions over byte slices — trivially unit-testable with fixture emails.

**Negative:**

- Two new deps. Both small; both standard.
- Parser output stored in cache means cache invalidation on `mailparse` major upgrade. Documented; cheap to handle (drop body_text/body_html columns on schema_version bump).
- HTML-stripped text loses formatting nuance. For a knowledge-extraction use case this is fine; for "show me the email exactly as sent" the consumer wants the raw HTML (we provide it).
- Attachments are non-trivial to deal with in HTTP transport mode (large base64 in response). Mitigated by `save_to` path option + size cap.
- PGP / S/MIME body content inside encrypted MIME requires a separate decryption path; out of scope for v1 (`text` returns `None` with a tracing-WARN noting the encryption format).

**Risks:**

- *Risk:* A pathological email triggers `mailparse` panic or runaway parsing.
  *Mitigation:* `mailparse` is well-fuzzed; we wrap parse in `tokio::time::timeout` (default 5s); failure surfaces as `Error::Parse` (per [ADR-0005](0005-error-model.md)) with a fallback to returning headers + snippet only.
- *Risk:* Charset detection wrong → text is mojibake.
  *Mitigation:* `encoding_rs` decoding uses U+FFFD replacement on failure (visible as `?` characters); operator can spot. Adding chardet-style auto-detection is out of scope; if a real source of bad-charset email shows up, revisit.
- *Risk:* `html2text` rendering of complex layouts (newsletters with deep table nesting) produces garbage.
  *Mitigation:* `html` field always available for the consumer to use directly. Document in tool descriptions.
- *Risk:* Attachment downloads bypass any AV / sandboxing the operator might want.
  *Mitigation:* Save-to-allowlist limits download paths. Document that operator is responsible for downstream handling. Flag in audit log per [ADR-0011](0011-audit-log.md).
- *Risk:* Body truncation hides important content from the model.
  *Mitigation:* `truncated: true` field is explicit; tool descriptions instruct callers to check. Larger bodies live in cache and are accessible via a future `get_full_body(message_id)` tool if needed.
- *Risk:* `cid:`-referenced inline images in HTML aren't resolved.
  *Mitigation:* Documented limitation. The HTML rendering shows `[image: cid-xxx]` placeholders. Resolving requires fetching the corresponding attachment; out of scope unless a clear use case appears.

## References

- [ADR-0005](0005-error-model.md) — `Error::Parse` for `mailparse` failures
- [ADR-0006](0006-config.md) — `[messages]` section: `max_body_bytes`, `attachment_save_dir`, `max_attachment_inline_bytes`
- [ADR-0009](0009-caching-with-sqlite-and-history-api.md) — parsed `body_text` / `body_html` cached in SQLite
- [ADR-0011](0011-audit-log.md) — attachment downloads logged to audit
- [`mailparse`](https://docs.rs/mailparse) — RFC 822/2822/5322 parser
- [`html2text`](https://docs.rs/html2text) — HTML → plain-text rendering
- [`encoding_rs`](https://docs.rs/encoding_rs) — charset conversion (transitive via `mailparse`)
- Gmail [`messages.get?format=raw`](https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.messages/get) — the source format for parsing

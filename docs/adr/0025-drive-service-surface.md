# ADR-0025: Drive service surface — full read/write/destructive under the capability gate

**Date:** 2026-05-28
**Status:** Accepted, target v1.1
**Depends on:** [ADR-0022](0022-capability-gating.md), [ADR-0021](0021-attachment-download-policy.md)

---

## Context

[ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) names Drive as a future service module. The maintainer wants full read/write/destructive Drive coverage under the [ADR-0022](0022-capability-gating.md) capability gate. Drive is the largest service surface and carries the heaviest threat model in the corpus so far:

- **`update_permissions` is the single highest-blast-radius operation in the project.** A wrong sharing change can leak operator data to the public internet — the kind of mistake that doesn't show up until a Google notification email arrives days later.
- **Drive's scope set has a sharp split that other services don't have.** `drive.file` (app-created-only — the daemon can only see files it created itself) vs `drive` (full read+write on every file the operator owns or has access to). The choice between them is a load-bearing threat-model decision the operator must make explicitly.
- **Files and folders are the same resource** (`mimeType: application/vnd.google-apps.folder`), but workflows differ — `upload_file` to a folder vs `create_folder` are distinct operations even though both create resources.
- **Google-native files** (Docs, Sheets, Slides) have **no downloadable bytes** — they must be exported (PDF, docx, csv, etc.) to extract content. This is API surface, not implementation detail.
- **Trashed vs permanently-deleted** is a real distinction: trash is undoable for 30 days, permanent delete is not.
- **Shared drives** ("Team Drives") have ownership semantics distinct from "My Drive" (shared-drive admin owns the file rather than the user). v1 covers personal Drive only.
- **File content attaches the existing [ADR-0021](0021-attachment-download-policy.md) policy** — path-traversal/extension/size rules apply identically to Drive downloads.

If no decision were made, Drive tools would inevitably ship `update_permissions` without confirming the right scope-and-capability gating, or surface `delete_file` without distinguishing it from `trash_file`, or mishandle the export-vs-download split for Google-native files.

## Decision

We adopt a 15-tool Drive surface, classified per [ADR-0022](0022-capability-gating.md) aspects, mapped onto Drive scopes, with **`update_permissions` carrying an additional explicit `confirm` literal** (per the [ADR-0019](0019-data-retention-and-purge.md) `purge_account` pattern) because public-share mistakes are unrecoverable.

### Tool inventory

| Tool | Aspect | Scope (min) | Notes |
| --- | --- | --- | --- |
| `list_files` | read | `drive.readonly` | Forward-passes Drive's `q` query syntax; `corpora` defaults to `user` (personal Drive only — shared drives out of scope per §Out of scope); `page_token` pagination |
| `search_files` | read | `drive.readonly` | Same as `list_files` with a free-text shortcut; folded into `list_files` if the tool count grows too high — kept separate for now to match Gmail pattern |
| `get_file_metadata` | read | `drive.metadata.readonly` | By `file_id`; supports `fields` mask analogous to Contacts' `personFields` |
| `download_file` | read | `drive.readonly` | Binary content; subjects to [ADR-0021](0021-attachment-download-policy.md) path-traversal/extension/size rules; Google-native files return a typed error pointing at `export_file` |
| `export_file` | read | `drive.readonly` | Docs/Sheets/Slides → bytes in a specified MIME type (PDF, docx, csv, html, txt) |
| `list_revisions` | read | `drive.metadata.readonly` | File version history; metadata only — no content downloads |
| `get_about` | read | `drive.readonly` | About resource: storage quota, user info, supported import/export MIME types |
| `upload_file` | write | `drive.file` or `drive` | Multipart upload; `drive.file` operators see only files they uploaded via this MCP |
| `create_folder` | write | `drive.file` or `drive` | Returns the folder file resource |
| `update_file_metadata` | write | `drive` | Rename, move (via `add_parents`/`remove_parents`), set description, set MIME type alias; `etag` required |
| `copy_file` | write | `drive.file` or `drive` | Returns the new file's metadata |
| `trash_file` | destructive | `drive` | Sets `trashed = true`; reversible via `untrash_file` for 30 days |
| `untrash_file` | write | `drive.file` or `drive` | Sets `trashed = false`; the recovery primitive; `drive.file` suffices for files the daemon originally created |
| `delete_file` | destructive | `drive` | **Permanent** delete; `dry_run` per [ADR-0012](0012-idempotency-and-dry-run.md); skips trash; irreversible |
| `update_permissions` | destructive | `drive` | Create / update / delete sharing entries; **requires `confirm: "yes-share-<file_id>"` literal**; `dry_run` shows the diff |

We deliberately keep `list_permissions` *out* of the read surface in v1 — its primary use is "preview before changing," and `update_permissions` with `dry_run: true` covers that. If a real listing-only use case emerges (audit reports), a follow-up adds it under read.

### Scope-to-capability mapping

| Config aspect | Tools enabled | Scopes implied |
| --- | --- | --- |
| read | `list_files`, `search_files`, `get_file_metadata`, `download_file`, `export_file`, `list_revisions`, `get_about` | `drive.readonly` (or `drive.metadata.readonly` for metadata-only subset) |
| write | `upload_file`, `create_folder`, `update_file_metadata`, `copy_file`, `untrash_file` | `drive.file` (limited) or `drive` (full) |
| destructive | `trash_file`, `delete_file`, `update_permissions` | `drive` |

### The `drive.file` vs `drive` decision

This is the single most important per-account choice an operator makes when enabling Drive. The two scopes carry materially different threat models:

| | `drive.file` | `drive` |
| --- | --- | --- |
| What the daemon sees | Only files it created (or that the user explicitly opens through the daemon) | Every file the operator owns or has access to |
| Read blast radius if daemon compromised | Limited to MCP-created files | Every file in the operator's Drive |
| Write blast radius | Same | Every file modifiable by the operator |
| Recommendation | **First-time setup default** | Operator opts in deliberately |

The capability config gates which scope is requested at `auth add`/`auth grant`:

```toml
[services.drive]
enabled = true
scopes  = ["https://www.googleapis.com/auth/drive.file"]   # safer default; agent only sees what it made

[services.drive.accounts.personal]
scopes = ["https://www.googleapis.com/auth/drive"]         # explicit per-account override for full Drive
```

Per-account scope override extends the [ADR-0022](0022-capability-gating.md) per-account capability override to scopes themselves. INSTALL.md leads with the `drive.file` default and explains when to widen.

### The `confirm` gate on `update_permissions`

Per [ADR-0019](0019-data-retention-and-purge.md)'s `purge_account` precedent, `update_permissions` requires an additional literal-string `confirm` parameter:

```rust
update_permissions(
  account: String,
  file_id: String,
  changes: Vec<PermissionChange>,        // {role, type, email_address, allow_file_discovery, ...}
  confirm: String,                       // literal "yes-share-<file_id>"
  dry_run: bool,
) -> UpdatePermissionsResult
```

Rationale: public-share mistakes are *uniquely* unrecoverable in the destructive-tool family. `delete_file` is irreversible but local; `update_permissions` to "anyone with the link" leaks data to the internet. The literal-string guard makes accidental model invocation structurally impossible — the model has to produce the exact string with the file id embedded.

`dry_run: true` returns the would-be permission diff (added, modified, removed entries) without writing; still emits an audit record per [ADR-0011](0011-audit-log.md) / [ADR-0012](0012-idempotency-and-dry-run.md).

### Untrusted-content posture

| Field | Trust | Why |
| --- | --- | --- |
| `name_untrusted` | untrusted | File names from shared files are attacker-controllable; even on operator-owned files, names embed prompt-injection vectors |
| `description_untrusted` | untrusted | Same |
| `owners[].display_name_untrusted`, `email_untrusted` | untrusted | For shared files, attacker-set |
| `permissions[].display_name_untrusted`, `email_untrusted` | untrusted | Same |
| `last_modified_by_user_untrusted` | untrusted | The modifier may be the operator on a file they own and only edit themselves, but on any file with sharing the modifier is whoever last touched it — including via the share-edit path on operator-owned files. Consistent wrapping avoids a per-file-trust toggle the host LLM might miss |
| `mime_type`, `id`, `size`, `created_time`, `modified_time` | trusted | Google-side typed/opaque |
| `web_view_link`, `web_content_link` | untrusted | URLs to potentially-attacker-controlled targets; agent should not auto-follow |

### `download_file` and the ADR-0021 policy

[ADR-0021](0021-attachment-download-policy.md) defines path-traversal, extension blocklist, and size-cap rules for Gmail attachment downloads. `download_file` inherits the same policy verbatim:

- `save_to` parameter rejects path-traversal patterns (`..`, absolute paths outside the operator's allowed-roots)
- Executable extension blocklist (`.command`, `.app`, `.exe`, `.scpt`, `.workflow`) — refused by default; operator can override per [ADR-0021](0021-attachment-download-policy.md)
- Size cap from [ADR-0021](0021-attachment-download-policy.md); response includes `truncated: true` if hit

This isn't a copy — `download_file` calls the same enforcement code as `download_attachment` (a shared `safe_save_to` helper). One policy, two callers.

### `export_file` — Google-native files

Docs/Sheets/Slides have no `bytes`; they require export. `export_file` accepts a `mime_type` parameter naming the export format (e.g. `application/pdf`, `application/vnd.openxmlformats-officedocument.wordprocessingml.document`, `text/csv`). `get_about` returns the per-account supported import/export MIME types so the caller can discover what's available.

If `download_file` is called on a Google-native file, the typed error variant ([ADR-0005](0005-error-model.md)) is `Error::ExportRequired { mime_type, supported_export_types }` with the suggestion to call `export_file`.

### Cache fit

Drive has `changes.list` (analogous to Gmail's `history.list`) for incremental sync. **Defer caching to a follow-up ADR.** Same staged approach. Drive metadata caching has particular value (file listings change rarely; content changes per file) but the value-per-LOC argument applies as for Calendar/Contacts.

### Out of scope (intentionally)

- **Shared drives (Team Drives).** Different ownership model; defer to follow-up.
- **Comments / replies / suggestions.** Collaboration features; not normal agent territory.
- **App data folder** (`drive.appdata` scope). Niche.
- **Resumable uploads for large files.** v1 uses multipart upload only; size cap surfaces at the [ADR-0021](0021-attachment-download-policy.md) shared limit. Resumable uploads earn their keep when an operator hits a real limit.
- **Watch / push.** Like Gmail watch — orchestration-level, out of MCP scope.
- **OCR / content extraction.** Per [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) no-smart-tools — the host processes downloaded bytes.

## Options Considered

### `drive.file` vs `drive` default

| Option | Pros | Cons |
| --- | --- | --- |
| (a) Default to `drive` (full) | Maximum capability out of the box | Highest blast radius; operator may not realize the daemon can read every file |
| **(b) Default to `drive.file`; per-account opt-in to `drive`** (chosen) | Safe default; explicit consent for full access; per-account override fits 10-account reality | First-time-setup operator who wants full access has to read INSTALL.md |
| (c) Require explicit scope choice every time, no default | Most explicit | Friction; most operators want the same answer per account |

### `update_permissions` confirm gate

| Option | Pros | Cons |
| --- | --- | --- |
| (d) `dry_run` only, no literal-string confirm | Symmetric with other destructive tools | Public-share is structurally more dangerous than delete; symmetry undersells the risk |
| **(e) Literal `confirm: "yes-share-<file_id>"` + `dry_run`** (chosen) | Structural defense against accidental model invocation; matches [ADR-0019](0019-data-retention-and-purge.md) precedent | Asymmetric with other destructives — documented |
| (f) Require human-in-the-loop callback (out-of-band confirmation) | Strongest defense | Requires the host to implement; the MCP can't enforce it |

### `download_file` vs `get_file_content`

| Option | Pros | Cons |
| --- | --- | --- |
| (g) `get_file_content` returns bytes inline in the response | Single tool, single round-trip | Large files balloon the MCP response payload; size cap forces truncation; host has to base64-decode |
| **(h) `download_file` writes to a path; returns metadata only** (chosen) | Matches `download_attachment` pattern; host gets a file path; large files just work | Two-step "download then read" for the host |

### Permissions listing

| Option | Pros | Cons |
| --- | --- | --- |
| (i) Ship `list_permissions` in v1 | Symmetric with other list tools | Encourages "what does it currently look like" calls separate from `dry_run` |
| **(j) Skip in v1; `update_permissions(dry_run: true)` surfaces current state** (chosen) | Smaller surface; the question "what's current?" funnels through the change tool | Operator who just wants to audit must phrase it as a dry-run |

## Consequences

**Positive:**

- Full Drive workflow is reachable from the MCP with one ADR.
- Safe-by-default `drive.file` scope means a freshly-added Drive account can't accidentally leak every file the operator owns.
- `update_permissions`'s literal `confirm` makes the highest-blast-radius op structurally hard to call by accident.
- `download_file` reuses the [ADR-0021](0021-attachment-download-policy.md) policy code — one policy, tested twice.
- Trash vs permanent-delete is exposed as two tools rather than one with a `permanent: bool` param; the names carry the semantic weight.

**Negative:**

- Fifteen tools is the largest service surface in the corpus; the snapshot diff for "adding Drive" will be substantial.
- The `drive.file` vs `drive` choice puts a real decision in front of the operator at setup time; INSTALL.md must lead with it clearly.
- `list_permissions` absent in v1 may be reopened as a real gap once operators ask "show me sharing without changing it."

**Risks:**

- *Risk:* Agent calls `update_permissions` with the wrong `confirm` string (e.g. literal `"yes-share-FILE_ID"` rather than the actual id substituted) and operators see a confusing error.
  *Mitigation:* The error names the *expected* literal precisely with the file id substituted: `expected "yes-share-1A2B3C..." but got "yes-share-FILE_ID"`. The model gets the recipe in the error.
- *Risk:* Operator widens to `drive` scope for an account, forgets, and later assumes `drive.file` semantics.
  *Mitigation:* `mcp_status` (per [ADR-0014](0014-status-introspection-tool.md) + [ADR-0022](0022-capability-gating.md)) surfaces the effective scope per account; the answer to "what does Drive see right now" is one tool call away.
- *Risk:* `download_file` on a 5 GB video runs out of disk silently.
  *Mitigation:* [ADR-0021](0021-attachment-download-policy.md) size cap applies; the typed error names the cap and the actual file size.
- *Risk:* `delete_file` called instead of `trash_file` on a high-value file (typo at the model layer).
  *Mitigation:* Per [ADR-0012](0012-idempotency-and-dry-run.md) / [ADR-0016](0016-tool-surface-and-conventions.md), `dry_run` defaults to `false` corpus-wide — the safety here is the tool-name distinction itself (`delete_file` vs `trash_file` is a deliberately verbose write) plus the audit-log record. For Drive specifically, given the irreversible nature, **`delete_file` overrides the corpus default and defaults `dry_run` to `true`** (operator must pass `dry_run: false` to actually delete). This is a Drive-only divergence from [ADR-0016](0016-tool-surface-and-conventions.md), documented here and listed in the [ADR-0016](0016-tool-surface-and-conventions.md) §Open / deferred questions amendment queue.
- *Risk:* Google-native file content download surprises agents who expect bytes and get an `ExportRequired` error.
  *Mitigation:* Tool description for `download_file` leads with "Google-native files (Docs/Sheets/Slides) require `export_file` instead — `download_file` returns `ExportRequired` with the suggested export type."

## References

- [ADR-0001](0001-monolithic-google-personal-mcp-architecture.md) — Drive as the third follow-on service
- [ADR-0002](0002-multi-account-architecture.md) — per-account scope and capability override
- [ADR-0005](0005-error-model.md) — typed `ExportRequired` and other Drive-specific variants
- [ADR-0011](0011-audit-log.md) — `update_permissions` audit record is mandatory
- [ADR-0012](0012-idempotency-and-dry-run.md) — `dry_run` on every destructive tool
- [ADR-0014](0014-status-introspection-tool.md) — `mcp_status` reports effective Drive scope per account
- [ADR-0015](0015-tool-versioning-policy.md) — snapshot captures every tool
- [ADR-0016](0016-tool-surface-and-conventions.md) — naming + parameter conventions
- [ADR-0017](0017-secrets-at-rest.md) — token-file perm-check applies unchanged
- [ADR-0018](0018-email-content-trust.md) — untrusted-content wrapping
- [ADR-0019](0019-data-retention-and-purge.md) — literal-string `confirm` precedent for the highest-blast-radius op
- [ADR-0021](0021-attachment-download-policy.md) — `download_file` inherits this policy verbatim
- [ADR-0022](0022-capability-gating.md) — aspect classification + per-account scope override
- Issue [#191](https://github.com/torsday/google-personal-mcp/issues/191) — this spike
- [Drive API v3 reference](https://developers.google.com/drive/api/v3/reference)

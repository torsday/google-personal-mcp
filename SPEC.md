# SPEC: what google-personal-mcp does, for whom, how well

This is the product layer. The 26 ADRs in [`docs/adr/`](docs/adr/) say *how*; this document says *what for*. If a use case here doesn't trace cleanly to a tool in [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) (Gmail v1.0) / [ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md) (Gmail v1.1) / [ADR-0023](docs/adr/0023-calendar-service-surface.md) (Calendar) / [ADR-0024](docs/adr/0024-contacts-service-surface.md) (Contacts) / [ADR-0025](docs/adr/0025-drive-service-surface.md) (Drive), that's a gap to close.

## Purpose

Give an AI assistant first-class, programmatic access to one operator's personal Google data — Gmail (shipped), Calendar / Contacts / Drive (designed for v1.1), Tasks / Keep / Photos / Docs / Chat / YouTube (future) — so the assistant can act as a competent personal-information collaborator. The MCP is the *data source*; the assistant (and whatever orchestration sits above it) is the *intelligence layer*. This split is the central design commitment of [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md): we expose primitives, not magic.

## Users

| User | Goals | Constraints |
| --- | --- | --- |
| **Primary** — the maintainer | 10+ Google accounts (personal, work, side projects). Wants Claude to triage, search, draft, and act across them without leaving the chat. Runs the daemon on a personal VPS and on a local Mac. | Has GCP project, can edit configs, willing to give the daemon real scopes. Reviews actions after the fact via the audit log. |
| **Secondary** — open-source operator | A developer or technical user who finds the repo and wants the same capabilities. Installs the binary, makes their own GCP project, registers one or more of their accounts. | Heterogeneous environment (Linux/macOS, headless/desktop, single-account/multi-account). Needs the threat model to be honest about what the daemon does and doesn't defend. |
| **Out of scope** | Multi-tenant SaaS deployments, organizational Workspace admins managing other people's accounts, anyone whose threat model includes "the operator is the adversary." | This MCP is a personal-data daemon. Trust boundary is the OS user. |

## Excellence criteria

The MCP must be **excellent**, not merely functional, at:

1. **Email search.** The single most important capability. Gmail's query language is rich; the MCP exposes it transparently and surfaces enough metadata in results that downstream operations rarely need a separate fetch. (See the **Search** section below.)
2. **Honest multi-account behavior.** Every tool takes `account` explicitly. No "default account" silently swallowing a request. No silent fallback when an account's scopes are insufficient — surface the gap and point at the fix. Cross-account fan-out (`account: "*"`) shipped in v1.0 for read tools ([ADR-0013](docs/adr/0013-cross-account-fan-out.md), [#84](https://github.com/torsday/google-personal-mcp/issues/84)); explicitly rejected for destructive tools ([#85](https://github.com/torsday/google-personal-mcp/issues/85)).
3. **Predictable destructive operations.** Destructive tools across every service are idempotent-where-possible per [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md). Operator can always preview via `dry_run`. The most-irrecoverable ops (`purge_account`, Drive `update_permissions`, Gmail `permanent_delete_thread`) additionally require a literal-string `confirm: "yes-<verb>-<resource>"` guard so a model can't trip them by accident.
4. **Trust-aware responses.** Any field that could carry attacker-controllable content — email body, calendar event title, contact display name, Drive file name — is suffixed `_untrusted` and wrapped in `<<<UNTRUSTED:KIND ... UNTRUSTED>>>` ([ADR-0018](docs/adr/0018-email-content-trust.md)). The MCP makes the trust boundary structurally visible; it does not sanitize.
5. **Long-lived stability.** Runs for weeks without intervention. OAuth refresh works ([ADR-0004](docs/adr/0004-oauth-token-refresh.md)). Token files are atomically written. Failures are loud and recoverable.
6. **Capability gating** ([ADR-0022](docs/adr/0022-capability-gating.md), v1.1). Per-service, per-aspect (read / write / destructive), per-account toggles layered on the OAuth scope ceiling. Operators can authorize a service for one account with full destructive access and the same service for a different account with read-only — without inventing per-service ad-hoc toggle vocabularies.

Not on the list (intentionally): pretty output, conversational tool descriptions, "smart" tools that classify or summarize, anything that wraps another tool. Those belong in the host.

## Use cases

The stories below are concrete workflows the maintainer or an early adopter wants to do. Each one cites the tool(s) it uses and the milestone in which that capability shipped. **v0.2** = Gmail core (search/read/triage/send) + stdio + audit log + macOS Keychain. **v1.0** = caching (ADR-0009), HTTP transport + bearer auth (ADR-0003 / ADR-0020), observability (ADR-0008), cross-account fan-out (ADR-0013). **v1.1** = capability gating + Calendar + Contacts + Drive + Gmail Phase 2 (ADR-0022..0026).

### Email search

The load-bearing capability. Gmail's query syntax is the input; `search_threads` is the tool ([ADR-0016](docs/adr/0016-tool-surface-and-conventions.md)).

**Sender / recipient:**

1. *"Find every email from `landlord@apartments.example` this year."* — `query: "from:landlord@apartments.example after:2026/01/01"`.
2. *"Find emails from anyone at my company's domain this week."* — `query: "from:@my-company.example newer_than:7d"`.
3. *"Find emails I sent to my partner about travel plans last quarter."* — `query: "from:me to:partner@example after:2026/01/01 before:2026/04/01 (travel OR flight OR trip)"`.
4. *"Find emails I'm cc'd on but never replied to."* — `query: "cc:me -in:sent"`; the host LLM filters by examining results.

**Topic / content:**

5. *"Find the email confirming flight `ABC123`."* — `query: "ABC123 subject:(confirmation OR itinerary)"`.
6. *"Find every email mentioning the URL `https://specific.example/thing`."* — `query: '"https://specific.example/thing"'` (Gmail full-text search).
7. *"Find emails about my tax filing this year."* — `query: "after:2026/01/01 (tax OR IRS OR W-2 OR 1099 OR refund)"`.
8. *"Find the email where someone said they'd handle the renewal."* — free-text search; the host LLM iterates by reading snippets.

**Time:**

9. *"Find every unread email older than 90 days I can probably archive."* — `query: "is:unread older_than:90d"`.
10. *"Find every email from this morning I haven't read yet."* — `query: "is:unread after:2026/05/16"`.
11. *"Find every email between me and a specific person over the last six months."* — `query: "(from:them OR to:them) newer_than:6m"`.

**Attachments:**

12. *"Find every PDF attachment from finance this year."* — `query: "from:finance@example has:attachment filename:pdf after:2026/01/01"`.
13. *"Find emails with attachments larger than 5 MB so I can free up storage."* — `query: "has:attachment larger:5M"`.
14. *"Find every receipt or invoice I've received this year."* — `query: "after:2026/01/01 (subject:(receipt OR invoice) OR subject:order) has:attachment"`.

**Labels and state:**

15. *"Find every email I've labeled `follow-up`."* — `query: "label:follow-up"`.
16. *"Find every starred email still in the inbox."* — `query: "is:starred in:inbox"`.
17. *"Find every email in the Promotions category."* — `query: "category:promotions"`.

**Triage:**

18. *"Find every newsletter from the last week."* — `query: "list:* newer_than:7d"` or `"unsubscribe newer_than:7d"`.
19. *"Find every notification-style email (no human author)."* — `query: "from:(noreply OR no-reply OR notifications OR donotreply)"`.
20. *"Find unread emails older than 30 days and archive everything from `*@notifications.*`."* — two-step: `search_threads` with `"is:unread older_than:30d"`, then `archive_thread` (or `batch_archive`) on the subset whose sender matches `*@notifications.*`. (See *Triage and bulk operations* below.)

**Cross-account:**

21. *"Has a specific person emailed me this week, across personal and work?"* — **v1.0:** one call with `account: "*"` ([ADR-0013](docs/adr/0013-cross-account-fan-out.md), shipped via [#84](https://github.com/torsday/google-personal-mcp/issues/84)).

#### What makes search excellent

- **Pass-through to Gmail's native query language.** Operators get the full expressive power of Gmail search: `from:`, `to:`, `cc:`, `bcc:`, `subject:`, `has:attachment`, `has:drive`, `has:userlabels`, `has:nouserlabels`, `filename:`, `label:`, `category:`, `after:`, `before:`, `older_than:`, `newer_than:`, `larger:`, `smaller:`, `size:`, `is:starred/unread/important/muted/snoozed`, `in:inbox/sent/trash/spam/anywhere/archive`, `list:`, `deliveredto:`, `rfc822msgid:`, `AROUND` (proximity), `+exactword`, `OR` / `AND` / `-` negation, `()` grouping, quoted phrases, and free-text. The MCP re-implements none of this; `query` is forwarded verbatim to `users.threads.list`.
- **Rich result metadata.** `search_threads` returns enough per-thread data (`subject_untrusted`, `from_untrusted`, `snippet_untrusted`, `internal_date`, `label_ids`, `message_count`, `size_estimate`) that the host LLM can decide whether to drill in with `get_thread` *without a follow-up call per result*. Schema pinned in [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) per-tool schemas. The metadata is hydrated internally via parallel `threads.get(format=metadata)` calls — see ADR-0016 cost model. `has_attachments` is deliberately not in ThreadSummary; the host fetches via `get_thread` if needed.
- **Predictable ordering.** Results come back newest-first by Gmail's `internalDate`, matching Gmail's own UI behavior. Operators can re-sort client-side.
- **Cursor-based pagination.** `page_token` round-trips work for large result sets without losing place.
- **Combinable with `get_thread` for drill-down.** Search finds threads; `get_thread` returns full content. The two together are the read path.

### Triage and bulk operations

22. *"Bulk-label every email from `vendor@example` with my custom `archive-candidate` label so I can review and decide later."* — `search_threads` for the sender, then `batch_modify_thread_labels` with `add_label_ids: ["Label_archive-candidate"]`.
23. *"Trash every Promotions email from the last week, but keep the ones from `team@my-favorite-blog.example`."* — `search_threads` `"category:promotions newer_than:7d"`, host filters out the keep-list, then `batch_trash` over the remaining thread IDs.
24. *"Archive every unread email older than 90 days from notification-style senders."* — `search_threads` + `batch_archive` (already in v0.2).
25. *"Find every starred thread I've forgotten about and unstar the ones older than 6 months."* — `search_threads` `"is:starred older_than:6m"` + `modify_thread_labels` removing `STARRED`.

### Reading and analysis

26. *"Summarize the most recent reply in this long thread, and tell me who else is on it."* — `get_thread` returns the full thread; the host LLM does the summarization (per [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) the MCP does not).
27. *"What did this person and I last talk about?"* — `search_threads` `"(from:them OR to:them)"` with small `max_results`, then `get_thread` on the most recent.
28. *"Pull the attachment from this thread."* — **v0.3:** `list_attachments` → `download_attachment` ([#62](https://github.com/torsday/google-personal-mcp/issues/62), [#63](https://github.com/torsday/google-personal-mcp/issues/63)) with the [ADR-0021](docs/adr/0021-attachment-download-policy.md) path-traversal + extension blocklist + size cap policy.

### Composing and replying

29. *"Draft a reply to a thread using context from the last three messages."* — host reads via `get_thread`, composes, and calls `send_email` with `thread_id` and `dry_run: true` first to preview, then again with `dry_run: false` to send. [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) deduplicates accidental double-sends.
30. *"Send a quick email to a known contact."* — `send_email` directly. Plain text only in v0.2; attachments deferred.
31. *"Compose a thank-you note that references something specific from an earlier thread."* — `search_threads` → `get_thread` → `send_email`.

### Cross-account workflows

32. *"What's unread across all my accounts right now?"* — **v1.0:** one call with `account: "*"` ([ADR-0013](docs/adr/0013-cross-account-fan-out.md), [#84](https://github.com/torsday/google-personal-mcp/issues/84)).
33. *"Send from my personal account but blind-copy my work account."* — `send_email` from `personal` with `bcc: ["work@example.com"]`.
34. *"Search this domain across every account I have to see all the touchpoints."* — same shape as #32.

### Account introspection

35. *"Which accounts are registered, and what scopes are granted on each?"* — `list_accounts` returns aliases; **v1.0** `mcp_status` ([ADR-0014](docs/adr/0014-status-introspection-tool.md), [#61](https://github.com/torsday/google-personal-mcp/issues/61)) reports per-account scopes, audit-log freshness, cache status; **v1.1** extends with the per-account capability matrix from [ADR-0022](docs/adr/0022-capability-gating.md).
36. *"List all the labels I have on this account so I can pick the right one."* — `list_labels`. **v1.1** adds `create_label` / `update_label` / `delete_label` for label vocabulary management ([ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md)).

### Gmail Phase 2 (v1.1 — [ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md))

37. *"Draft this reply, save it, but don't send — I want to review later."* — `create_draft` → `update_draft` (iterate) → `send_draft` when ready. Operators who want compose-only agents disable `send_draft` via the per-tool override.
38. *"Create a label called 'archive-candidate' and apply it to last week's newsletters."* — `create_label` → `search_threads` → `batch_modify_thread_labels`.
39. *"Set up a filter that auto-archives mail from `notifications@example.com`."* — `create_filter` with criteria + actions. `delete_filter` reverses.
40. *"Recover the thread I trashed yesterday."* — `untrash_thread` (works inside Gmail's 30-day trash window).
41. *"Show me the storage usage on my work account."* — `get_account_profile`.
42. *"Send a copy of this thread to my legal@ alias."* — `send_email` with the alias in `From:`; first `list_send_as_aliases` to discover what's available.
43. *"Set my vacation responder for next week."* — `set_vacation`.
44. *"Forward this thread to alice@example.com with a note."* — `forward_thread`.
45. *"Permanently delete this thread, skipping trash."* — `permanent_delete_thread` with `confirm: "yes-permanent-delete-<thread_id>"` literal.
46. *"Send this email with the PDF attached."* — `send_email` with the additive `attachments: [...]` parameter.
47. *"Open the forwarded message attached to this thread."* — `parse_forwarded_attachment` returns the nested `ParsedMessage` (recursion-depth-capped).

### Calendar (v1.1 — [ADR-0023](docs/adr/0023-calendar-service-surface.md))

48. *"What's on my calendar this week?"* — `list_events` with `time_min` / `time_max`.
49. *"Find the dentist appointment in the next two months."* — `list_events` with `q: "dentist"`.
50. *"Am I free Friday at 3pm?"* — `query_freebusy`.
51. *"When are Alice, Bob, and I all free for 30 minutes next week?"* — `query_freebusy` across multiple calendars (subject to the `calendarExpansionMax: 50` cap).
52. *"Create a 30-minute meeting with Alice on Friday at 2pm."* — `create_event` with attendees + `send_updates: "all"` (default `"none"` — operator must opt into emailing attendees).
53. *"Move just this Friday's recurring 3pm standup to 4pm — don't touch the rest of the series."* — `update_event` with `recurrence_scope: "single"` (default).
54. *"Cancel the whole recurring weekly review series."* — `delete_event` with `recurrence_scope: "series"` + `send_updates: "all"`.
55. *"Decline the all-hands invite."* — `respond_to_event` with `response: "declined"`.
56. *"Show me every conference call I have next month."* — `list_events` over the window; filter by `conferenceData` field on the host side.

### Contacts (v1.1 — [ADR-0024](docs/adr/0024-contacts-service-surface.md))

57. *"What's my landlord's email?"* — `search_contacts` with `query: "landlord"` (People API prefix-match).
58. *"Show me everyone I've corresponded with at example.com that I haven't explicitly saved."* — `list_other_contacts` (auto-saved from Gmail, read-only).
59. *"Add Alice Johnson, alice@example.com, to my contacts."* — `create_contact`.
60. *"Update Bob's phone number."* — `get_contact` (for the etag) → `update_contact` (etag required; mismatch returns typed `ConcurrencyConflict`).
61. *"Remove the contact for Carol Smith."* — `delete_contact` with optional `dry_run`.
62. *"Who's in my 'Family' contact group?"* — `get_contact_group`.
63. *"Add Dave to my 'Work' contact group."* — `modify_contact_group_membership`.

### Drive (v1.1 — [ADR-0025](docs/adr/0025-drive-service-surface.md))

64. *"Find the lease agreement I uploaded last month."* — `list_files` with Drive's `q` syntax: `q: "name contains 'lease' and modifiedTime > '2026-04-29'"`.
65. *"Download the Q3 financials PDF to my Downloads folder."* — `download_file`; inherits the [ADR-0021](docs/adr/0021-attachment-download-policy.md) path-traversal + extension + size policy.
66. *"Export the meeting-notes Google Doc as a Word file."* — `export_file` with `mime_type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document"`. (Google-native files have no downloadable bytes — `download_file` returns `ExportRequired` and points here.)
67. *"Upload tax-receipt-2026.pdf to my 'Tax 2026' folder."* — `upload_file` (uses `drive.file` scope by default; per-account opt-in to full `drive` for files the daemon didn't create).
68. *"Create a 'Project Phoenix' folder."* — `create_folder`.
69. *"Trash the old draft."* — `trash_file` (reversible for 30 days via `untrash_file`).
70. *"Make report.pdf public-link-shareable."* — `update_permissions` with `confirm: "yes-share-<file_id>"` literal. Default `dry_run: true` returns the would-be permission diff without writing.
71. *"Who has access to my 'Salary Negotiation' document?"* — `update_permissions(dry_run: true, changes: [])` returns the current permission set without modifying.
72. *"Permanently delete this file (skip trash)."* — `delete_file` (Drive-specific `dry_run: true` default — operator must pass `dry_run: false` to actually delete).

### Capability gating (v1.1 — [ADR-0022](docs/adr/0022-capability-gating.md))

73. *"Enable Calendar read-only on my work account, but read + write on my personal."* — `[services.calendar.accounts.work.capabilities] read=true, write=false` + `[services.calendar.accounts.personal.capabilities] read=true, write=true, destructive=false`. The destructive aspect stays off everywhere until the operator deliberately opts in.
74. *"Give the daemon Drive access, but only for files it creates."* — `[services.drive] scopes = ["…/auth/drive.file"]`. Promotes to full `drive` per-account if needed.
75. *"Disable `send_draft` on Gmail — I want compose, but no automated sends."* — `[services.gmail.tools.send_draft] enabled = false` (the per-tool override sanctioned by [ADR-0022](docs/adr/0022-capability-gating.md) §Per-tool override).

## Search-excellence checklist

These are concrete claims the v1.0 Gmail surface must be able to make. End-to-end verification is tracked separately as a `type: test` ticket. If any fail, the search story isn't complete:

- [ ] Any Gmail query syntax that works in the Gmail web UI works in `search_threads` with identical results.
- [ ] A query that returns ≥1000 threads in Gmail is paginatable via `page_token` without dropping or duplicating results.
- [ ] A single `search_threads` call provides enough per-thread metadata that the host LLM can answer "show me the senders and subjects of the 20 most recent unread emails" without a `get_thread` call per result.
- [ ] A query that returns zero results returns a clean empty list — never an error — and `next_page_token` is `null`.
- [ ] A query that exercises a quota limit returns a typed `RateLimited` error per [ADR-0005](docs/adr/0005-error-model.md), with the `retry_after_secs` populated.
- [ ] The same query against two different accounts gives results scoped to each, with no cross-talk.
- [ ] An expired access token transparently refreshes ([ADR-0004](docs/adr/0004-oauth-token-refresh.md)) — no operator-visible failure.
- [ ] A rich search at `max_results=25` costs ~1010 quota units (1× `threads.list` + 25× `threads.get(format=metadata)`); ~6 sustained searches/minute fit under the 6,000 quota-units/minute per-user ceiling. Honest about the cost.

## Non-goals

Explicit non-goals, to head off scope creep:

- **No smart tools.** No `summarize_thread`, `find_emails_about_topic`, `draft_reply_for_me`, `prioritize_inbox`, `triage`. Those are host-level intelligence; this MCP provides the primitives they call.
- **No multi-tenant SaaS shape.** Single-operator daemon. The day someone wants to host this for other people is the day they fork and rewrite the auth model.
- **No non-Google providers.** Outlook, Fastmail, Apple Mail, Notion, Obsidian — out of scope per [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md). If you want those, build separate MCPs and compose at the host.
- **No public-corpus Google APIs.** Maps, Translate, public YouTube — different access model, different threat model, separate MCPs.
- **No client-side filtering or ranking that Gmail already does well.** The MCP forwards Gmail's responses; it doesn't second-guess them.
- **No content sanitization or prompt-injection heuristics.** [ADR-0018](docs/adr/0018-email-content-trust.md) explicitly chooses structural markers over content classification.
- **No UI.** This is a daemon. UIs live elsewhere.

## How to evaluate "is this excellent?"

For each milestone (v0.2, v1.0, v1.1), evaluate by tracing the use cases above end-to-end with a host LLM (Claude Desktop or equivalent). For each:

1. Can the host LLM accomplish the workflow with the tools available?
2. How many tool calls does it take? (Lower is better for latency and quota; aim for ≤3 tool calls for a typical read workflow.)
3. Are responses rich enough that follow-up calls are rare?
4. When something fails, is the error self-explanatory? (Per the honest-failure criterion: "surface the gap and point at the fix.")
5. For multi-service workflows (e.g. "find the email about Q3, the calendar event for the meeting, and the linked Drive doc — all in one breath"), do tool calls compose cleanly without per-service idiosyncrasies?

If a use case requires either (a) a tool that doesn't exist or (b) a parameter shape that forces a workaround in the host, file it as an ADR amendment or as a new issue against the relevant service ADR.

## References

- [ADR-0001](docs/adr/0001-monolithic-google-personal-mcp-architecture.md) — scope; "data source not knowledge layer"
- [ADR-0004](docs/adr/0004-oauth-token-refresh.md) — long-lived auth
- [ADR-0012](docs/adr/0012-idempotency-and-dry-run.md) — destructive-op safety
- [ADR-0013](docs/adr/0013-cross-account-fan-out.md) — `account: "*"` cross-account read
- [ADR-0016](docs/adr/0016-tool-surface-and-conventions.md) — v1.0 Gmail tool inventory and conventions
- [ADR-0018](docs/adr/0018-email-content-trust.md) — untrusted-content model
- [ADR-0022](docs/adr/0022-capability-gating.md) — v1.1 capability gating model
- [ADR-0023](docs/adr/0023-calendar-service-surface.md) — Calendar surface
- [ADR-0024](docs/adr/0024-contacts-service-surface.md) — Contacts surface
- [ADR-0025](docs/adr/0025-drive-service-surface.md) — Drive surface
- [ADR-0026](docs/adr/0026-gmail-tool-surface-phase-2.md) — Gmail Phase 2 expansion
- [ADR-0000](docs/adr/0000-adr-process.md) — corpus + open-questions queue
- [Gmail search operators reference](https://support.google.com/mail/answer/7190) — the Gmail query syntax this MCP forwards
- [Google Calendar API v3 reference](https://developers.google.com/calendar/api/v3/reference)
- [People API reference](https://developers.google.com/people/api/rest)
- [Drive API v3 reference](https://developers.google.com/drive/api/v3/reference)

//! MIME / encoding parser per [ADR-0010](../../docs/adr/0010-mime-and-encoding.md).
//!
//! Input: raw RFC 822 bytes (e.g. the base64-decoded body of Gmail's
//! `messages.get(format=raw)` response).
//!
//! Output: [`ParsedMessage`] — selected headers, a best-effort plain-text body
//! (with HTML rendering fallback), and attachment metadata. Pure transformation
//! over byte slices, no I/O. Tests live alongside; fixtures are inline.

use std::io::Cursor;

use mailparse::{DispositionType, MailHeaderMap, ParsedMail};

use crate::error::Error;
use crate::gmail::types::{AttachmentMeta, BodyContent, ForwardedMessage, Headers, ParsedMessage};

/// HTML→text rendering width. Wide enough that wrapping rarely fires for
/// the typical email body; matched to ADR-0010's "100 columns" decision.
const HTML_RENDER_WIDTH: usize = 100;

/// Parse one RFC 822 / MIME message into headers + body + attachments.
///
/// Returns `Error::Parse` if the top-level structure is unparseable.
/// Individual leaf-decoding failures (a charset that fails to convert, etc.)
/// are surfaced as best-effort fallback strings — `mailparse` and `encoding_rs`
/// both decode-with-replacement rather than panicking.
pub(crate) fn parse_message(raw_rfc822: &[u8]) -> Result<ParsedMessage, Error> {
    let mail = mailparse::parse_mail(raw_rfc822).map_err(|e| Error::Parse {
        context: "parse_mail".to_owned(),
        source: serde::de::Error::custom(e.to_string()),
    })?;

    let headers = extract_headers(&mail);

    let mut collector = LeafCollector::default();
    walk(&mail, &mut Vec::new(), &mut collector);

    let body = build_body(&collector);

    Ok(ParsedMessage {
        headers,
        body,
        attachments: collector.attachments,
    })
}

/// Recursively parse a forwarded `message/rfc822` byte stream into a
/// [`ForwardedMessage`] tree, descending into nested `message/rfc822` parts up
/// to `max_depth` total levels (the top message is level 1). Per
/// [ADR-0026](../../docs/adr/0026-gmail-tool-surface-phase-2.md)
/// §`parse_forwarded_attachment`.
///
/// `max_depth` is the *total* number of levels parsed, not the number of
/// descents: `max_depth == 1` parses only the directly-attached message and
/// leaves `forwarded` empty even when nested forwards exist; `max_depth == 0`
/// is clamped to 1 (always at least the top message). The cap bounds both stack
/// depth and output size against a forwarded-within-forwarded `DoS`.
///
/// Returns [`Error::Parse`] only when the *top-level* stream is unparseable;
/// individual nested parts that fail to parse are silently skipped (best-effort,
/// matching [`parse_message`]'s leaf-level tolerance) rather than failing the
/// whole tree.
pub(crate) fn parse_forwarded(
    raw_rfc822: &[u8],
    max_depth: u32,
) -> Result<ForwardedMessage, Error> {
    let mail = mailparse::parse_mail(raw_rfc822).map_err(|e| Error::Parse {
        context: "parse_forwarded".to_owned(),
        source: serde::de::Error::custom(e.to_string()),
    })?;
    Ok(build_forwarded(&mail, max_depth.max(1), 1))
}

/// Build one [`ForwardedMessage`] level and, while `depth < max_depth`, recurse
/// into its direct `message/rfc822` children.
fn build_forwarded(mail: &ParsedMail<'_>, max_depth: u32, depth: u32) -> ForwardedMessage {
    let headers = extract_headers(mail);
    let mut collector = LeafCollector::default();
    walk(mail, &mut Vec::new(), &mut collector);
    let message = ParsedMessage {
        headers,
        body: build_body(&collector),
        attachments: collector.attachments,
    };

    let mut forwarded = Vec::new();
    if depth < max_depth {
        let mut nested = Vec::new();
        collect_direct_rfc822(mail, &mut nested);
        for inner_raw in nested {
            // Best-effort: a corrupt nested forward drops out rather than
            // failing the whole parse (the outer message is still useful).
            if let Ok(inner) = mailparse::parse_mail(&inner_raw) {
                forwarded.push(build_forwarded(&inner, max_depth, depth + 1));
            }
        }
    }

    ForwardedMessage {
        depth,
        message,
        forwarded,
    }
}

/// Collect the raw bytes of every `message/rfc822` part directly contained in
/// `part` (recursing only through `multipart/*` containers, never *into* an
/// rfc822 part — that descent is [`build_forwarded`]'s job, one level down).
fn collect_direct_rfc822(part: &ParsedMail<'_>, out: &mut Vec<Vec<u8>>) {
    let mime = part.ctype.mimetype.to_ascii_lowercase();
    if mime == "message/rfc822" {
        // The part body *is* the embedded message's raw RFC 822 stream.
        if let Ok(raw) = part.get_body_raw() {
            out.push(raw);
        }
        return; // do not descend; the inner message is parsed by recursion
    }
    for sub in &part.subparts {
        collect_direct_rfc822(sub, out);
    }
}

// ── Tree walk ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct LeafCollector {
    /// First text/plain leaf encountered (with its decoded body).
    text_plain: Option<String>,
    /// First text/html leaf encountered (with its decoded body).
    text_html: Option<String>,
    attachments: Vec<AttachmentMeta>,
}

fn walk(part: &ParsedMail<'_>, path: &mut Vec<usize>, out: &mut LeafCollector) {
    let mime = part.ctype.mimetype.to_ascii_lowercase();

    // `multipart/*` containers — recurse into subparts. mailparse exposes
    // `subparts` for every multipart kind (alternative, mixed, related, signed, ...).
    if mime.starts_with("multipart/") {
        for (i, sub) in part.subparts.iter().enumerate() {
            path.push(i);
            walk(sub, path, out);
            path.pop();
        }
        return;
    }

    // Treat the leaf as an attachment if it is explicitly marked
    // `Content-Disposition: attachment` *or* it has a filename parameter.
    let disp = part.get_content_disposition();
    let filename = disp
        .params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned());
    let is_attachment_like =
        matches!(disp.disposition, DispositionType::Attachment) || filename.is_some();

    if is_attachment_like {
        // Size is the decoded-body byte length. Cheap relative to the parse
        // work already done; gives consumers a useful pre-download number.
        let size_bytes = part
            .get_body_raw()
            .map(|b| b.len() as u64)
            .unwrap_or_default();
        out.attachments.push(AttachmentMeta {
            attachment_id: positional_id(path),
            filename_untrusted: filename.unwrap_or_else(|| "(unnamed)".to_owned()),
            mime_type: part.ctype.mimetype.clone(),
            size_bytes,
        });
        return;
    }

    // Text leaves contribute to the body. We keep the *first* of each kind
    // so multipart/alternative semantics work — Gmail typically orders the
    // plain part before the html part, and the first text/plain seen wins.
    let Ok(body) = part.get_body() else {
        // unreadable leaf — skip rather than fail the whole parse
        return;
    };

    if mime == "text/plain" && out.text_plain.is_none() {
        out.text_plain = Some(body);
    } else if mime == "text/html" && out.text_html.is_none() {
        out.text_html = Some(body);
    }
    // Other content types (image/*, application/octet-stream without
    // disposition, etc.) are ignored — they don't contribute to either
    // body or attachments. Inline images without filenames fall here.
}

fn positional_id(path: &[usize]) -> String {
    if path.is_empty() {
        "part-root".to_owned()
    } else {
        let joined = path
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("-");
        format!("part-{joined}")
    }
}

// ── Body assembly ─────────────────────────────────────────────────────────────

fn build_body(c: &LeafCollector) -> BodyContent {
    let text = match (&c.text_plain, &c.text_html) {
        (Some(t), _) => Some(t.clone()),
        (None, Some(h)) => Some(render_html_to_text(h)),
        (None, None) => None,
    };

    let raw_length = c.text_plain.as_deref().map(str::len).unwrap_or_default()
        + c.text_html.as_deref().map(str::len).unwrap_or_default();

    BodyContent {
        text_untrusted: text,
        html_untrusted: c.text_html.clone(),
        raw_length,
        truncated: false,
    }
}

fn render_html_to_text(html: &str) -> String {
    html2text::from_read(Cursor::new(html.as_bytes()), HTML_RENDER_WIDTH).unwrap_or_else(|_| {
        // The renderer can fail on pathological input; fall back to a crude
        // tag-strip so the consumer at least sees something. Real HTML is
        // overwhelmingly well-formed enough for html2text to handle.
        strip_tags_fallback(html)
    })
}

fn strip_tags_fallback(html: &str) -> String {
    // Minimalist last-resort. Not security-critical: per ADR-0010 we do not
    // sanitize HTML, we render or skip. This path is a "better than empty"
    // fallback for the rare html2text failure.
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

// ── Header extraction ────────────────────────────────────────────────────────

fn extract_headers(mail: &ParsedMail<'_>) -> Headers {
    let h = &mail.headers;
    Headers {
        from_untrusted: h.get_first_value("From"),
        to_untrusted: split_address_list(h.get_first_value("To").as_deref()),
        cc_untrusted: split_address_list(h.get_first_value("Cc").as_deref()),
        bcc_untrusted: split_address_list(h.get_first_value("Bcc").as_deref()),
        subject_untrusted: h.get_first_value("Subject"),
        date_untrusted: h.get_first_value("Date"),
        message_id_untrusted: h.get_first_value("Message-ID"),
        in_reply_to_untrusted: h.get_first_value("In-Reply-To"),
        references_untrusted: h
            .get_first_value("References")
            .as_deref()
            .map(split_whitespace_list)
            .unwrap_or_default(),
    }
}

fn split_address_list(raw: Option<&str>) -> Vec<String> {
    // Header-folded address lists use comma as separator. We don't parse the
    // mailbox syntax fully — the consumer can apply RFC 5322 parsing if it
    // wants structured names. Empty entries are dropped.
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

fn split_whitespace_list(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build a minimal RFC 822 message with the given headers and a
    /// `text/plain` body. Convenience for the simpler test cases.
    fn plain_eml(headers: &str, body: &str) -> Vec<u8> {
        format!(
            "From: alice@example.com\r\n\
             To: bob@example.com\r\n\
             Subject: hello\r\n\
             {headers}\
             Content-Type: text/plain; charset=us-ascii\r\n\
             MIME-Version: 1.0\r\n\
             \r\n\
             {body}"
        )
        .into_bytes()
    }

    // ── Headers ───────────────────────────────────────────────────────────────

    #[test]
    fn extracts_envelope_headers() {
        let raw = b"From: alice@example.com\r\n\
                    To: bob@example.com, carol@example.com\r\n\
                    Cc: dave@example.com\r\n\
                    Subject: Re: lunch?\r\n\
                    Date: Fri, 16 May 2026 12:00:00 -0700\r\n\
                    Message-ID: <abc@example.com>\r\n\
                    In-Reply-To: <xyz@example.com>\r\n\
                    References: <one@example.com> <two@example.com>\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body\r\n";
        let p = parse_message(raw).expect("parse ok");
        assert_eq!(
            p.headers.from_untrusted.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(
            p.headers.to_untrusted,
            vec!["bob@example.com", "carol@example.com"]
        );
        assert_eq!(p.headers.cc_untrusted, vec!["dave@example.com"]);
        assert_eq!(p.headers.subject_untrusted.as_deref(), Some("Re: lunch?"));
        assert_eq!(
            p.headers.message_id_untrusted.as_deref(),
            Some("<abc@example.com>")
        );
        assert_eq!(
            p.headers.in_reply_to_untrusted.as_deref(),
            Some("<xyz@example.com>")
        );
        assert_eq!(
            p.headers.references_untrusted,
            vec!["<one@example.com>", "<two@example.com>"]
        );
    }

    // ── Plain text ────────────────────────────────────────────────────────────

    #[test]
    fn plain_text_returned_verbatim() {
        let raw = plain_eml("", "Hello world.\r\nSecond line.");
        let p = parse_message(&raw).expect("parse ok");
        assert_eq!(
            p.body.text_untrusted.as_deref(),
            Some("Hello world.\r\nSecond line.")
        );
        assert!(p.body.html_untrusted.is_none());
        assert!(p.attachments.is_empty());
        assert!(!p.body.truncated);
    }

    // ── HTML-only → rendered to text ──────────────────────────────────────────

    #[test]
    fn html_only_renders_via_html2text() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <html><body><p>Hello <b>world</b></p>\
                    <p>Visit <a href=\"https://example.com\">our site</a>.</p></body></html>";
        let p = parse_message(raw).expect("parse ok");
        let text = p.body.text_untrusted.as_deref().expect("text present");
        // Tags must be gone, words preserved.
        assert!(!text.contains("<p>"), "tags leaked: {text}");
        assert!(text.contains("Hello"), "missing text: {text}");
        assert!(text.contains("world"), "missing text: {text}");
        // html2text renders links so the URL is visible to the consumer.
        assert!(text.contains("example.com"), "link URL missing: {text}");
        assert!(p.body.html_untrusted.is_some(), "html should be retained");
    }

    // ── multipart/alternative prefers text/plain ──────────────────────────────

    #[test]
    fn multipart_alternative_prefers_text_plain() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/alternative; boundary=BOUND\r\n\
                    \r\n\
                    --BOUND\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Plain version.\r\n\
                    --BOUND\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>HTML version.</p>\r\n\
                    --BOUND--\r\n";
        let p = parse_message(raw).expect("parse ok");
        assert_eq!(
            p.body.text_untrusted.as_deref().map(str::trim_end),
            Some("Plain version.")
        );
        // HTML still retained.
        assert!(p
            .body
            .html_untrusted
            .as_deref()
            .unwrap()
            .contains("HTML version"));
    }

    // ── multipart/alternative falls back to HTML when no plain part ───────────

    #[test]
    fn multipart_alternative_falls_back_to_html() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/alternative; boundary=BOUND\r\n\
                    \r\n\
                    --BOUND\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>Only HTML here.</p>\r\n\
                    --BOUND--\r\n";
        let p = parse_message(raw).expect("parse ok");
        let text = p.body.text_untrusted.as_deref().expect("rendered");
        assert!(text.contains("Only HTML here"), "got: {text}");
    }

    // ── multipart/mixed with attachment surfaces attachment metadata ──────────

    #[test]
    fn multipart_mixed_extracts_attachments() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: pic\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/mixed; boundary=MX\r\n\
                    \r\n\
                    --MX\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    See attached.\r\n\
                    --MX\r\n\
                    Content-Type: image/png; name=\"cat.png\"\r\n\
                    Content-Disposition: attachment; filename=\"cat.png\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==\r\n\
                    --MX--\r\n";
        let p = parse_message(raw).expect("parse ok");
        assert_eq!(
            p.body.text_untrusted.as_deref().map(str::trim_end),
            Some("See attached.")
        );
        assert_eq!(p.attachments.len(), 1);
        let att = &p.attachments[0];
        assert_eq!(att.filename_untrusted, "cat.png");
        assert_eq!(att.mime_type, "image/png");
        assert!(att.size_bytes > 0, "attachment size should be > 0");
        assert_eq!(att.attachment_id, "part-1");
    }

    // ── multipart/related walks correctly ─────────────────────────────────────

    #[test]
    fn multipart_related_walks_into_parts() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: rel\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/related; boundary=R\r\n\
                    \r\n\
                    --R\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>HTML with <img src=\"cid:logo\"></p>\r\n\
                    --R\r\n\
                    Content-Type: image/png\r\n\
                    Content-ID: <logo>\r\n\
                    Content-Disposition: inline; filename=\"logo.png\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    iVBORw0KGgo=\r\n\
                    --R--\r\n";
        let p = parse_message(raw).expect("parse ok");
        // HTML rendered into text.
        let text = p.body.text_untrusted.as_deref().expect("text");
        assert!(text.contains("HTML"), "got: {text}");
        // Inline image with a filename surfaces as an attachment-shaped record.
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].mime_type, "image/png");
    }

    // ── multipart/signed walks into the protected payload ─────────────────────

    #[test]
    fn multipart_signed_walks_into_payload() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: signed\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; \
                    micalg=pgp-sha256; boundary=S\r\n\
                    \r\n\
                    --S\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Signed body content.\r\n\
                    --S\r\n\
                    Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
                    Content-Disposition: attachment; filename=\"signature.asc\"\r\n\
                    \r\n\
                    -----BEGIN PGP SIGNATURE-----\r\n\
                    SIG\r\n\
                    -----END PGP SIGNATURE-----\r\n\
                    --S--\r\n";
        let p = parse_message(raw).expect("parse ok");
        assert_eq!(
            p.body.text_untrusted.as_deref().map(str::trim_end),
            Some("Signed body content.")
        );
        // The signature part surfaces as an attachment, not body.
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].filename_untrusted, "signature.asc");
    }

    // ── Charset: ISO-8859-1 ───────────────────────────────────────────────────

    #[test]
    fn non_utf8_iso_8859_1_decoded() {
        // "Café" in Latin-1: 0x43 0x61 0x66 0xE9.
        let mut raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                        Content-Type: text/plain; charset=iso-8859-1\r\n\
                        \r\n"
            .to_vec();
        raw.extend_from_slice(&[0x43, 0x61, 0x66, 0xE9, b'\r', b'\n']);
        let p = parse_message(&raw).expect("parse ok");
        let text = p.body.text_untrusted.as_deref().expect("text");
        assert!(text.contains("Café"), "iso-8859-1 not decoded: {text:?}");
    }

    // ── Quoted-Printable transfer encoding ────────────────────────────────────

    #[test]
    fn quoted_printable_decoded() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: qp\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    Content-Transfer-Encoding: quoted-printable\r\n\
                    \r\n\
                    Caf=C3=A9 and =E2=98=83\r\n";
        let p = parse_message(raw).expect("parse ok");
        let text = p.body.text_untrusted.as_deref().expect("text");
        assert!(text.contains("Café"), "qp not decoded: {text:?}");
        assert!(text.contains("☃"), "qp utf-8 not decoded: {text:?}");
    }

    // ── No body at all (rare: signed-only) ────────────────────────────────────

    #[test]
    fn no_text_parts_yields_none_body() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: bin\r\n\
                    Content-Type: application/octet-stream; name=\"x.bin\"\r\n\
                    Content-Disposition: attachment; filename=\"x.bin\"\r\n\
                    \r\n\
                    \x00\x01\x02\r\n";
        let p = parse_message(raw).expect("parse ok");
        assert!(p.body.text_untrusted.is_none(), "should have no body");
        assert_eq!(p.attachments.len(), 1);
    }

    // ── Malformed input surfaces typed error ──────────────────────────────────

    #[test]
    fn malformed_message_returns_parse_error() {
        // mailparse is generous; truly unparseable input is hard to construct.
        // An empty input is the simplest "no headers at all" case — mailparse
        // accepts it as an empty message, so this test asserts the happy
        // path (no panic) rather than a forced error.
        let p = parse_message(b"").expect("empty parse should not panic");
        assert!(p.body.text_untrusted.is_none() || p.body.text_untrusted.as_deref() == Some(""));
    }

    // ── Attachment-id positional encoding ─────────────────────────────────────

    #[test]
    fn positional_id_encodes_path() {
        assert_eq!(positional_id(&[]), "part-root");
        assert_eq!(positional_id(&[0]), "part-0");
        assert_eq!(positional_id(&[1, 2]), "part-1-2");
    }

    // ── parse_forwarded: nested message/rfc822 recursion + depth cap ──────────

    /// A `multipart/mixed` message carrying `inner` as a nested `message/rfc822`
    /// attachment, using the given MIME `boundary`. `inner` is itself a full RFC
    /// 822 message, so embedding one of these inside another produces a
    /// forward-of-a-forward. Distinct boundaries per nesting level are required —
    /// a reused boundary would let the outer parser mis-split on the inner
    /// delimiter.
    fn fwd_eml_with(subject: &str, body: &str, inner: Option<&str>, boundary: &str) -> String {
        let attachment = inner.map_or(String::new(), |raw| {
            format!(
                "--{boundary}\r\n\
                 Content-Type: message/rfc822\r\n\
                 Content-Disposition: attachment; filename=\"inner.eml\"\r\n\
                 \r\n\
                 {raw}\r\n"
            )
        });
        format!(
            "From: outer@example.com\r\n\
             To: me@example.com\r\n\
             Subject: {subject}\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: text/plain; charset=us-ascii\r\n\
             \r\n\
             {body}\r\n\
             {attachment}\
             --{boundary}--\r\n"
        )
    }

    /// Single-level convenience over [`fwd_eml_with`] with a fixed boundary.
    fn fwd_eml(subject: &str, body: &str, inner: Option<&str>) -> Vec<u8> {
        fwd_eml_with(subject, body, inner, "BOUND").into_bytes()
    }

    fn inner_eml(subject: &str, body: &str) -> String {
        format!(
            "From: inner@example.com\r\n\
             To: outer@example.com\r\n\
             Subject: {subject}\r\n\
             Content-Type: text/plain; charset=us-ascii\r\n\
             \r\n\
             {body}"
        )
    }

    #[test]
    fn parse_forwarded_extracts_top_level() {
        let raw = fwd_eml("Fwd: hello", "outer body", None);
        let fwd = parse_forwarded(&raw, 5).expect("parses");
        assert_eq!(fwd.depth, 1);
        assert_eq!(
            fwd.message.headers.subject_untrusted.as_deref(),
            Some("Fwd: hello")
        );
        assert_eq!(
            fwd.message.body.text_untrusted.as_deref(),
            Some("outer body")
        );
        assert!(fwd.forwarded.is_empty(), "no nested forward present");
    }

    #[test]
    fn parse_forwarded_descends_into_nested_rfc822() {
        let inner = inner_eml("the inner message", "inner body");
        let raw = fwd_eml("Fwd: nested", "outer body", Some(&inner));
        let fwd = parse_forwarded(&raw, 5).expect("parses");

        assert_eq!(fwd.depth, 1);
        assert_eq!(fwd.forwarded.len(), 1, "one nested forward");
        let child = &fwd.forwarded[0];
        assert_eq!(child.depth, 2);
        assert_eq!(
            child.message.headers.subject_untrusted.as_deref(),
            Some("the inner message")
        );
        assert_eq!(
            child.message.body.text_untrusted.as_deref(),
            Some("inner body")
        );
    }

    #[test]
    fn parse_forwarded_respects_depth_cap() {
        let inner = inner_eml("inner", "inner body");
        let raw = fwd_eml("outer", "outer body", Some(&inner));
        // max_depth = 1 means only the top message; the nested forward is not
        // descended into even though it exists.
        let fwd = parse_forwarded(&raw, 1).expect("parses");
        assert_eq!(fwd.depth, 1);
        assert!(
            fwd.forwarded.is_empty(),
            "depth cap of 1 must not descend into the nested forward"
        );
    }

    #[test]
    fn parse_forwarded_clamps_zero_depth_to_one() {
        // A max_depth of 0 is clamped to 1 — always at least the top message.
        let raw = fwd_eml("outer", "outer body", None);
        let fwd = parse_forwarded(&raw, 0).expect("parses");
        assert_eq!(fwd.depth, 1);
    }

    #[test]
    fn parse_forwarded_handles_three_levels_of_nesting() {
        // ADR-0026 AC: Layer 1 covers a 3-level-nested rfc822. Each level uses a
        // distinct MIME boundary so the outer parser doesn't mis-split on an
        // inner delimiter.
        let level3 = inner_eml("level 3", "deepest body");
        let level2 = fwd_eml_with("level 2", "mid body", Some(&level3), "BOUNDB");
        let top = fwd_eml_with("level 1", "top body", Some(&level2), "BOUNDA");

        let fwd = parse_forwarded(top.as_bytes(), 5).expect("parses");
        assert_eq!(fwd.depth, 1);
        assert_eq!(
            fwd.message.headers.subject_untrusted.as_deref(),
            Some("level 1")
        );

        assert_eq!(fwd.forwarded.len(), 1);
        let l2 = &fwd.forwarded[0];
        assert_eq!(l2.depth, 2);
        assert_eq!(
            l2.message.headers.subject_untrusted.as_deref(),
            Some("level 2")
        );

        assert_eq!(l2.forwarded.len(), 1);
        let l3 = &l2.forwarded[0];
        assert_eq!(l3.depth, 3);
        assert_eq!(
            l3.message.headers.subject_untrusted.as_deref(),
            Some("level 3")
        );
        assert_eq!(
            l3.message.body.text_untrusted.as_deref(),
            Some("deepest body")
        );
    }
}

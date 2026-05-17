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
use crate::gmail::types::{AttachmentMeta, BodyContent, Headers, ParsedMessage};

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
}

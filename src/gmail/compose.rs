//! RFC 2822 message composition for `send_email`.
//!
//! See [ADR-0016](../../docs/adr/0016-tool-surface-and-conventions.md) for the
//! request schema this builds against. The output is a base64url-encoded
//! string suitable for Gmail's `messages.send` `raw` field.
//!
//! Address validation runs **before** any composition: every recipient and
//! sender address is checked for `\r`/`\n`, which would otherwise allow a
//! prompt-injection attacker to inject `Bcc:` and other headers by smuggling
//! CRLF into a user-supplied address (the
//! [`crate::error::Error::HeaderInjection`] guard).

use std::fmt::Write as _;

use base64::Engine;
use chrono::Utc;

use crate::error::Error;

/// Inputs to the composer. All addresses are caller-validated for CR/LF.
#[derive(Debug)]
pub(crate) struct ComposeInput<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body_text: &'a str,
    /// `Message-Id`s of prior thread messages, used to set `References` and
    /// `In-Reply-To`. Empty for new threads.
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [String],
}

/// Compose one message and return its base64url-encoded form.
pub(crate) fn compose_raw(input: &ComposeInput<'_>) -> Result<String, Error> {
    validate_addresses(input)?;
    Error::check_header_field("Subject", input.subject)?;
    let body = compose_2822(input, &Utc::now().to_rfc2822());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.as_bytes()))
}

/// Public for tests; production callers use [`compose_raw`].
pub(crate) fn compose_2822(input: &ComposeInput<'_>, date: &str) -> String {
    let mut buf = String::with_capacity(input.body_text.len() + 256);
    push_header(&mut buf, "Date", date);
    push_header(&mut buf, "From", input.from);
    push_address_header(&mut buf, "To", input.to);
    if !input.cc.is_empty() {
        push_address_header(&mut buf, "Cc", input.cc);
    }
    if !input.bcc.is_empty() {
        push_address_header(&mut buf, "Bcc", input.bcc);
    }
    push_header(&mut buf, "Subject", input.subject);
    push_header(&mut buf, "MIME-Version", "1.0");

    let needs_qp = body_needs_quoted_printable(input.body_text);
    push_header(&mut buf, "Content-Type", "text/plain; charset=utf-8");
    push_header(
        &mut buf,
        "Content-Transfer-Encoding",
        if needs_qp { "quoted-printable" } else { "7bit" },
    );
    if let Some(irt) = input.in_reply_to {
        push_header(&mut buf, "In-Reply-To", irt);
    }
    if !input.references.is_empty() {
        push_header(&mut buf, "References", &input.references.join(" "));
    }
    buf.push_str("\r\n");
    if needs_qp {
        buf.push_str(&encode_quoted_printable(input.body_text));
    } else {
        buf.push_str(input.body_text);
    }
    buf
}

fn push_header(buf: &mut String, name: &str, value: &str) {
    buf.push_str(name);
    buf.push_str(": ");
    buf.push_str(value);
    buf.push_str("\r\n");
}

fn push_address_header(buf: &mut String, name: &str, addrs: &[String]) {
    push_header(buf, name, &addrs.join(", "));
}

fn validate_addresses(input: &ComposeInput<'_>) -> Result<(), Error> {
    Error::check_header_field("From", input.from)?;
    for a in input.to.iter().chain(input.cc).chain(input.bcc) {
        Error::check_header_field("To/Cc/Bcc", a)?;
    }
    if input.to.is_empty() && input.cc.is_empty() && input.bcc.is_empty() {
        return Err(Error::InvalidArgument {
            field: "to".to_owned(),
            detail: "at least one recipient required".to_owned(),
        });
    }
    Ok(())
}

fn body_needs_quoted_printable(body: &str) -> bool {
    // 7bit requires only US-ASCII + lines ≤ 998 chars + no bare CR/LF
    // weirdness. Anything outside US-ASCII forces QP.
    !body.is_ascii() || body.lines().any(|line| line.len() > 998)
}

/// Minimal RFC 2045 §6.7 Quoted-Printable encoder — enough for UTF-8 email
/// bodies. Encodes any non-printable / non-ASCII byte as `=XX`; preserves
/// the existing line breaks; wraps long lines with `=\r\n` soft breaks.
fn encode_quoted_printable(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut line_len: usize = 0;
    for &b in body.as_bytes() {
        match b {
            b'\n' => {
                out.push_str("\r\n");
                line_len = 0;
            }
            b'\r' => {
                // Skip lone CR; the LF case writes the full CRLF.
            }
            // Printable ASCII that doesn't need encoding, except '=' (which
            // is the QP escape char and must always be encoded).
            0x20..=0x3C | 0x3E..=0x7E => {
                if line_len + 1 > 75 {
                    out.push_str("=\r\n");
                    line_len = 0;
                }
                out.push(b as char);
                line_len += 1;
            }
            _ => {
                if line_len + 3 > 75 {
                    out.push_str("=\r\n");
                    line_len = 0;
                }
                let _ = write!(out, "={b:02X}");
                line_len += 3;
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn input<'a>(to: &'a [String], subject: &'a str, body: &'a str) -> ComposeInput<'a> {
        ComposeInput {
            from: "alice@example.com",
            to,
            cc: &[],
            bcc: &[],
            subject,
            body_text: body,
            in_reply_to: None,
            references: &[],
        }
    }

    // ── Address validation (AC) ──────────────────────────────────────────────

    #[test]
    fn rejects_crlf_injection_in_to() {
        let to = vec!["victim@example.com\r\nBcc: attacker@evil.com".to_owned()];
        let err = compose_raw(&input(&to, "hi", "body")).expect_err("must fail");
        assert!(matches!(err, Error::HeaderInjection { .. }), "got: {err:?}");
    }

    #[test]
    fn rejects_crlf_injection_in_subject() {
        let to = vec!["a@x.com".to_owned()];
        let err = compose_raw(&input(&to, "evil\r\nBcc: attacker@evil.com", "body"))
            .expect_err("must fail");
        assert!(matches!(err, Error::HeaderInjection { .. }), "got: {err:?}");
    }

    #[test]
    fn rejects_zero_recipients() {
        let no_to: Vec<String> = vec![];
        let err = compose_raw(&input(&no_to, "hi", "body")).expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument { .. }), "got: {err:?}");
    }

    // ── RFC 2822 output shape (AC: snapshot-ish) ─────────────────────────────

    #[test]
    fn compose_has_required_headers() {
        let to = vec!["b@example.com".to_owned()];
        let composed = compose_2822(&input(&to, "hi", "body"), "Sat, 17 May 2026 12:00:00 +0000");
        assert!(composed.contains("Date: Sat, 17 May 2026 12:00:00 +0000\r\n"));
        assert!(composed.contains("From: alice@example.com\r\n"));
        assert!(composed.contains("To: b@example.com\r\n"));
        assert!(composed.contains("Subject: hi\r\n"));
        assert!(composed.contains("MIME-Version: 1.0\r\n"));
        assert!(composed.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(composed.contains("Content-Transfer-Encoding: 7bit\r\n"));
        // Headers terminated by blank line, then body.
        assert!(composed.ends_with("\r\nbody"));
    }

    #[test]
    fn compose_joins_multiple_addresses() {
        let to = vec!["a@x".to_owned(), "b@y".to_owned()];
        let cc = vec!["c@z".to_owned()];
        let ci = ComposeInput {
            from: "me@x",
            to: &to,
            cc: &cc,
            bcc: &[],
            subject: "s",
            body_text: "body",
            in_reply_to: None,
            references: &[],
        };
        let composed = compose_2822(&ci, "date");
        assert!(composed.contains("To: a@x, b@y\r\n"));
        assert!(composed.contains("Cc: c@z\r\n"));
        assert!(!composed.contains("Bcc:"), "empty bcc omitted");
    }

    #[test]
    fn compose_includes_reply_headers() {
        let to = vec!["a@x".to_owned()];
        let refs = vec!["<m1@x>".to_owned(), "<m2@x>".to_owned()];
        let ci = ComposeInput {
            from: "me@x",
            to: &to,
            cc: &[],
            bcc: &[],
            subject: "Re: hi",
            body_text: "body",
            in_reply_to: Some("<m2@x>"),
            references: &refs,
        };
        let composed = compose_2822(&ci, "date");
        assert!(composed.contains("In-Reply-To: <m2@x>\r\n"));
        assert!(composed.contains("References: <m1@x> <m2@x>\r\n"));
    }

    #[test]
    fn non_ascii_body_uses_quoted_printable() {
        let to = vec!["a@x".to_owned()];
        let ci = ComposeInput {
            from: "me@x",
            to: &to,
            cc: &[],
            bcc: &[],
            subject: "café",
            body_text: "Café ☃",
            in_reply_to: None,
            references: &[],
        };
        let composed = compose_2822(&ci, "date");
        assert!(composed.contains("Content-Transfer-Encoding: quoted-printable\r\n"));
        // 'é' (U+00E9) is 0xC3 0xA9 in UTF-8.
        assert!(composed.contains("Caf=C3=A9"));
    }

    #[test]
    fn body_with_only_ascii_is_7bit() {
        let to = vec!["a@x".to_owned()];
        let composed = compose_2822(&input(&to, "hi", "Plain ASCII body."), "date");
        assert!(composed.contains("Content-Transfer-Encoding: 7bit\r\n"));
    }

    #[test]
    fn compose_raw_returns_base64url_no_pad() {
        let to = vec!["a@x".to_owned()];
        let raw = compose_raw(&input(&to, "hi", "body")).expect("ok");
        // base64url charset only; no padding `=`.
        for c in raw.chars() {
            assert!(
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_'),
                "non-base64url char: {c:?}"
            );
        }
        // Round-trip back to verify shape.
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&raw)
            .expect("decodes");
        let s = std::str::from_utf8(&bytes).expect("utf-8");
        assert!(s.contains("Subject: hi\r\n"));
    }

    // ── QP encoder edge cases ────────────────────────────────────────────────

    #[test]
    fn qp_encodes_equals_sign() {
        let out = encode_quoted_printable("1+1=2");
        assert!(out.contains("=3D"));
        assert!(!out.contains(" =2 "));
    }

    #[test]
    fn qp_preserves_ascii() {
        assert_eq!(encode_quoted_printable("hello"), "hello");
    }

    #[test]
    fn qp_lf_becomes_crlf() {
        assert_eq!(encode_quoted_printable("a\nb"), "a\r\nb");
    }
}

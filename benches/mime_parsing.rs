//! MIME parser throughput benchmark per
//! [ADR-0008 §SLOs](../docs/adr/0008-observability-and-deployment.md).
//!
//! Establishes baselines for [`google_personal_mcp::bench_parse_mime`]
//! (the public bench-only wrapper around `gmail::mime::parse_message`)
//! across the message shapes the parser actually handles in production:
//!
//! - `plain_text_short` — minimal RFC 822, single `text/plain` body.
//!   The 90th-percentile shape of transactional mail.
//! - `plain_text_long` — 50 KB body, single part. Newsletter size.
//! - `multipart_alternative` — `text/plain` + `text/html` alternative,
//!   the dominant shape of marketing / newsletter mail.
//! - `multipart_mixed_with_attachment` — body + one PDF-like attachment.
//!   Tests the tree-walk path that collects attachments.
//!
//! Run with `just bench` or `cargo bench --bench mime_parsing`.
//! Non-regression-gated initially; CI gating to follow once we have a
//! stable baseline and per-PR variance numbers.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use google_personal_mcp::bench_parse_mime;

/// Smallest realistic input — single text/plain body, minimal headers.
fn fixture_plain_short() -> Vec<u8> {
    let body = "Hi,\r\n\r\nQuick note: meeting moved to 2pm.\r\n\r\nThanks,\r\nAlice";
    format!(
        "From: alice@example.com\r\n\
         To: bob@example.com\r\n\
         Subject: Re: meeting\r\n\
         Date: Mon, 22 May 2026 10:00:00 -0700\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {body}",
    )
    .into_bytes()
}

/// Long body — newsletter-shaped. ~50 KB single part.
fn fixture_plain_long() -> Vec<u8> {
    // Synthesize ~50 KB of varied text — repeating a paragraph 200×
    // gets us close enough; the parser doesn't care about the content.
    let paragraph = "This week in distributed systems: we talk about leader election, \
        the perils of split-brain, and why your gossip protocol is probably \
        wrong. Subscribe at https://example.com/newsletter to never miss an \
        update.\r\n\r\n";
    let body: String = paragraph.repeat(200);
    format!(
        "From: news@example.com\r\n\
         To: subscriber@example.com\r\n\
         Subject: This Week in Distributed Systems\r\n\
         Date: Mon, 22 May 2026 10:00:00 -0700\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {body}",
    )
    .into_bytes()
}

/// multipart/alternative — text/plain + text/html. The shape of most
/// transactional / marketing mail in the wild.
fn fixture_multipart_alternative() -> Vec<u8> {
    let plain = "View this in a real client.\r\n";
    let html = "<html><body><h1>Hello</h1><p>This is a test message.</p>\
                <p>Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>\
                </body></html>";
    format!(
        "From: sender@example.com\r\n\
         To: recipient@example.com\r\n\
         Subject: HTML test\r\n\
         Date: Mon, 22 May 2026 10:00:00 -0700\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"BOUND\"\r\n\
         \r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {plain}\r\n\
         --BOUND\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         \r\n\
         {html}\r\n\
         --BOUND--\r\n",
    )
    .into_bytes()
}

/// multipart/mixed with a text body and one base64-encoded attachment.
/// Exercises the tree-walk + attachment-collection path.
fn fixture_multipart_with_attachment() -> Vec<u8> {
    // 4 KB of base64-encoded payload — small enough that decoding cost
    // doesn't dominate, large enough to be realistic.
    let attachment_b64 = "A".repeat(4 * 1024);
    let body = "Please find the report attached.";
    format!(
        "From: sender@example.com\r\n\
         To: recipient@example.com\r\n\
         Subject: Report attached\r\n\
         Date: Mon, 22 May 2026 10:00:00 -0700\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
         \r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {body}\r\n\
         --BOUND\r\n\
         Content-Type: application/pdf; name=\"report.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {attachment_b64}\r\n\
         --BOUND--\r\n",
    )
    .into_bytes()
}

fn bench_mime_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("mime_parsing");

    for (name, fixture) in [
        ("plain_text_short", fixture_plain_short()),
        ("plain_text_long", fixture_plain_long()),
        ("multipart_alternative", fixture_multipart_alternative()),
        (
            "multipart_mixed_with_attachment",
            fixture_multipart_with_attachment(),
        ),
    ] {
        // Throughput in bytes/sec — useful when comparing across fixtures
        // of different size.
        group.throughput(Throughput::Bytes(fixture.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let ok = bench_parse_mime(black_box(&fixture));
                black_box(ok);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_mime_parsing);
criterion_main!(benches);

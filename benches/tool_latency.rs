//! End-to-end tool dispatch latency benchmark per
//! [ADR-0008 §SLOs](../docs/adr/0008-observability-and-deployment.md).
//!
//! `< 500 ms p95 cache-warm` is the target. Cache layer isn't built yet
//! (ADR-0009), so this bench measures dispatch against a *wiremock* Gmail —
//! the result excludes real-network latency but exercises every layer of
//! the daemon stack: tracing spans (#69), rate limiter (#25), token check,
//! request build, retry policy, JSON parse.
//!
//! Three tools per ADR-0008's "tool call latency" SLI: `list_labels`,
//! `get_thread`, `search_threads`. Run with `just bench` or
//! `cargo bench --bench tool_latency`.

#![allow(clippy::expect_used)] // bench-only; surface the construction failure loudly

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use google_personal_mcp::bench_handle::BenchHandle;

/// JSON response shapes large enough to be realistic but small enough that
/// per-iteration deserialize cost doesn't dominate.
const LABELS_RESPONSE: &str = r#"{
  "labels": [
    {"id": "INBOX",   "name": "INBOX",   "type": "system", "messagesTotal": 1234, "messagesUnread": 12, "threadsTotal": 800, "threadsUnread": 8},
    {"id": "SENT",    "name": "SENT",    "type": "system", "messagesTotal": 4321, "messagesUnread": 0,  "threadsTotal": 2100, "threadsUnread": 0},
    {"id": "STARRED", "name": "STARRED", "type": "system", "messagesTotal": 22,   "messagesUnread": 3,  "threadsTotal": 15, "threadsUnread": 3},
    {"id": "Label_1", "name": "Newsletters", "type": "user", "messagesTotal": 654, "messagesUnread": 12, "threadsTotal": 400, "threadsUnread": 8}
  ]
}"#;

const THREAD_RESPONSE: &str = r#"{
  "id": "thr-1",
  "historyId": "1",
  "messages": [
    {
      "id": "msg-1",
      "threadId": "thr-1",
      "labelIds": ["INBOX"],
      "snippet": "Quick reply needed by EOD.",
      "internalDate": "1716393600000",
      "payload": {
        "headers": [
          {"name": "From",       "value": "Alice <alice@example.com>"},
          {"name": "To",         "value": "bob@example.com"},
          {"name": "Subject",    "value": "Quick question"},
          {"name": "Date",       "value": "Mon, 22 May 2026 10:00:00 -0700"},
          {"name": "Message-ID", "value": "<m1@example.com>"}
        ],
        "mimeType": "text/plain",
        "body": {
          "size": 42,
          "data": "UXVpY2sgcmVwbHkgbmVlZGVkIGJ5IEVPRC4K"
        }
      }
    }
  ]
}"#;

const THREADS_LIST_RESPONSE: &str = r#"{
  "threads": [
    {"id": "thr-1", "historyId": "100", "snippet": "Quick reply needed by EOD."},
    {"id": "thr-2", "historyId": "101", "snippet": "Project update for this sprint."},
    {"id": "thr-3", "historyId": "102", "snippet": "Re: scheduling next week."}
  ],
  "resultSizeEstimate": 3
}"#;

/// Spin up a wiremock server pre-loaded with the responses every benched
/// tool will hit. Server tear-down is automatic on drop after the bench
/// run completes.
fn setup_mock_server(runtime: &Runtime) -> MockServer {
    runtime.block_on(async {
        let server = MockServer::start().await;
        // `list_labels` → GET /users/{account}/labels
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/[^/]+/labels$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LABELS_RESPONSE))
            .mount(&server)
            .await;
        // `get_thread` → GET /users/{account}/threads/{id}?format=FULL
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/[^/]+/threads/[^/?]+$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(THREAD_RESPONSE))
            .mount(&server)
            .await;
        // `search_threads` → GET /users/{account}/threads + N parallel
        // `get_thread(metadata)` calls. Both share the same handler — the
        // hydration call also matches the threads/{id} pattern above.
        Mock::given(method("GET"))
            .and(path_regex(r"^/users/[^/]+/threads$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(THREADS_LIST_RESPONSE))
            .mount(&server)
            .await;
        // `/token` is wired but never hit because BenchHandle's TokenState
        // is constructed with `expires_at` an hour out — no refresh fires.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"access_token":"FRESH","expires_in":3600}"#),
            )
            .mount(&server)
            .await;
        server
    })
}

fn bench_tools(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let server = setup_mock_server(&runtime);
    let base_url = server.uri();
    let handle = BenchHandle::new(&base_url, "work");

    let mut group = c.benchmark_group("tool_latency");
    // Wiremock responses are ~instant; criterion's default 5s warm-up + 5s
    // measurement is overkill and pads CI runtime. Trim to 3s each.
    group.warm_up_time(std::time::Duration::from_secs(3));
    group.measurement_time(std::time::Duration::from_secs(3));

    group.bench_function("list_labels", |b| {
        b.to_async(&runtime).iter(|| async {
            let ok = handle.list_labels().await;
            black_box(ok);
        });
    });

    group.bench_function("get_thread", |b| {
        b.to_async(&runtime).iter(|| async {
            let ok = handle.get_thread(black_box("thr-1")).await;
            black_box(ok);
        });
    });

    group.bench_function("search_threads", |b| {
        b.to_async(&runtime).iter(|| async {
            let ok = handle.search_threads(black_box("from:alice")).await;
            black_box(ok);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_tools);
criterion_main!(benches);

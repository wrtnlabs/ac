//! Restart continuity — the reopen scenario of docs/ac-durability.md §6:
//! drive the real host binary against a scripted provider, complete a turn,
//! SIGKILL the host mid-second-turn (the provider stub stalls mid-stream to
//! pin the kill inside window 5.1), restart on the same store, and assert
//! the reopen contract: turn 1 intact, turn 2's *input* present (input-first,
//! §3.1), no partial turn-2 output, and a fresh turn completes.
//!
//! This is "close the machine, open it the next day," simulated honestly.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header;
use axum::response::Response;
use axum::routing::post;
use futures::StreamExt;
use serde_json::json;

// ---------------------------------------------------------------------------
// Provider stub: an OpenAI-compatible SSE endpoint with scripted responses.
// Call 0 = clean turn; call 1 = starts a text delta then stalls (holds the
// connection open forever); call 2+ = clean turn again (the post-restart one).
// ---------------------------------------------------------------------------

async fn start_stub(calls: Arc<AtomicUsize>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub listener");
    let addr = listener.local_addr().expect("stub addr");
    let router = Router::new()
        .route("/api/v1/chat/completions", post(completions))
        .with_state(calls);
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("stub serve");
    });
    format!("http://{addr}/api/v1")
}

async fn completions(State(calls): State<Arc<AtomicUsize>>) -> Response {
    match calls.fetch_add(1, Ordering::SeqCst) {
        0 => clean_turn("Day one reply."),
        1 => stalled_turn("partial-"),
        _ => clean_turn("Day three reply."),
    }
}

fn sse_frame(v: &serde_json::Value) -> String {
    format!("data: {v}\n\n")
}

fn text_delta(text: &str) -> serde_json::Value {
    json!({ "choices": [{ "delta": { "content": text }, "finish_reason": null }] })
}

fn clean_turn(text: &str) -> Response {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        sse_frame(&text_delta(text)),
        sse_frame(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] })),
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .expect("stub response")
}

/// One delta, then the connection hangs open — the turn is pinned mid-stream
/// until the test kills the host process.
fn stalled_turn(text: &str) -> Response {
    let first = sse_frame(&text_delta(text));
    let stream =
        futures::stream::once(async move { Ok::<_, std::convert::Infallible>(Bytes::from(first)) })
            .chain(futures::stream::pending());
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .expect("stub response")
}

// ---------------------------------------------------------------------------
// The host binary under test.
// ---------------------------------------------------------------------------

struct Host {
    child: Child,
    port: u16,
}

impl Host {
    async fn launch(dir: &Path, db: &Path, stub_base: &str) -> Self {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_ac-ai-sdk"))
            .arg("--dir")
            .arg(dir)
            .arg("--db")
            .arg(db)
            .arg("--port")
            .arg(port.to_string())
            .env("OPENROUTER_API_KEY", "test-key")
            .env("AC_OPENROUTER_BASE_URL", stub_base)
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn host binary");
        let mut host = Self { child, port };
        host.wait_ready().await;
        host
    }

    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let client = reqwest::Client::new();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("host binary exited during startup: {status}");
            }
            if let Ok(resp) = client
                .get(format!("http://127.0.0.1:{}/", self.port))
                .send()
                .await
                && resp.status() == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "host did not become ready within 60s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// SIGKILL — no shutdown hook runs; whatever the store holds is what
    /// survives (that's the point).
    fn sigkill(&mut self) {
        self.child.kill().expect("kill host");
        let _ = self.child.wait();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

// ---------------------------------------------------------------------------
// Client helpers.
// ---------------------------------------------------------------------------

fn chat_body(id: &str, text: &str) -> serde_json::Value {
    json!({
        "id": id,
        "message": { "role": "user", "parts": [{ "type": "text", "text": text }] },
    })
}

async fn chat_to_done(client: &reqwest::Client, port: u16, id: &str, text: &str) {
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .json(&chat_body(id, text))
        .send()
        .await
        .expect("POST /api/chat");
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut buf = String::new();
        while !buf.contains("[DONE]") {
            let chunk = stream
                .next()
                .await
                .expect("SSE ended before [DONE]")
                .expect("SSE read");
            buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("turn did not reach [DONE] within 60s");
}

async fn session_raw(client: &reqwest::Client, port: u16, id: &str) -> String {
    client
        .get(format!("http://127.0.0.1:{port}/api/sessions/{id}"))
        .send()
        .await
        .expect("GET /api/sessions/{id}")
        .text()
        .await
        .expect("session body")
}

/// The hydrated transcript as (role, concatenated text-part text) pairs.
async fn transcript(client: &reqwest::Client, port: u16, id: &str) -> Vec<(String, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&session_raw(client, port, id).await).expect("session JSON");
    v["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or_default().to_string();
            let text = m["parts"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p["type"] == "text")
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            (role, text)
        })
        .collect()
}

fn pair(role: &str, text: &str) -> (String, String) {
    (role.to_string(), text.to_string())
}

// ---------------------------------------------------------------------------
// The scenario.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn restart_continuity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).expect("mkdir workspace");
    // The host requires the store outside the sandbox root.
    let db = tmp.path().join("store.db");

    let calls = Arc::new(AtomicUsize::new(0));
    let stub_base = start_stub(calls.clone()).await;
    let client = reqwest::Client::new();

    // Day one: a clean turn completes.
    let mut host = Host::launch(&workspace, &db, &stub_base).await;
    chat_to_done(&client, host.port, "day1", "Hello day one").await;

    // Second turn: the provider stalls after one delta. Read the host's SSE
    // until that delta has been forwarded — proof the turn is mid-stream
    // (window 5.1: input appended, output unsettled) — then SIGKILL. The
    // response stream is held alive across the kill so the host's
    // disconnect-cancel path never runs; death is the only exit.
    let resp = client
        .post(format!("http://127.0.0.1:{}/api/chat", host.port))
        .json(&chat_body("day1", "Hello again"))
        .send()
        .await
        .expect("POST /api/chat");
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut seen = String::new();
        while !seen.contains("partial-") {
            let chunk = stream
                .next()
                .await
                .expect("SSE ended during the stall")
                .expect("SSE read");
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("never observed the stalled turn's first delta");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "turn 2 reached the provider"
    );
    host.sigkill();
    drop(stream);
    drop(host);

    // Reopen: fresh process, same store — nothing in-memory survived.
    let host = Host::launch(&workspace, &db, &stub_base).await;

    let sessions: serde_json::Value = client
        .get(format!("http://127.0.0.1:{}/api/sessions", host.port))
        .send()
        .await
        .expect("GET /api/sessions")
        .json()
        .await
        .expect("sessions JSON");
    let listed = sessions["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .any(|s| s["id"] == "day1");
    assert!(listed, "day1 must survive the kill: {sessions}");

    // Turn 1 intact; turn 2's input present (input-first); no partial output.
    let after_kill = transcript(&client, host.port, "day1").await;
    assert_eq!(
        after_kill,
        vec![
            pair("user", "Hello day one"),
            pair("assistant", "Day one reply."),
            pair("user", "Hello again"),
        ],
    );
    assert!(
        !session_raw(&client, host.port, "day1")
            .await
            .contains("partial-"),
        "no partial turn-2 assistant content may leak into the store"
    );

    // A fresh turn on the reopened store completes.
    chat_to_done(&client, host.port, "day1", "Hello day three").await;

    // Persistence is detached from the response stream — poll for it.
    let want = vec![
        pair("user", "Hello day one"),
        pair("assistant", "Day one reply."),
        pair("user", "Hello again"),
        pair("user", "Hello day three"),
        pair("assistant", "Day three reply."),
    ];
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = transcript(&client, host.port, "day1").await;
        if got == want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "final transcript never settled; last saw {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !session_raw(&client, host.port, "day1")
            .await
            .contains("partial-"),
        "the lost turn's partial output must stay lost"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "no hidden provider retries"
    );
}

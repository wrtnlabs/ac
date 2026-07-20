//! Live probe for the provider-server-tools seam (web search).
//!
//! This is a FIRST-PARTY TEST of a capability that is deliberately NOT a
//! first-party built-in feature: web search is a provider-executed server tool,
//! requested via `CompletionRequest::server_tools`, never a tool in the
//! registry. The probe asserts that when we ask OpenRouter for web search, the
//! model actually searches and the results come back as `Citation` events.
//!
//! It hits the real OpenRouter API, so it is `#[ignore]`d — CI never runs it.
//! Run it explicitly with a key:
//!
//!   OPENROUTER_API_KEY=sk-or-... cargo test -p ac-provider-openrouter \
//!       --test live_web_search -- --ignored --nocapture

use ac_provider::{CompletionRequest, Provider, ServerTool};
use ac_provider_openrouter::OpenRouter;
use ac_types::{CompletionEvent, Message, Role};
use futures::StreamExt;

#[tokio::test]
#[ignore = "hits the live OpenRouter API; requires OPENROUTER_API_KEY"]
async fn web_search_server_tool_yields_citations_live() {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("set OPENROUTER_API_KEY to run the live web-search probe");

    let provider = OpenRouter::new(api_key);

    // Sanity: the provider advertises the capability the host is about to use.
    assert!(provider.supports_server_tool(&ServerTool::WebSearch { max_results: None }));

    let mut request = CompletionRequest::new("openai/gpt-4o-mini");
    request.server_tools.push(ServerTool::WebSearch {
        max_results: Some(5),
    });
    request.messages.push(Message::text(
        Role::User,
        "Search the web: what is the latest stable version of the Rust compiler? \
         Cite your sources.",
    ));

    let mut stream = provider
        .stream_completion(request)
        .await
        .expect("stream opened");

    let mut citations = Vec::new();
    let mut text = String::new();
    let mut saw_stop = false;
    while let Some(item) = stream.next().await {
        match item.expect("stream item") {
            CompletionEvent::Citation(c) => citations.push(c),
            CompletionEvent::Text(t) => text.push_str(&t),
            CompletionEvent::Stop(_) => saw_stop = true,
            _ => {}
        }
    }

    eprintln!("live web-search probe: {} citation(s)", citations.len());
    for c in &citations {
        eprintln!("  - {} {}", c.title.as_deref().unwrap_or(""), c.url);
    }

    assert!(saw_stop, "stream must terminate with a Stop");
    assert!(!text.is_empty(), "model should have produced an answer");
    assert!(
        !citations.is_empty(),
        "web search should have surfaced at least one Citation; got none — text was: {text}"
    );
}

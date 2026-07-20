//! OpenRouter wire crate: hand-rolled chat-completions client over reqwest +
//! eventsource-stream, mapping the OpenAI-compatible SSE stream into the
//! unified [`CompletionEvent`] enum. Owns the parts a generic SDK gets wrong:
//! Anthropic `cache_control` breakpoints, usage accounting, error taxonomy.

use ac_provider::{CompletionRequest, EventStream, Provider, ServerTool, ToolChoice};
use ac_types::{
    Citation, CompletionError, CompletionEvent, ContentPart, Role, StopReason, TokenUsage, ToolUse,
};
use async_stream::try_stream;
use eventsource_stream::Eventsource;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouter {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenRouter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn supports_server_tool(&self, tool: &ServerTool) -> bool {
        matches!(tool, ServerTool::WebSearch { .. })
    }

    fn stream_completion(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>> {
        let http = self.http.clone();
        let api_key = self.api_key.clone();
        let url = format!("{}/chat/completions", self.base_url);
        Box::pin(async move {
            let body = build_body(&request);
            let response = http
                .post(url)
                .bearer_auth(api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| CompletionError::Http(e.to_string()))?;

            let status = response.status();
            if !status.is_success() {
                let retry_after_ms = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|secs| secs * 1000);
                let text = response.text().await.unwrap_or_default();
                return Err(match status.as_u16() {
                    401 | 403 => CompletionError::Auth(text),
                    429 => CompletionError::RateLimited { retry_after_ms },
                    400 => CompletionError::BadRequest(text),
                    500..=599 => CompletionError::Overloaded(text),
                    _ => CompletionError::Http(format!("{status}: {text}")),
                });
            }

            Ok(map_events(response.bytes_stream().eventsource()))
        })
    }
}

fn build_body(request: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": build_messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
    }
    if !request.tools.is_empty() {
        body["tool_choice"] = match &request.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Force(name) => json!({ "type": "function", "function": { "name": name } }),
        };
    }
    // Server-side web search rides OpenRouter's `web` plugin. The model decides
    // 0..N searches; results come back as url_citation annotations (see
    // map_events). ServerTool variants OpenRouter can't do fall through ignored.
    // Accumulate across all requested server tools so none clobbers another.
    let mut plugins: Vec<Value> = Vec::new();
    for tool in &request.server_tools {
        match tool {
            ServerTool::WebSearch { max_results } => {
                let mut plugin = json!({ "id": "web" });
                if let Some(n) = max_results {
                    plugin["max_results"] = json!(n);
                }
                plugins.push(plugin);
            }
        }
    }
    if !plugins.is_empty() {
        body["plugins"] = json!(plugins);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    body
}

fn build_messages(request: &CompletionRequest) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(system) = &request.system {
        let mut part = json!({ "type": "text", "text": system });
        if request.cache_system {
            part["cache_control"] = json!({ "type": "ephemeral" });
        }
        out.push(json!({ "role": "system", "content": [part] }));
    }

    for message in &request.messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut parts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();
        for content in &message.content {
            match content {
                ContentPart::Text { text } => {
                    parts.push(json!({ "type": "text", "text": text }));
                }
                ContentPart::Image { media_type, data } => {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{media_type};base64,{data}") },
                    }));
                }
                ContentPart::ToolUse(tool_use) => {
                    tool_calls.push(json!({
                        "id": tool_use.id,
                        "type": "function",
                        "function": {
                            "name": tool_use.name,
                            "arguments": tool_use.input.to_string(),
                        },
                    }));
                }
                ContentPart::ToolResult(result) => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": result.tool_use_id,
                        "content": result.content,
                    }));
                }
                // Thinking replay over chat completions is a phase-2 concern
                // (signature preservation); dropped for now.
                ContentPart::Thinking { .. } | ContentPart::RedactedThinking { .. } => {}
            }
        }

        if message.cache
            && let Some(last_text) = parts.iter_mut().rev().find(|p| p["type"] == json!("text"))
        {
            last_text["cache_control"] = json!({ "type": "ephemeral" });
        }

        if !parts.is_empty() || !tool_calls.is_empty() {
            let mut wire = json!({ "role": role });
            wire["content"] = if parts.is_empty() {
                Value::Null
            } else {
                Value::Array(parts)
            };
            if !tool_calls.is_empty() {
                wire["tool_calls"] = Value::Array(tool_calls);
            }
            out.push(wire);
        }
        out.extend(tool_results);
    }
    out
}

fn map_events<S, E>(mut sse: S) -> EventStream
where
    S: Stream<Item = Result<eventsource_stream::Event, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    Box::pin(try_stream! {
        let mut pending: Vec<PendingToolCall> = Vec::new();
        let mut finish: Option<String> = None;
        while let Some(frame) = sse.next().await {
            let frame = frame.map_err(|e| CompletionError::Parse(e.to_string()))?;
            if frame.data.trim() == "[DONE]" {
                break;
            }
            let chunk: ChatChunk = serde_json::from_str(&frame.data)
                .map_err(|e| CompletionError::Parse(format!("{e} in: {}", frame.data)))?;
            if let Some(usage) = chunk.usage {
                yield CompletionEvent::UsageUpdate(usage.into());
            }
            for choice in chunk.choices {
                if let Some(text) = choice.delta.content
                    && !text.is_empty()
                {
                    yield CompletionEvent::Text(text);
                }
                if let Some(reasoning) = choice.delta.reasoning
                    && !reasoning.is_empty()
                {
                    yield CompletionEvent::Thinking { text: reasoning, signature: None };
                }
                for annotation in choice.delta.annotations {
                    // A citation is decorative metadata — never let a malformed
                    // or shape-shifted one abort a load-bearing turn. Skip any
                    // without a url rather than failing the whole stream.
                    if let Some(citation) = annotation.url_citation
                        && let Some(url) = citation.url
                    {
                        yield CompletionEvent::Citation(Citation {
                            url,
                            title: citation.title,
                        });
                    }
                }
                for tool_call in choice.delta.tool_calls {
                    if pending.len() <= tool_call.index {
                        pending.resize_with(tool_call.index + 1, PendingToolCall::default);
                    }
                    let slot = &mut pending[tool_call.index];
                    if let Some(id) = tool_call.id {
                        slot.id = id;
                    }
                    if let Some(function) = tool_call.function {
                        if let Some(name) = function.name {
                            slot.name.push_str(&name);
                        }
                        if let Some(arguments) = function.arguments {
                            slot.arguments.push_str(&arguments);
                        }
                    }
                }
                if let Some(reason) = choice.finish_reason {
                    finish = Some(reason);
                }
            }
        }
        for call in pending.drain(..) {
            let input = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).map_err(|e| {
                    CompletionError::Parse(format!("tool input for {}: {e}", call.name))
                })?
            };
            yield CompletionEvent::ToolUse(ToolUse { id: call.id, name: call.name, input });
        }
        let stop = match finish.as_deref() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::EndTurn,
        };
        yield CompletionEvent::Stop(stop);
    })
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChoiceChunk>,
    usage: Option<UsageChunk>,
}

#[derive(Deserialize)]
struct ChoiceChunk {
    #[serde(default)]
    delta: DeltaChunk,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct DeltaChunk {
    content: Option<String>,
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallChunk>,
    /// Web-search citations OpenRouter attaches to the message as it streams.
    #[serde(default)]
    annotations: Vec<AnnotationChunk>,
}

#[derive(Deserialize)]
struct AnnotationChunk {
    url_citation: Option<UrlCitationChunk>,
}

#[derive(Deserialize)]
struct UrlCitationChunk {
    // Lenient on purpose: a citation missing its url is skipped, not fatal.
    url: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct ToolCallChunk {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    function: Option<FunctionChunk>,
}

#[derive(Deserialize)]
struct FunctionChunk {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageChunk {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    prompt_tokens_details: Option<PromptTokensDetails>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl From<UsageChunk> for TokenUsage {
    fn from(usage: UsageChunk) -> Self {
        TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_input_tokens: usage
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::Message;

    #[test]
    fn cache_marks_become_cache_control() {
        let mut request = CompletionRequest::new("test/model");
        request.system = Some("sys".into());
        request.cache_system = true;
        let mut message = Message::text(Role::User, "hello");
        message.cache = true;
        request.messages.push(message);

        let messages = build_messages(&request);
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert_eq!(
            messages[1]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn tool_results_become_tool_role_messages() {
        let mut request = CompletionRequest::new("test/model");
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ac_types::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            })],
            cache: false,
        });
        let messages = build_messages(&request);
        assert_eq!(messages[0]["role"], json!("tool"));
        assert_eq!(messages[0]["tool_call_id"], json!("call_1"));
    }

    // tool_choice only makes sense alongside tools; with no tools declared the
    // key must be absent entirely (some backends reject a dangling tool_choice).
    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let mut request = CompletionRequest::new("test/model");
        request.tool_choice = ToolChoice::Required;
        let body = build_body(&request);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn tool_choice_variants_encode() {
        let spec = ac_types::ToolSpec {
            name: "lookup".into(),
            description: "d".into(),
            input_schema: json!({ "type": "object" }),
        };
        let cases = [
            (ToolChoice::Auto, json!("auto")),
            (ToolChoice::None, json!("none")),
            (ToolChoice::Required, json!("required")),
            (
                ToolChoice::Force("lookup".into()),
                json!({ "type": "function", "function": { "name": "lookup" } }),
            ),
        ];
        for (choice, expected) in cases {
            let mut request = CompletionRequest::new("test/model");
            request.tools.push(spec.clone());
            request.tool_choice = choice;
            assert_eq!(build_body(&request)["tool_choice"], expected);
        }
    }

    // --- provider-server-tools seam (web search) ---
    // Encode side: requesting the WebSearch server tool must add OpenRouter's
    // `web` plugin, and nothing when it isn't requested.
    #[test]
    fn web_search_server_tool_encodes_web_plugin() {
        let mut request = CompletionRequest::new("test/model");
        assert!(build_body(&request).get("plugins").is_none());

        request.server_tools.push(ServerTool::WebSearch {
            max_results: Some(3),
        });
        let body = build_body(&request);
        assert_eq!(body["plugins"][0]["id"], json!("web"));
        assert_eq!(body["plugins"][0]["max_results"], json!(3));
    }

    #[test]
    fn openrouter_advertises_web_search_support() {
        let provider = OpenRouter::new("key");
        assert!(provider.supports_server_tool(&ServerTool::WebSearch { max_results: None }));
    }

    // Decode side: a `url_citation` annotation in the SSE stream must surface as
    // a Citation event — the observable artifact of a server-side search.
    #[tokio::test]
    async fn url_citation_annotation_maps_to_citation_event() {
        fn frame(data: &str) -> eventsource_stream::Event {
            eventsource_stream::Event {
                data: data.into(),
                ..Default::default()
            }
        }
        let frames = vec![
            Ok::<_, std::convert::Infallible>(frame(
                r#"{"choices":[{"delta":{"annotations":[{"type":"url_citation","url_citation":{"url":"https://example.com/a","title":"Example A"}}]}}]}"#,
            )),
            Ok(frame(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            )),
        ];

        let mut stream = map_events(futures::stream::iter(frames));
        let mut citations = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok(CompletionEvent::Citation(c)) = item {
                citations.push(c);
            }
        }
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].url, "https://example.com/a");
        assert_eq!(citations[0].title.as_deref(), Some("Example A"));
    }

    // A malformed citation (no url) must be skipped, not abort the turn — the
    // model's answer and any well-formed citation still come through.
    #[tokio::test]
    async fn malformed_citation_is_skipped_not_fatal() {
        fn frame(data: &str) -> eventsource_stream::Event {
            eventsource_stream::Event {
                data: data.into(),
                ..Default::default()
            }
        }
        let frames = vec![
            Ok::<_, std::convert::Infallible>(frame(
                r#"{"choices":[{"delta":{"annotations":[{"type":"url_citation","url_citation":{"title":"no url here"}}],"content":"answer"}}]}"#,
            )),
            Ok(frame(
                r#"{"choices":[{"delta":{"annotations":[{"type":"url_citation","url_citation":{"url":"https://ok.example"}}]}}]}"#,
            )),
            Ok(frame(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            )),
        ];

        let mut stream = map_events(futures::stream::iter(frames));
        let (mut citations, mut text, mut stopped) = (Vec::new(), String::new(), false);
        while let Some(item) = stream.next().await {
            match item.expect("no frame should error the stream") {
                CompletionEvent::Citation(c) => citations.push(c),
                CompletionEvent::Text(t) => text.push_str(&t),
                CompletionEvent::Stop(_) => stopped = true,
                _ => {}
            }
        }
        assert_eq!(text, "answer", "answer text must survive a bad citation");
        assert!(stopped, "stream must still terminate cleanly");
        assert_eq!(citations.len(), 1, "only the well-formed citation surfaces");
        assert_eq!(citations[0].url, "https://ok.example");
    }
}

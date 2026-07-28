//! OpenRouter wire crate: hand-rolled chat-completions client over reqwest +
//! eventsource-stream, mapping the OpenAI-compatible SSE stream into the
//! unified [`CompletionEvent`] enum. Owns the parts a generic SDK gets wrong:
//! Anthropic `cache_control` breakpoints, usage accounting, error taxonomy.

pub mod images;

use ac_provider::{CompletionRequest, EventStream, Provider, ServerTool, ToolChoice};
use ac_types::{
    CacheMark, Citation, CompletionError, CompletionEvent, ContentPart, Effort, Role, StopReason,
    TokenUsage, ToolUse,
};
use async_stream::try_stream;
use eventsource_stream::Eventsource;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// A rejected extra header ([`OpenRouter::with_extra_header`]): the name or
/// value failed validation. Raised at build time so a malformed header is a
/// typed error where it was written, never a panic (or silent drop) at send.
#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    #[error("invalid header name: {0:?}")]
    InvalidName(String),
    #[error("invalid value for header {0}")]
    InvalidValue(String),
}

pub struct OpenRouter {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    extra_headers: HeaderMap,
    provider_order: Option<Vec<String>>,
}

impl OpenRouter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            extra_headers: HeaderMap::new(),
            provider_order: None,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Add a static header sent on every request (attribution headers, proxy
    /// tokens, …). Validated eagerly — see [`HeaderError`]. Repeated names
    /// append rather than replace, matching HTTP multi-value semantics.
    pub fn with_extra_header(
        mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<Self, HeaderError> {
        let name = HeaderName::from_bytes(name.as_ref().as_bytes())
            .map_err(|_| HeaderError::InvalidName(name.as_ref().to_string()))?;
        let value = HeaderValue::from_str(value.as_ref())
            .map_err(|_| HeaderError::InvalidValue(name.to_string()))?;
        self.extra_headers.append(name, value);
        Ok(self)
    }

    /// Pin which upstream providers may serve requests, in preference order —
    /// emitted as the body's `provider.order`. Unset emits no `provider` key.
    pub fn with_provider_order(mut self, order: Vec<String>) -> Self {
        self.provider_order = Some(order);
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
        let extra_headers = self.extra_headers.clone();
        let provider_order = self.provider_order.clone();
        let url = format!("{}/chat/completions", self.base_url);
        Box::pin(async move {
            let body = build_body(&request, provider_order.as_deref());
            let response = http
                .post(url)
                .headers(extra_headers)
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
                    402 => CompletionError::InsufficientCredits(text),
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

fn build_body(request: &CompletionRequest, provider_order: Option<&[String]>) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": build_messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(order) = provider_order {
        body["provider"] = json!({ "order": order });
    }
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
    // Reasoning effort → OpenRouter's `reasoning.effort` (low/medium/high). The
    // agnostic `Max` collapses to `high` — OpenRouter exposes no level above it
    // ([docs/ac-ultra.md] §3, "the top tier collapses to max at the wire").
    if let Some(effort) = request.effort {
        let level = match effort {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High | Effort::Max => "high",
        };
        body["reasoning"] = json!({ "effort": level });
    }
    body
}

/// The Anthropic-compatible `cache_control` object for a mark, carrying the
/// explicit TTL when the mark pins one.
fn cache_control(mark: CacheMark) -> Value {
    match mark.ttl() {
        Some(ttl) => json!({ "type": "ephemeral", "ttl": ttl.as_str() }),
        None => json!({ "type": "ephemeral" }),
    }
}

fn build_messages(request: &CompletionRequest) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(system) = &request.system {
        let mut part = json!({ "type": "text", "text": system });
        if request.cache_system.is_on() {
            part["cache_control"] = cache_control(request.cache_system);
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

        // The breakpoint must land on the LAST wire message this Message emits.
        // Tool results are re-emitted after (or instead of) the main message as
        // standalone role:"tool" messages, so when any exist the mark goes on
        // the last of those — its plain-string content switched to the
        // parts-array form, the encoding that can carry `cache_control`.
        if message.cache.is_on() {
            if let Some(last) = tool_results.last_mut() {
                let text = last["content"].take();
                last["content"] = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_control(message.cache),
                }]);
            } else if let Some(last_text) =
                parts.iter_mut().rev().find(|p| p["type"] == json!("text"))
            {
                last_text["cache_control"] = cache_control(message.cache);
            }
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
                    // Stream newly-arrived argument bytes as a delta once the
                    // call is identified, so a client can render tool input as
                    // it forms. `emitted` is a byte cursor into `arguments`
                    // (which only ever grows via push_str), so each byte is sent
                    // in exactly one delta, in order: the concatenation of every
                    // args_delta for an id equals the final assembled arguments.
                    // Fragments arriving before id+name are known buffer here and
                    // flush as one delta the moment the call is identified.
                    if !slot.id.is_empty() && !slot.name.is_empty() && slot.emitted < slot.arguments.len() {
                        let args_delta = slot.arguments[slot.emitted..].to_string();
                        slot.emitted = slot.arguments.len();
                        let id = slot.id.clone();
                        let name = slot.name.clone();
                        yield CompletionEvent::ToolCallDelta { id, name, args_delta };
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
    /// Byte cursor into `arguments`: how much has already been emitted as a
    /// [`CompletionEvent::ToolCallDelta`]. Guarantees each byte streams once.
    emitted: usize,
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
    completion_tokens_details: Option<CompletionTokensDetails>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    /// Where OpenRouter reports cache WRITES. The top-level
    /// `cache_creation_input_tokens` is the Anthropic-direct shape and stays a
    /// fallback, but a request routed through OpenRouter carries the count
    /// here — reading only the top-level field reports every cache write as
    /// zero while reads stay correct, which shows up in a cost view as
    /// "caching is off" on a provider where it is demonstrably on.
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<UsageChunk> for TokenUsage {
    fn from(usage: UsageChunk) -> Self {
        let details_write = usage
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cache_write_tokens)
            .unwrap_or(0);
        TokenUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_input_tokens: usage
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            // Nested first (OpenRouter), top-level as the Anthropic-direct
            // fallback; whichever is present wins, so both wire shapes report.
            cache_creation_input_tokens: if details_write > 0 {
                details_write
            } else {
                usage.cache_creation_input_tokens.unwrap_or(0)
            },
            reasoning_tokens: usage
                .completion_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::{CacheTtl, Message, ToolResult};

    #[test]
    fn cache_marks_become_cache_control() {
        let mut request = CompletionRequest::new("test/model");
        request.system = Some("sys".into());
        request.cache_system = CacheMark::On;
        let mut message = Message::text(Role::User, "hello");
        message.cache = CacheMark::On;
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
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            })],
            cache: CacheMark::Off,
        });
        let messages = build_messages(&request);
        assert_eq!(messages[0]["role"], json!("tool"));
        assert_eq!(messages[0]["tool_call_id"], json!("call_1"));
    }

    #[test]
    fn transient_image_sequence_encodes_as_assistant_tool_then_user_image() {
        let mut request = CompletionRequest::new("test/model");
        request.messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse(ac_types::ToolUse {
                    id: "call_1".into(),
                    name: "see".into(),
                    input: json!({}),
                })],
                cache: CacheMark::Off,
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::ToolResult(ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "live envelope".into(),
                    is_error: false,
                })],
                cache: CacheMark::Off,
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                }],
                cache: CacheMark::Off,
            },
        ];

        let messages = build_messages(&request);
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["assistant", "tool", "user"]
        );
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "live envelope");
        assert_eq!(messages[2]["content"][0]["type"], "image_url");
        assert_eq!(
            messages[2]["content"][0]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
    }

    fn tool_result_part(id: &str, content: &str) -> ContentPart {
        ContentPart::ToolResult(ToolResult {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
        })
    }

    // The breakpoint must land on the LAST wire message a marked Message emits.
    // Tool results become standalone role:"tool" messages emitted last, so a
    // tool-results-only marked message puts the mark on the final tool message
    // (parts-array form) — previously it was silently dropped.
    #[test]
    fn cache_mark_on_a_tool_results_only_message_lands_on_the_last_tool_message() {
        let mut request = CompletionRequest::new("test/model");
        request.messages.push(Message {
            role: Role::User,
            content: vec![tool_result_part("c1", "one"), tool_result_part("c2", "two")],
            cache: CacheMark::On,
        });

        let messages = build_messages(&request);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]["content"],
            json!("one"),
            "only the last tool message carries the mark; earlier ones keep string content"
        );
        assert_eq!(messages[1]["tool_call_id"], json!("c2"));
        assert_eq!(
            messages[1]["content"],
            json!([{
                "type": "text",
                "text": "two",
                "cache_control": { "type": "ephemeral" },
            }])
        );
    }

    #[test]
    fn cache_mark_on_a_mixed_text_and_tool_result_message_rides_the_last_emitted_piece() {
        let mut request = CompletionRequest::new("test/model");
        request.messages.push(Message {
            role: Role::User,
            content: vec![
                ContentPart::Text {
                    text: "note".into(),
                },
                tool_result_part("c1", "out"),
            ],
            cache: CacheMark::On,
        });

        let messages = build_messages(&request);
        assert_eq!(messages.len(), 2, "user message first, tool message last");
        assert!(
            messages[0]["content"][0].get("cache_control").is_none(),
            "the text part must NOT carry the mark — it is not the last emitted piece"
        );
        assert_eq!(
            messages[1]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    // Unmarked messages must be untouched by the mark-placement logic —
    // asserted as exact wire JSON, not spot checks.
    #[test]
    fn unmarked_messages_are_unchanged() {
        let mut request = CompletionRequest::new("test/model");
        request.messages.push(Message {
            role: Role::User,
            content: vec![
                ContentPart::Text {
                    text: "note".into(),
                },
                tool_result_part("c1", "out"),
            ],
            cache: CacheMark::Off,
        });

        let messages = build_messages(&request);
        assert_eq!(
            serde_json::to_value(&messages).unwrap(),
            json!([
                { "role": "user", "content": [{ "type": "text", "text": "note" }] },
                { "role": "tool", "tool_call_id": "c1", "content": "out" },
            ])
        );
    }

    #[test]
    fn cache_ttl_encodes_into_cache_control() {
        let mut request = CompletionRequest::new("test/model");
        request.system = Some("sys".into());
        request.cache_system = CacheMark::WithTtl(CacheTtl::OneHour);
        let mut message = Message::text(Role::User, "hello");
        message.cache = CacheMark::WithTtl(CacheTtl::FiveMinutes);
        request.messages.push(message);

        let messages = build_messages(&request);
        assert_eq!(
            messages[0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
        assert_eq!(
            messages[1]["content"][0]["cache_control"],
            json!({ "type": "ephemeral", "ttl": "5m" })
        );
    }

    // tool_choice only makes sense alongside tools; with no tools declared the
    // key must be absent entirely (some backends reject a dangling tool_choice).
    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let mut request = CompletionRequest::new("test/model");
        request.tool_choice = ToolChoice::Required;
        let body = build_body(&request, None);
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
            assert_eq!(build_body(&request, None)["tool_choice"], expected);
        }
    }

    // --- provider-server-tools seam (web search) ---
    // Encode side: requesting the WebSearch server tool must add OpenRouter's
    // `web` plugin, and nothing when it isn't requested.
    #[test]
    fn web_search_server_tool_encodes_web_plugin() {
        let mut request = CompletionRequest::new("test/model");
        assert!(build_body(&request, None).get("plugins").is_none());

        request.server_tools.push(ServerTool::WebSearch {
            max_results: Some(3),
        });
        let body = build_body(&request, None);
        assert_eq!(body["plugins"][0]["id"], json!("web"));
        assert_eq!(body["plugins"][0]["max_results"], json!(3));
    }

    // --- reasoning effort ([docs/ac-ultra.md] §3) ---
    #[test]
    fn effort_encodes_reasoning_and_max_collapses_to_high() {
        // Absent effort adds nothing.
        let request = CompletionRequest::new("test/model");
        assert!(build_body(&request, None).get("reasoning").is_none());

        for (effort, level) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Max, "high"), // the wire collapse — OpenRouter exposes three
        ] {
            let mut request = CompletionRequest::new("test/model");
            request.effort = Some(effort);
            assert_eq!(
                build_body(&request, None)["reasoning"]["effort"],
                json!(level),
                "{effort:?} must map to {level}"
            );
        }
    }

    #[test]
    fn openrouter_advertises_web_search_support() {
        let provider = OpenRouter::new("key");
        assert!(provider.supports_server_tool(&ServerTool::WebSearch { max_results: None }));
    }

    // --- provider routing (`provider.order`) ---
    #[test]
    fn provider_order_emits_the_provider_object_and_is_omitted_when_unset() {
        let request = CompletionRequest::new("test/model");
        assert!(
            build_body(&request, None).get("provider").is_none(),
            "unset order must emit no provider key at all"
        );

        let order = vec!["anthropic".to_string(), "openai".to_string()];
        assert_eq!(
            build_body(&request, Some(&order)),
            json!({
                "model": "test/model",
                "messages": [],
                "stream": true,
                "stream_options": { "include_usage": true },
                "provider": { "order": ["anthropic", "openai"] },
            })
        );
    }

    // --- extra headers ---
    #[test]
    fn malformed_extra_headers_are_rejected_at_the_builder() {
        assert!(matches!(
            OpenRouter::new("key").with_extra_header("bad name\n", "v"),
            Err(HeaderError::InvalidName(_))
        ));
        assert!(matches!(
            OpenRouter::new("key").with_extra_header("x-ok", "bad\nvalue"),
            Err(HeaderError::InvalidValue(_))
        ));
    }

    /// A minimal one-connection HTTP server: consumes one full request, hands
    /// its head (request line + headers) back through the channel, writes
    /// `response`, and closes. Everything stays on 127.0.0.1 — hermetic.
    async fn one_shot_server(response: String) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let head = loop {
                let mut chunk = [0u8; 1024];
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + content_length {
                        break head;
                    }
                }
            };
            let _ = tx.send(head);
            sock.write_all(response.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn extra_headers_reach_the_wire_request() {
        let sse = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                   Connection: close\r\n\r\ndata: [DONE]\n\n";
        let (base_url, head_rx) = one_shot_server(sse.into()).await;

        let provider = OpenRouter::new("key")
            .with_base_url(base_url)
            .with_extra_header("x-attribution", "some-host")
            .unwrap()
            .with_extra_header("http-referer", "https://host.invalid")
            .unwrap();
        let mut stream = provider
            .stream_completion(CompletionRequest::new("test/model"))
            .await
            .expect("request must succeed");
        while stream.next().await.is_some() {}

        let head = head_rx.await.unwrap().to_ascii_lowercase();
        assert!(head.contains("x-attribution: some-host"), "head: {head}");
        assert!(head.contains("http-referer: https://host.invalid"));
        assert!(
            head.contains("authorization: bearer key"),
            "extra headers must not displace auth"
        );
    }

    // --- error taxonomy: 402 ---
    #[tokio::test]
    async fn status_402_maps_to_insufficient_credits() {
        let body = r#"{"error":"credits exhausted"}"#;
        let response = format!(
            "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (base_url, _head_rx) = one_shot_server(response).await;

        let err = OpenRouter::new("key")
            .with_base_url(base_url)
            .stream_completion(CompletionRequest::new("test/model"))
            .await
            .err()
            .expect("402 must be an error");
        assert!(
            matches!(&err, CompletionError::InsufficientCredits(text) if text.contains("credits exhausted")),
            "got: {err:?}"
        );
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

    fn frame(data: &str) -> Result<eventsource_stream::Event, std::convert::Infallible> {
        Ok(eventsource_stream::Event {
            data: data.into(),
            ..Default::default()
        })
    }

    // The load-bearing invariant: concatenating every args_delta emitted for an
    // id equals the final assembled arguments string — no bytes dropped,
    // duplicated, or reordered across arbitrary fragment boundaries, including an
    // empty fragment. The terminal ToolUse still carries the full parsed input.
    #[tokio::test]
    async fn tool_call_argument_fragments_stream_as_deltas() {
        let frames = vec![
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            ),
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#,
            ),
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a."}}]}}]}"#,
            ),
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"txt\"}"}}]}}]}"#,
            ),
            frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        ];

        let mut stream = map_events(futures::stream::iter(frames));
        let mut deltas: Vec<(String, String)> = Vec::new();
        let mut tool_use: Option<ToolUse> = None;
        while let Some(item) = stream.next().await {
            match item.expect("no frame should error the stream") {
                CompletionEvent::ToolCallDelta { id, args_delta, .. } => {
                    deltas.push((id, args_delta))
                }
                CompletionEvent::ToolUse(tu) => tool_use = Some(tu),
                _ => {}
            }
        }
        // The empty fragment produces no delta: 3 non-empty fragments → 3 deltas.
        assert_eq!(
            deltas.len(),
            3,
            "one delta per non-empty fragment, none for the empty one"
        );
        assert!(deltas.iter().all(|(id, _)| id == "c1"));
        let concat: String = deltas.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(
            concat, r#"{"path":"a.txt"}"#,
            "delta concat must equal the assembled arguments"
        );
        let tu = tool_use.expect("the assembled ToolUse still fires");
        assert_eq!(tu.id, "c1");
        assert_eq!(tu.name, "read_file");
        assert_eq!(tu.input, json!({ "path": "a.txt" }));
    }

    // Fragments that arrive before the call's id/name are known must buffer and
    // flush as a single delta the moment the call is identified — never dropped.
    #[tokio::test]
    async fn fragments_before_id_buffer_and_flush_on_identification() {
        let frames = vec![
            // First chunk carries argument bytes but no id/name yet.
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1"}}]}}]}"#,
            ),
            // Identification arrives with the rest of the arguments.
            frame(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"tool","arguments":"}"}}]}}]}"#,
            ),
            frame(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        ];

        let mut stream = map_events(futures::stream::iter(frames));
        let mut deltas: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            if let Ok(CompletionEvent::ToolCallDelta { args_delta, .. }) = item {
                deltas.push(args_delta);
            }
        }
        assert_eq!(
            deltas.len(),
            1,
            "buffered prefix flushes as one delta at identification"
        );
        assert_eq!(deltas[0], r#"{"x":1}"#, "the buffered prefix is not lost");
    }

    // --- usage accounting: completion_tokens_details ---
    #[tokio::test]
    async fn usage_chunk_surfaces_reasoning_tokens() {
        let frames = vec![
            frame(
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"completion_tokens_details":{"reasoning_tokens":7}}}"#,
            ),
            frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        ];

        let mut stream = map_events(futures::stream::iter(frames));
        let mut usage: Option<TokenUsage> = None;
        while let Some(item) = stream.next().await {
            if let Ok(CompletionEvent::UsageUpdate(u)) = item {
                usage = Some(u);
            }
        }
        let u = usage.expect("usage event must surface");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.reasoning_tokens, 7);

        // A chunk without the details block stays at zero, not an error.
        let frames = vec![
            frame(r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#),
            frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        ];
        let mut stream = map_events(futures::stream::iter(frames));
        let mut usage: Option<TokenUsage> = None;
        while let Some(item) = stream.next().await {
            if let Ok(CompletionEvent::UsageUpdate(u)) = item {
                usage = Some(u);
            }
        }
        assert_eq!(usage.expect("usage event").reasoning_tokens, 0);
    }

    /// Cache writes arrive under two different wire shapes and BOTH must
    /// report. Reading only the top-level `cache_creation_input_tokens` (the
    /// Anthropic-direct shape) silently reported zero writes for every
    /// OpenRouter-routed request while reads were correct — a cost view that
    /// says caching never engages on a provider where it demonstrably does.
    #[tokio::test]
    async fn cache_writes_report_under_both_wire_shapes() {
        async fn usage_of(json: &str) -> TokenUsage {
            let frames = vec![
                frame(json),
                frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            ];
            let mut stream = map_events(futures::stream::iter(frames));
            let mut usage: Option<TokenUsage> = None;
            while let Some(item) = stream.next().await {
                if let Ok(CompletionEvent::UsageUpdate(u)) = item {
                    usage = Some(u);
                }
            }
            usage.expect("usage event must surface")
        }

        // OpenRouter: nested under prompt_tokens_details.
        let u = usage_of(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":60}}}"#,
        )
        .await;
        assert_eq!(u.cache_read_input_tokens, 40);
        assert_eq!(
            u.cache_creation_input_tokens, 60,
            "OpenRouter reports cache writes nested; reading only the top-level field zeroes them"
        );

        // Anthropic-direct: top-level, still honored.
        let u = usage_of(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,
                "cache_creation_input_tokens":77}}"#,
        )
        .await;
        assert_eq!(u.cache_creation_input_tokens, 77);

        // Neither present is zero, not an error.
        let u = usage_of(
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,
                "prompt_tokens_details":{"cached_tokens":1}}}"#,
        )
        .await;
        assert_eq!(u.cache_creation_input_tokens, 0);
    }
}

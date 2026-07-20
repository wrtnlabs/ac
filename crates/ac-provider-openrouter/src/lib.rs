//! OpenRouter wire crate: hand-rolled chat-completions client over reqwest +
//! eventsource-stream, mapping the OpenAI-compatible SSE stream into the
//! unified [`CompletionEvent`] enum. Owns the parts a generic SDK gets wrong:
//! Anthropic `cache_control` breakpoints, usage accounting, error taxonomy.

use ac_provider::{CompletionRequest, EventStream, Provider};
use ac_types::{
    CompletionError, CompletionEvent, ContentPart, Role, StopReason, TokenUsage, ToolUse,
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
}

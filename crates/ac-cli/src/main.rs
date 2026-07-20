//! The generic host. Phase 1: stream a raw completion. This binary is the
//! standing proof that AC works for a host with no app attached — it must
//! never grow consumer-specific behavior.

use ac_provider::{CompletionRequest, Provider};
use ac_provider_openrouter::OpenRouter;
use ac_types::{CompletionEvent, Message, Role};
use futures::StreamExt;
use std::io::Write;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut model = "anthropic/claude-haiku-4.5".to_string();
    if args.first().map(String::as_str) == Some("--model") {
        args.remove(0);
        if args.is_empty() {
            anyhow::bail!("--model needs a value");
        }
        model = args.remove(0);
    }
    let prompt = args.join(" ");
    if prompt.is_empty() {
        anyhow::bail!("usage: ac [--model <id>] <prompt>");
    }

    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY is not set"))?;
    let provider = OpenRouter::new(api_key);

    let mut request = CompletionRequest::new(model);
    request.messages.push(Message::text(Role::User, prompt));

    let mut stream = provider.stream_completion(request).await?;
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event? {
            CompletionEvent::Text(text) => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            CompletionEvent::Thinking { text, .. } => eprint!("{text}"),
            CompletionEvent::ToolUse(tool_use) => {
                eprintln!("\n[tool_use {} {}]", tool_use.name, tool_use.input);
            }
            CompletionEvent::UsageUpdate(u) => usage = Some(u),
            CompletionEvent::Stop(reason) => {
                println!();
                eprintln!("[stop: {reason:?}, usage: {usage:?}]");
            }
        }
    }
    Ok(())
}

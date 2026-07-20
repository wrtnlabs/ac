//! `ac` — the generic agent host binary. A thin shell over
//! [`ac_cli::build_host`]: parse args, pick the OpenRouter provider, wire
//! Ctrl-C to the run's cancel token, and render the event stream. All the
//! actual wiring lives in the library so the tests exercise what ships.

use std::path::PathBuf;
use std::sync::Arc;

use ac_cli::build_host;
use ac_provider_openrouter::OpenRouter;
use ac_runtime::AgentEvent;
use ac_types::{StopReason, TokenUsage};
use tokio::sync::mpsc;

struct Args {
    model: String,
    dir: PathBuf,
    prompt: String,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut model = "anthropic/claude-haiku-4.5".to_string();
    let mut dir: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--model" => {
                model = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--model needs a value"))?;
            }
            "--dir" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--dir needs a value"))?;
                dir = Some(PathBuf::from(v));
            }
            _ => rest.push(arg),
        }
    }

    let prompt = rest.join(" ");
    if prompt.trim().is_empty() {
        anyhow::bail!("usage: ac [--model <id>] [--dir <path>] <prompt...>");
    }

    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };

    Ok(Args { model, dir, prompt })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;

    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY is not set"))?;

    let provider = Arc::new(OpenRouter::new(api_key));
    let host = build_host(provider, &args.dir, args.model)?;
    let mut session = host.session;

    // Ctrl-C cancels the loop and any running shell via the shared token.
    let cancel_ctx = host.ctx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n\x1b[2m· interrupt — cancelling\x1b[0m");
            cancel_ctx.cancel.cancel();
        }
    });

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();

    let prompt = args.prompt;
    let handle = tokio::spawn(async move { session.run_turn(prompt, tx).await });

    let mut usage: Option<TokenUsage> = None;
    while let Some(event) = rx.recv().await {
        render(event, &mut usage);
    }

    // The sink is dropped once the loop ends; join the driver for its result.
    let result = handle.await?;
    match result {
        Ok(stop) => {
            summary(stop, usage);
            Ok(())
        }
        Err(e) => {
            eprintln!("\x1b[31m✗ {e}\x1b[0m");
            std::process::exit(1);
        }
    }
}

fn render(event: AgentEvent, usage: &mut Option<TokenUsage>) {
    use std::io::Write;
    match event {
        AgentEvent::Text(s) => {
            print!("{s}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::Thinking(_) => {}
        AgentEvent::ToolCall { name, input, .. } => {
            eprintln!("\x1b[2m· {name}({})\x1b[0m", compact(&input));
        }
        AgentEvent::ToolResult {
            output, is_error, ..
        } => {
            let tag = if is_error { "ERR" } else { "ok" };
            eprintln!("\x1b[2m  ↳ {tag} ({} bytes)\x1b[0m", output.len());
        }
        AgentEvent::Usage(u) => *usage = Some(u),
        AgentEvent::TurnComplete { .. } => {}
        AgentEvent::Error(e) => {
            eprintln!("\x1b[31m✗ {e}\x1b[0m");
        }
    }
}

fn summary(stop: StopReason, usage: Option<TokenUsage>) {
    println!();
    match usage {
        Some(u) => eprintln!(
            "\x1b[2m[stop: {stop:?} · in {} out {} cache_read {} cache_write {}]\x1b[0m",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens
        ),
        None => eprintln!("\x1b[2m[stop: {stop:?}]\x1b[0m"),
    }
}

/// Render a tool input as a short single-line string for the trace.
fn compact(input: &serde_json::Value) -> String {
    let s = input.to_string();
    const MAX: usize = 120;
    if s.chars().count() > MAX {
        let truncated: String = s.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

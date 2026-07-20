//! The `fetch` tool: HTTP(S) GET a URL and return its body as text.
//!
//! This is the one built-in that reaches the network. Only `http`/`https` are
//! allowed; the body is read as text and capped. There is no host-side URL
//! allowlist in this phase — a host that must restrict egress should not
//! register this tool.

use std::sync::Arc;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

/// Maximum number of bytes read from a response body.
const FETCH_CAP: usize = 256 * 1024;

/// Fetch a URL over HTTP(S) and return the response body as text.
///
/// Only `http` and `https` URLs are allowed. The body is read as UTF-8 text and
/// truncated at 256 KiB. Non-success HTTP statuses are reported as errors.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct FetchInput {
    /// The absolute `http`/`https` URL to GET.
    pub url: String,
}

/// Fetches a URL over HTTP(S) (network access).
pub struct Fetch;

impl Tool for Fetch {
    type Input = FetchInput;

    fn name(&self) -> &'static str {
        "fetch"
    }

    fn description(&self) -> String {
        "HTTP(S) GET a URL and return the response body as text (capped at 256 \
         KiB). Only http/https URLs are allowed. This tool accesses the \
         network."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let parsed = match reqwest::Url::parse(&input.url) {
                Ok(u) => u,
                Err(e) => return ToolOutput::error(format!("invalid url: {e}")),
            };
            match parsed.scheme() {
                "http" | "https" => {}
                other => {
                    return ToolOutput::error(format!(
                        "unsupported url scheme '{other}': only http/https are allowed"
                    ));
                }
            }

            let response = match reqwest::get(parsed).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("request failed: {e}")),
            };

            let status = response.status();
            if !status.is_success() {
                return ToolOutput::error(format!("HTTP {status}"));
            }

            let (body, truncated) = match read_body_capped(response, FETCH_CAP).await {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(format!("failed to read body: {e}")),
            };

            let mut text = String::from_utf8_lossy(&body).into_owned();
            if truncated {
                text.push_str(&format!("\n\n[truncated: body exceeds {FETCH_CAP} bytes]"));
            }
            ToolOutput::ok(text)
        })
    }
}

async fn read_body_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    use futures::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() >= cap {
            truncated = true;
            break;
        }
        let remaining = cap - buf.len();
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, truncated))
}

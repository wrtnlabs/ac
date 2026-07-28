//! The `fetch` tool: HTTP(S) GET a URL and return its body as text.
//!
//! This is the one built-in that reaches the network. Hosts may expose it
//! unrestricted, omit it entirely, or inject a [`FetchUrlPolicy`] that limits
//! which URLs it may reach. The policy is checked before the first socket is
//! opened and again for every redirect target.

use std::{sync::Arc, time::Duration};

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use reqwest::StatusCode;
use serde::Deserialize;

/// Parsed URL type passed to a host's [`FetchUrlPolicy`].
pub type FetchUrl = reqwest::Url;

/// Maximum number of bytes read from a response body.
const FETCH_CAP: usize = 256 * 1024;

/// Hard wall-clock bound for the full redirect/request/body sequence.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Match reqwest's normal redirect ceiling while keeping redirects under the
/// host's URL policy.
const MAX_REDIRECTS: usize = 10;

/// Host policy for URLs the generic [`Fetch`] tool may reach.
///
/// The same policy is evaluated for the initial URL and every redirect
/// target, before that target is requested. Returning `Err` refuses the
/// request and exposes the reason as tool-error data.
pub trait FetchUrlPolicy: Send + Sync {
    fn check(&self, url: &FetchUrl) -> Result<(), String>;
}

impl<F> FetchUrlPolicy for F
where
    F: Fn(&FetchUrl) -> Result<(), String> + Send + Sync,
{
    fn check(&self, url: &FetchUrl) -> Result<(), String> {
        self(url)
    }
}

/// Exact HTTP(S) origins admitted by a [`FetchUrlPolicy`].
///
/// Origins include scheme, host, and effective port. `https://example.com`
/// and `https://example.com:443` are therefore the same origin, while HTTP,
/// a subdomain, or a non-default port are different origins. Credentials in
/// either configured origins or requested URLs are refused.
#[derive(Clone, Debug)]
pub struct AllowedOrigins {
    origins: Vec<String>,
}

impl AllowedOrigins {
    /// Build an exact-origin allowlist.
    ///
    /// Each entry must be an absolute HTTP(S) origin with no path, query,
    /// fragment, or user credentials.
    pub fn new<I, S>(origins: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed_origins = Vec::new();
        for raw in origins {
            let raw = raw.as_ref();
            let url = FetchUrl::parse(raw)
                .map_err(|e| format!("invalid allowed fetch origin `{raw}`: {e}"))?;
            validate_http_url(&url)
                .map_err(|e| format!("invalid allowed fetch origin `{raw}`: {e}"))?;
            if !url.username().is_empty() || url.password().is_some() {
                return Err(format!(
                    "invalid allowed fetch origin `{raw}`: credentials are not allowed"
                ));
            }
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return Err(format!(
                    "invalid allowed fetch origin `{raw}`: expected only scheme, host, and optional port"
                ));
            }
            let origin = url.origin().ascii_serialization();
            if origin == "null" {
                return Err(format!(
                    "invalid allowed fetch origin `{raw}`: opaque origins are not allowed"
                ));
            }
            if !parsed_origins.contains(&origin) {
                parsed_origins.push(origin);
            }
        }
        Ok(Self {
            origins: parsed_origins,
        })
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }
}

impl FetchUrlPolicy for AllowedOrigins {
    fn check(&self, url: &FetchUrl) -> Result<(), String> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err("URL credentials are not allowed".into());
        }
        let origin = url.origin().ascii_serialization();
        if self.origins.contains(&origin) {
            Ok(())
        } else {
            Err(format!("origin `{origin}` is not allowed"))
        }
    }
}

struct AllowAll;

impl FetchUrlPolicy for AllowAll {
    fn check(&self, _url: &FetchUrl) -> Result<(), String> {
        Ok(())
    }
}

/// Fetch a URL over HTTP(S) and return the response body as text.
///
/// Only `http` and `https` URLs are allowed. The body is read as UTF-8 text and
/// truncated at 256 KiB. Non-success HTTP statuses are reported as errors. The
/// complete operation has a 30-second default timeout and honors the run's
/// cancellation signal.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct FetchInput {
    /// The absolute `http`/`https` URL to GET.
    pub url: String,
}

/// Fetches a URL over HTTP(S) (network access).
pub struct Fetch {
    http: reqwest::Client,
    policy: Arc<dyn FetchUrlPolicy>,
    timeout: Duration,
}

impl Default for Fetch {
    fn default() -> Self {
        Self::with_shared_policy(Arc::new(AllowAll))
    }
}

impl Fetch {
    /// An unrestricted HTTP(S) fetch tool.
    pub fn unrestricted() -> Self {
        Self::default()
    }

    /// A fetch tool governed by a host-supplied URL policy.
    pub fn with_policy(policy: impl FetchUrlPolicy + 'static) -> Self {
        Self::with_shared_policy(Arc::new(policy))
    }

    /// A fetch tool governed by a shared host-supplied URL policy.
    pub fn with_shared_policy(policy: Arc<dyn FetchUrlPolicy>) -> Self {
        // Redirects are handled below so the policy is checked before every
        // hop. Falling back to Client::new() would silently restore automatic
        // redirect following and fail this security control open.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client builder (manual redirects)");
        Self {
            http,
            policy,
            timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }

    /// Override the hard wall-clock bound for the full operation.
    ///
    /// The timeout covers all redirect hops, request setup, response headers,
    /// and response-body streaming. It is not reset between redirects.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn get(&self, initial: FetchUrl) -> Result<reqwest::Response, String> {
        let mut current = initial;
        for redirects in 0..=MAX_REDIRECTS {
            validate_http_url(&current)?;
            self.policy
                .check(&current)
                .map_err(|e| format!("URL refused by host fetch policy: {e}"))?;

            let response = self
                .http
                .get(current.clone())
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;

            if !follows_redirect(response.status()) {
                return Ok(response);
            }
            if redirects == MAX_REDIRECTS {
                return Err(format!(
                    "too many redirects: exceeded {MAX_REDIRECTS} redirects"
                ));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| {
                    format!("HTTP {} redirect has no Location header", response.status())
                })?
                .to_str()
                .map_err(|e| format!("invalid redirect Location header: {e}"))?;
            current = current
                .join(location)
                .map_err(|e| format!("invalid redirect target `{location}`: {e}"))?;
            // The next loop iteration validates both scheme and host policy
            // before sending this redirect target.
        }
        unreachable!("redirect loop returns or advances within its bound")
    }

    async fn fetch_text(&self, initial: FetchUrl) -> Result<String, String> {
        let response = self.get(initial).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }

        let (body, truncated) = read_body_capped(response, FETCH_CAP)
            .await
            .map_err(|e| format!("failed to read body: {e}"))?;

        let mut text = String::from_utf8_lossy(&body).into_owned();
        if truncated {
            text.push_str(&format!("\n\n[truncated: body exceeds {FETCH_CAP} bytes]"));
        }
        Ok(text)
    }
}

fn validate_http_url(url: &FetchUrl) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "unsupported url scheme '{other}': only http/https are allowed"
        )),
    }
}

fn follows_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

impl Tool for Fetch {
    type Input = FetchInput;

    fn name(&self) -> &'static str {
        "fetch"
    }

    fn description(&self) -> String {
        "HTTP(S) GET a URL and return the response body as text (capped at 256 \
         KiB). Only http/https URLs admitted by the host's fetch policy are \
         allowed. This tool accesses the network."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let parsed = match FetchUrl::parse(&input.url) {
                Ok(u) => u,
                Err(e) => return ToolOutput::error(format!("invalid url: {e}")),
            };
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => ToolOutput::error("fetch cancelled"),
                _ = tokio::time::sleep(self.timeout) => ToolOutput::error(format!(
                    "fetch timed out after {} ms",
                    self.timeout.as_millis()
                )),
                result = self.fetch_text(parsed) => match result {
                    Ok(text) => ToolOutput::ok(text),
                    Err(error) => ToolOutput::error(error),
                },
            }
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use ac_tool::{SubtreePolicy, ToolCtx};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    fn ctx() -> Arc<ToolCtx> {
        let dir = tempfile::tempdir().unwrap().keep();
        Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(dir).unwrap())))
    }

    async fn one_request_server(response: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn allowed_origins_are_exact_and_normalized() {
        let policy = AllowedOrigins::new([
            "https://api.example.test",
            "https://api.example.test:443",
            "http://api.example.test:8080",
        ])
        .unwrap();
        assert_eq!(policy.origins().len(), 2);
        for allowed in [
            "https://api.example.test/v1/search",
            "https://api.example.test:443/download/item.json",
            "http://api.example.test:8080/x",
        ] {
            assert!(
                policy.check(&FetchUrl::parse(allowed).unwrap()).is_ok(),
                "{allowed}"
            );
        }
        for refused in [
            "http://api.example.test/x",
            "https://cdn.example.test/x",
            "https://api.example.test:8443/x",
            "https://user@api.example.test/x",
        ] {
            assert!(
                policy.check(&FetchUrl::parse(refused).unwrap()).is_err(),
                "{refused}"
            );
        }
        assert!(AllowedOrigins::new(["https://api.example.test/path"]).is_err());
        assert!(AllowedOrigins::new(["file:///tmp/item.json"]).is_err());
    }

    #[tokio::test]
    async fn allowed_same_origin_redirect_is_followed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://{address}");
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nitem",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        let tool = Arc::new(Fetch::with_policy(
            AllowedOrigins::new([origin.as_str()]).unwrap(),
        ));
        let output = tool
            .run(
                FetchInput {
                    url: format!("{origin}/start"),
                },
                ctx(),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "item");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn disallowed_redirect_target_is_never_requested() {
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target_hit = Arc::new(AtomicBool::new(false));
        let target_hit_task = target_hit.clone();
        let target_probe = tokio::spawn(async move {
            let (mut socket, _) = target_listener.accept().await.unwrap();
            target_hit_task.store(true, Ordering::SeqCst);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });

        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (source_origin, source_server) = one_request_server(redirect).await;
        let tool = Arc::new(Fetch::with_policy(
            AllowedOrigins::new([source_origin.as_str()]).unwrap(),
        ));
        let output = tool
            .run(
                FetchInput {
                    url: format!("{source_origin}/start"),
                },
                ctx(),
            )
            .await;

        assert!(output.is_error);
        assert!(
            output.content.contains("host fetch policy"),
            "{}",
            output.content
        );
        assert!(output.content.contains("not allowed"), "{}", output.content);
        source_server.await.unwrap();
        assert!(
            !target_hit.load(Ordering::SeqCst),
            "redirect target was dialed"
        );
        target_probe.abort();
    }

    async fn hanging_body_server() -> (String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (headers_sent, headers_seen) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let _ = headers_sent.send(());
            let _socket = socket;
            std::future::pending::<()>().await;
        });
        (format!("http://{address}"), headers_seen, task)
    }

    #[tokio::test]
    async fn cancellation_covers_response_body_streaming() {
        let (origin, headers_seen, server) = hanging_body_server().await;
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        let run = tokio::spawn(Arc::new(Fetch::default()).run(
            FetchInput {
                url: format!("{origin}/slow"),
            },
            ctx,
        ));

        tokio::time::timeout(Duration::from_secs(1), headers_seen)
            .await
            .expect("server did not send response headers")
            .expect("server dropped response-header signal");
        cancel.cancel();

        let output = tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("fetch did not honor cancellation")
            .expect("fetch task panicked");
        assert!(output.is_error);
        assert_eq!(output.content, "fetch cancelled");
        server.abort();
    }

    #[tokio::test]
    async fn configurable_timeout_covers_response_body_streaming() {
        let (origin, _headers_seen, server) = hanging_body_server().await;
        let tool = Fetch::default().with_timeout(Duration::from_millis(20));

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            Arc::new(tool).run(
                FetchInput {
                    url: format!("{origin}/slow"),
                },
                ctx(),
            ),
        )
        .await
        .expect("fetch exceeded its configured timeout");

        assert!(output.is_error);
        assert_eq!(output.content, "fetch timed out after 20 ms");
        server.abort();
    }
}

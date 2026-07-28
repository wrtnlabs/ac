//! Application-agnostic loopback HTTP server that receives an OAuth
//! `?code=&state=` redirect.
//!
//! Each [`OAuthCallbackServer`] owns one listener and callback registry on a
//! host-configured `127.0.0.1:<port>/<path>`. It is started on demand and
//! stopped once no auth is pending. Pending auths are keyed by the CSRF
//! `state`: presence AND a registered match are both enforced before a code
//! is handed to the waiter, and each pending auth times out after 5 minutes.
//!
//! EVERY query value reflected into a response page is HTML-escaped. The
//! `error` / `error_description` parameters are attacker-controllable for a
//! malicious or compromised authorization server, and this page renders in
//! the user's browser on a loopback origin — the escape is the reflected-XSS
//! guard, not cosmetics.
//!
//! Hand-rolled HTTP: the request is a single GET whose response is a
//! self-contained HTML page, so a full server framework (axum/hyper — neither
//! is a direct dependency of this crate) would be all cost and no benefit.
//! The parser reads the request line, resolves it against a dummy origin with
//! `reqwest::Url` (which owns the percent-decoding), and answers with one
//! `Connection: close` response.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

#[cfg(test)]
use std::sync::LazyLock;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::oauth::is_loopback_redirect;

const HOST: &str = "127.0.0.1";
/// Maximum time an interactive authorization may wait for its redirect.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Cap on the request head we buffer — a callback request line is a few
/// hundred bytes; anything larger is not the browser we are waiting for.
const MAX_REQUEST_HEAD: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

// Stable model-facing rejection messages.
pub const ERR_TIMEOUT: &str = "OAuth callback timeout — authorization took too long";
pub const ERR_CANCELLED: &str = "Authorization cancelled";
pub const ERR_STOPPED: &str = "OAuth callback server stopped";

// Stable rejection copies.
pub const PAGE_MISSING_STATE: &str = "Missing required state parameter — potential CSRF attack.";
pub const PAGE_UNKNOWN_STATE: &str = "Invalid or expired state parameter — potential CSRF attack.";
pub const PAGE_NO_CODE: &str = "No authorization code provided.";

/// Host-owned copy rendered after a successful authorization redirect.
///
/// The callback state machine belongs to AC, but the browser page may name
/// the application that embedded it. Passing the copy at bind time keeps that
/// product wording out of the generic core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCopy {
    pub success_title: String,
    pub success_body: String,
    pub error_title: String,
}

impl Default for PageCopy {
    fn default() -> Self {
        Self {
            success_title: "Authentication complete".to_string(),
            success_body: "You can close this tab and return to the application.".to_string(),
            error_title: "Authentication failed".to_string(),
        }
    }
}

/// Escape the five HTML-significant characters. Applied to EVERY value that
/// reaches a page, whoever supplied it.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

pub fn page(title: &str, body: &str) -> String {
    let t = escape_html(title);
    let b = escape_html(body);
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{t}</title>\
         <h1>{t}</h1><p>{b}</p>"
    )
}

fn success_page(copy: &PageCopy) -> String {
    page(&copy.success_title, &copy.success_body)
}

pub fn error_page(copy: &PageCopy, message: &str) -> String {
    page(&copy.error_title, message)
}

// --- instance-owned registry ----------------------------------------------

struct Binding {
    port: u16,
    path: String,
    page_copy: PageCopy,
    shutdown: CancellationToken,
}

impl Drop for Binding {
    fn drop(&mut self) {
        // The accept loop holds only a weak reference to the server. If the
        // embedding drops its final server/lease/pending handle without an
        // explicit stop, dropping the registry therefore still closes the
        // listener instead of leaving a self-retaining background task.
        self.shutdown.cancel();
    }
}

#[derive(Default)]
struct Registry {
    binding: Option<Binding>,
    /// A coordinator has bound this endpoint and has not completed its flow.
    /// Pending callbacks are not registered until after discovery/DCR, so the
    /// lease itself must keep the listener alive during that gap.
    binding_leases: usize,
    /// CSRF state → the waiter's one-shot.
    pending: HashMap<String, oneshot::Sender<Result<String, String>>>,
    /// Host-visible server name → CSRF state, so `cancel_pending(name)` finds
    /// the entry.
    name_to_state: HashMap<String, String>,
}

struct ServerInner {
    registry: Mutex<Registry>,
    /// Every start is serialized so two near-simultaneous flows on this
    /// server cannot both reach `bind()` (the EADDRINUSE race).
    start: tokio::sync::Mutex<()>,
    /// One interactive flow owns this callback endpoint at a time.
    ///
    /// Serializing the *flow lease* (not merely `bind()`) prevents another
    /// flow on the same embedding from rebinding or stopping the listener
    /// between the first flow's bind and callback.
    flow_lease: Arc<tokio::sync::Mutex<()>>,
}

impl Default for ServerInner {
    fn default() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            start: tokio::sync::Mutex::new(()),
            flow_lease: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// An embedding-owned OAuth loopback callback server.
///
/// Clones refer to the same callback registry and listener. Construct a
/// separate instance for each independent embedding so server names,
/// cancellation, and callback state cannot collide across hosts.
#[derive(Clone, Default)]
pub struct OAuthCallbackServer {
    inner: Arc<ServerInner>,
}

impl OAuthCallbackServer {
    pub fn new() -> Self {
        Self::default()
    }

    fn reg(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// The server exists only while an authorization is pending.
fn stop_if_idle(registry: &mut Registry) {
    if registry.binding_leases > 0 || !registry.pending.is_empty() {
        return;
    }
    if let Some(binding) = registry.binding.take() {
        binding.shutdown.cancel();
    }
}

fn cleanup_state_index(registry: &mut Registry, oauth_state: &str) {
    let stale: Option<String> = registry
        .name_to_state
        .iter()
        .find(|(_, state)| state.as_str() == oauth_state)
        .map(|(name, _)| name.clone());
    if let Some(name) = stale {
        registry.name_to_state.remove(&name);
    }
}

// --- request handling ------------------------------------------------------

enum Reply {
    /// `(status, reason, content-type, body)`
    Http(u16, &'static str, &'static str, String),
}

/// Route one callback request without a socket. Resolves the pending waiter as
/// a side effect.
fn route(server: &OAuthCallbackServer, request_target: &str) -> Reply {
    const HTML: &str = "text/html; charset=utf-8";
    let Some(url) = reqwest::Url::parse("http://127.0.0.1/")
        .expect("static base parses")
        .join(request_target)
        .ok()
    else {
        return Reply::Http(
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "Not found".into(),
        );
    };

    let Some((expected_path, page_copy)) = server
        .reg()
        .binding
        .as_ref()
        .map(|b| (b.path.clone(), b.page_copy.clone()))
    else {
        return Reply::Http(
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "Not found".into(),
        );
    };
    if url.path() != expected_path {
        return Reply::Http(
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            "Not found".into(),
        );
    }

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut error: Option<String> = None;
    let mut error_description: Option<String> = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }

    // No state at all: refuse before looking at anything else.
    let Some(state) = state else {
        return Reply::Http(
            400,
            "Bad Request",
            HTML,
            error_page(&page_copy, PAGE_MISSING_STATE),
        );
    };

    if let Some(error) = error {
        let message = error_description.unwrap_or(error);
        let mut registry = server.reg();
        if let Some(tx) = registry.pending.remove(&state) {
            cleanup_state_index(&mut registry, &state);
            let _ = tx.send(Err(message.clone()));
        }
        stop_if_idle(&mut registry);
        drop(registry);
        return Reply::Http(200, "OK", HTML, error_page(&page_copy, &message));
    }

    let Some(code) = code else {
        return Reply::Http(
            400,
            "Bad Request",
            HTML,
            error_page(&page_copy, PAGE_NO_CODE),
        );
    };

    let mut registry = server.reg();
    let Some(tx) = registry.pending.remove(&state) else {
        // A state we never minted (or one already consumed / expired).
        return Reply::Http(
            400,
            "Bad Request",
            HTML,
            error_page(&page_copy, PAGE_UNKNOWN_STATE),
        );
    };
    cleanup_state_index(&mut registry, &state);
    let _ = tx.send(Ok(code));
    stop_if_idle(&mut registry);
    drop(registry);
    Reply::Http(200, "OK", HTML, success_page(&page_copy))
}

async fn serve_connection(server: OAuthCallbackServer, mut stream: TcpStream) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() >= MAX_REQUEST_HEAD {
                break;
            }
        }
        Some(())
    };
    if tokio::time::timeout(READ_TIMEOUT, read).await.is_err() {
        return;
    }

    let line = head
        .split(|b| *b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .unwrap_or_default();
    // `GET <target> HTTP/1.1` — the method is irrelevant; a browser redirect
    // is always a GET and anything else gets the same 404/handled answer.
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let Reply::Http(status, reason, content_type, body) = route(&server, &target);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

// --- lifecycle -------------------------------------------------------------

/// What one callback-server start actually did — reported rather than left
/// for the caller to infer from separate state.
///
/// The caller must know "did *I* get this port", and the only code that can
/// answer that is the code holding the instance's start lock while it observes
/// the port. A separate [`OAuthCallbackServer::is_running`] read afterwards
/// answers the different question "does this instance have some binding?"
/// Returning the outcome makes the decision atomic with the observation by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// This port/path is ours (freshly bound, or already bound by us).
    Bound { port: u16 },
    /// A foreign process holds the port. Nothing was bound and nothing is
    /// serving for this flow — a code redirected here would go to THEM.
    PortHeldByForeignProcess { port: u16 },
}

/// Exclusive ownership of one server instance's callback endpoint for a flow.
///
/// Dropping the lease releases an idle listener. A registered
/// [`PendingCallback`] keeps the listener alive until its own drop/response,
/// so cleanup is correct regardless of which value is dropped first.
pub struct BindingLease {
    _exclusive: tokio::sync::OwnedMutexGuard<()>,
    server: OAuthCallbackServer,
    protects_binding: bool,
}

struct BindingReservation {
    server: OAuthCallbackServer,
    active: bool,
}

impl BindingReservation {
    fn acquire(server: OAuthCallbackServer) -> Self {
        server.reg().binding_leases += 1;
        Self {
            server,
            active: true,
        }
    }

    fn transfer(mut self) {
        self.active = false;
    }
}

impl Drop for BindingReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut registry = self.server.reg();
        registry.binding_leases = registry
            .binding_leases
            .checked_sub(1)
            .expect("binding reservation accounting underflow");
        stop_if_idle(&mut registry);
    }
}

impl Drop for BindingLease {
    fn drop(&mut self) {
        let mut registry = self.server.reg();
        if self.protects_binding {
            registry.binding_leases = registry
                .binding_leases
                .checked_sub(1)
                .expect("binding lease accounting underflow");
        }
        stop_if_idle(&mut registry);
    }
}

fn callback_endpoint(redirect_uri: &str) -> io::Result<(u16, String)> {
    if !is_loopback_redirect(redirect_uri) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OAuth callback redirect URI must use http://127.0.0.1",
        ));
    }
    let url = reqwest::Url::parse(redirect_uri).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid OAuth callback redirect URI: {error}"),
        )
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "OAuth callback redirect URI has no usable port",
        )
    })?;
    Ok((port, url.path().to_string()))
}

/// Acquire the callback endpoint for one complete interactive flow.
///
/// The returned lease MUST be held until the flow no longer needs the
/// callback endpoint. Calls using a different endpoint wait instead of
/// rebinding underneath the current owner.
impl OAuthCallbackServer {
    pub async fn acquire_binding(
        &self,
        redirect_uri: &str,
        page_copy: PageCopy,
    ) -> io::Result<(BindOutcome, BindingLease)> {
        let exclusive = self.inner.flow_lease.clone().lock_owned().await;
        // Reserve listener liveness before the bind/reuse decision. Without
        // this, an idle-release racing between the start and lease
        // construction could stop the listener in that one-instruction gap.
        // The reservation is RAII because cancellation can drop this future
        // while it waits for the bind serializer or an old socket to close.
        let reservation = BindingReservation::acquire(self.clone());
        let outcome = self
            .ensure_running_with_page_copy_inner(redirect_uri, page_copy, true)
            .await?;
        let protects_binding = matches!(outcome, BindOutcome::Bound { .. });
        if protects_binding {
            reservation.transfer();
        } else {
            drop(reservation);
        }
        Ok((
            outcome,
            BindingLease {
                _exclusive: exclusive,
                server: self.clone(),
                protects_binding,
            },
        ))
    }

    /// Start this callback server (idempotent). A `redirect_uri` that changes
    /// the port/path rebinds when idle and returns `WouldBlock` while another
    /// flow holds a [`BindingLease`]. Every call on this instance is
    /// serialized; a failed start does not poison the chain.
    pub async fn ensure_running(&self, redirect_uri: &str) -> io::Result<BindOutcome> {
        self.ensure_running_with_page_copy(redirect_uri, PageCopy::default())
            .await
    }

    /// [`Self::ensure_running`] with host-supplied browser-page copy.
    pub async fn ensure_running_with_page_copy(
        &self,
        redirect_uri: &str,
        page_copy: PageCopy,
    ) -> io::Result<BindOutcome> {
        self.ensure_running_with_page_copy_inner(redirect_uri, page_copy, false)
            .await
    }

    async fn ensure_running_with_page_copy_inner(
        &self,
        redirect_uri: &str,
        page_copy: PageCopy,
        owns_lease_reservation: bool,
    ) -> io::Result<BindOutcome> {
        let _serialized = self.inner.start.lock().await;
        let (port, path) = callback_endpoint(redirect_uri)?;

        let (current, active_leases) = {
            let registry = self.reg();
            (
                registry
                    .binding
                    .as_ref()
                    .map(|b| (b.port, b.path.clone(), b.page_copy.clone())),
                registry.binding_leases,
            )
        };
        match current {
            Some((p, ref existing, ref existing_copy))
                if p == port && *existing == path && *existing_copy == page_copy =>
            {
                return Ok(BindOutcome::Bound { port });
            }
            Some(_) if active_leases > usize::from(owns_lease_reservation) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the OAuth callback endpoint is leased by another flow",
                ));
            }
            Some(_) => self.stop().await,
            None if active_leases > usize::from(owns_lease_reservation) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the OAuth callback endpoint is leased by another flow",
                ));
            }
            None => {}
        }

        // A predecessor owned by this instance closes asynchronously after
        // its cancellation token fires, so retry `AddrInUse` briefly before
        // classifying the holder as foreign. This also makes a dropped
        // [`BindingLease`] deterministic for the next endpoint owner.
        let mut listener = None;
        for attempt in 0..10 {
            match TcpListener::bind((HOST, port)).await {
                Ok(l) => {
                    listener = Some(l);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse && attempt < 9 => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    return Ok(BindOutcome::PortHeldByForeignProcess { port });
                }
                Err(error) => return Err(error),
            }
        }
        let listener = listener.expect("loop returns on the last failure");

        let shutdown = CancellationToken::new();
        let accept_shutdown = shutdown.clone();
        let weak_server: Weak<ServerInner> = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_shutdown.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let Some(inner) = weak_server.upgrade() else {
                                return;
                            };
                            tokio::spawn(serve_connection(OAuthCallbackServer { inner }, stream));
                        }
                        // A transient accept error must not kill the listener.
                        Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                    },
                }
            }
        });
        self.reg().binding = Some(Binding {
            port,
            path,
            page_copy,
            shutdown,
        });
        Ok(BindOutcome::Bound { port })
    }

    /// Release this instance's listener if no lease or callback still needs
    /// it. This is a no-op while either remains active.
    ///
    /// Coordinators should call this when a flow fails between acquiring its
    /// binding and registering a pending callback (for example during
    /// discovery, dynamic client registration, or persistence). That gap has
    /// no pending entry whose cleanup would otherwise release the listener.
    pub fn release_if_idle(&self) {
        let mut registry = self.reg();
        stop_if_idle(&mut registry);
    }
}

/// A registered pending auth. Split from [`PendingCallback::wait`] so the
/// caller can register BEFORE the authorization URL is handed to the browser
/// — a code that arrives instantly still finds its waiter.
pub struct PendingCallback {
    server: OAuthCallbackServer,
    /// `None` once [`wait`](Self::wait) has taken it; the registry cleanup in
    /// [`Drop`] does not need it.
    rx: Option<oneshot::Receiver<Result<String, String>>>,
    state: String,
    name: String,
    deadline: tokio::time::Instant,
}

impl OAuthCallbackServer {
    /// Register a pending auth for `oauth_state` (indexed by `server_name` so
    /// [`Self::cancel_pending`] can find it).
    pub fn begin(&self, oauth_state: &str, server_name: &str) -> PendingCallback {
        self.begin_with_timeout(oauth_state, server_name, CALLBACK_TIMEOUT)
    }

    /// [`Self::begin`] with an explicit deadline. Only the default is used in
    /// production; tests take the short one instead of a paused clock (the
    /// `tokio/test-util` feature is not enabled in this workspace).
    pub fn begin_with_timeout(
        &self,
        oauth_state: &str,
        server_name: &str,
        timeout: Duration,
    ) -> PendingCallback {
        let (tx, rx) = oneshot::channel();
        {
            let mut registry = self.reg();
            registry.pending.insert(oauth_state.to_string(), tx);
            registry
                .name_to_state
                .insert(server_name.to_string(), oauth_state.to_string());
        }
        PendingCallback {
            server: self.clone(),
            rx: Some(rx),
            state: oauth_state.to_string(),
            name: server_name.to_string(),
            deadline: tokio::time::Instant::now() + timeout,
        }
    }
}

impl PendingCallback {
    /// Await the `?code`. Rejects on timeout, cancel, or server stop; the
    /// registry entry is cleaned up by [`Drop`] on the way out, whichever way
    /// that is.
    pub async fn wait(mut self) -> Result<String, String> {
        let rx = self
            .rx
            .take()
            .expect("wait consumes the receiver exactly once");
        match tokio::time::timeout_at(self.deadline, rx).await {
            Ok(Ok(result)) => result,
            // The sender was dropped without a value — only reachable if the
            // registry entry was discarded, which is the stop path.
            Ok(Err(_)) => Err(ERR_STOPPED.to_string()),
            Err(_) => Err(ERR_TIMEOUT.to_string()),
        }
        // `self` drops here → unregister → stop_if_idle.
    }

    /// Remove this flow's registry footprint and release the listener if it
    /// was the last one. Idempotent.
    fn unregister(&self) {
        let mut registry = self.server.reg();
        registry.pending.remove(&self.state);
        if registry
            .name_to_state
            .get(&self.name)
            .is_some_and(|s| *s == self.state)
        {
            registry.name_to_state.remove(&self.name);
        }
        stop_if_idle(&mut registry);
    }
}

/// Cleanup on EVERY exit, not just a completed [`wait`](PendingCallback::wait).
///
/// A `wait` future can be dropped mid-flight — the client closed the stream,
/// the enclosing flow future was cancelled — and without this the `pending`
/// entry stays in the registry forever, which in turn pins the loopback
/// binding forever (`stop_if_idle` refuses to release while anything is
/// pending). The same hole let a second concurrent authorization for one
/// server overwrite `name_to_state` and orphan the first flow's entry.
impl Drop for PendingCallback {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl OAuthCallbackServer {
    /// Cancel an in-flight auth for `server_name`.
    pub fn cancel_pending(&self, server_name: &str) {
        let mut registry = self.reg();
        let key = registry
            .name_to_state
            .get(server_name)
            .cloned()
            .unwrap_or_else(|| server_name.to_string());
        if let Some(tx) = registry.pending.remove(&key) {
            registry.name_to_state.remove(server_name);
            let _ = tx.send(Err(ERR_CANCELLED.to_string()));
        }
        // Unconditional, not inside the `if let`: a cancel that arrives during
        // the discovery / registration window has nothing pending to remove.
        // A no-op while anything is pending, so another flow already
        // registered on this instance is unaffected.
        stop_if_idle(&mut registry);
    }

    /// Stop this instance and reject every pending auth.
    pub async fn stop(&self) {
        let cancelled = {
            let mut registry = self.reg();
            let binding = registry.binding.take();
            let pending: Vec<_> = registry.pending.drain().collect();
            registry.name_to_state.clear();
            for (_, tx) in pending {
                let _ = tx.send(Err(ERR_STOPPED.to_string()));
            }
            binding
        };
        if let Some(binding) = cancelled {
            binding.shutdown.cancel();
            // Let the accept task observe the cancel and drop the listener
            // before a caller rebinds the same port.
            tokio::task::yield_now().await;
        }
    }

    pub fn is_running(&self) -> bool {
        self.reg().binding.is_some()
    }
}

/// True if `port` already has a listener on 127.0.0.1.
pub async fn is_port_in_use(port: u16) -> bool {
    tokio::time::timeout(Duration::from_millis(250), TcpStream::connect((HOST, port)))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Loopback tests reserve an ephemeral port and then bind it in a second
/// operation. Serialize those physical-port tests across this crate so one
/// test cannot be assigned another test's just-released port in that gap.
#[cfg(test)]
pub(crate) static TEST_SERIAL: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CALLBACK_PATH: &str = "/oauth/callback";

    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().await
    }

    async fn free_port() -> u16 {
        let l = TcpListener::bind((HOST, 0)).await.unwrap();
        l.local_addr().unwrap().port()
    }

    fn redirect(port: u16) -> String {
        format!("http://127.0.0.1:{port}{TEST_CALLBACK_PATH}")
    }

    async fn get(port: u16, target: &str) -> (String, String) {
        let mut stream = TcpStream::connect((HOST, port)).await.unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        (head.to_string(), body.to_string())
    }

    #[test]
    fn escaping_covers_every_html_significant_char() {
        assert_eq!(
            escape_html(r#"<script>alert("x&y")</script>'"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;&#39;"
        );
        // The page interpolates the title too, and escapes it.
        let html = page("<b>t</b>", "<i>b</i>");
        assert!(!html.contains("<b>"), "{html}");
        assert!(!html.contains("<i>"), "{html}");
        assert!(html.contains("&lt;b&gt;t&lt;/b&gt;"), "{html}");
        let copy = PageCopy {
            success_title: "Done <now>".to_string(),
            success_body: "Return to <host>.".to_string(),
            error_title: "Failed <now>".to_string(),
        };
        let html = success_page(&copy);
        assert!(html.contains("Done &lt;now&gt;"), "{html}");
        assert!(html.contains("Return to &lt;host&gt;."), "{html}");
    }

    #[tokio::test]
    async fn happy_path_resolves_the_waiter_and_stops_when_idle() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        assert!(server.is_running());

        let pending = server.begin("st4te", "tracker");
        let waiter = tokio::spawn(pending.wait());
        let (head, body) = get(port, &format!("{TEST_CALLBACK_PATH}?code=abc&state=st4te")).await;
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(body.contains("Authentication complete"), "{body}");
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
        // Nothing pending means the server shuts itself down.
        assert!(!server.is_running());
        server.stop().await;
    }

    #[tokio::test]
    async fn a_binding_lease_prevents_cross_flow_rebinds() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let first_port = free_port().await;
        let mut second_port = free_port().await;
        while second_port == first_port {
            second_port = free_port().await;
        }
        let (_, first_lease) = server
            .acquire_binding(
                &redirect(first_port),
                PageCopy {
                    success_title: "First".into(),
                    ..PageCopy::default()
                },
            )
            .await
            .unwrap();
        server.cancel_pending("unrelated");
        server.release_if_idle();
        assert!(
            is_port_in_use(first_port).await,
            "idle release must not stop a listener protected by a flow lease"
        );

        let second_server = server.clone();
        let second = tokio::spawn(async move {
            second_server
                .acquire_binding(
                    &redirect(second_port),
                    PageCopy {
                        success_title: "Second".into(),
                        ..PageCopy::default()
                    },
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !second.is_finished(),
            "a second endpoint must wait for the current flow lease"
        );
        assert!(is_port_in_use(first_port).await);
        assert!(!is_port_in_use(second_port).await);

        drop(first_lease);
        let (outcome, second_lease) = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second endpoint should acquire after release")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, BindOutcome::Bound { port: second_port });
        assert!(is_port_in_use(second_port).await);
        drop(second_lease);
        server.stop().await;
    }

    #[tokio::test]
    async fn cancelling_a_waiting_binding_acquire_does_not_leak_liveness() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let first_port = free_port().await;
        let mut second_port = free_port().await;
        while second_port == first_port {
            second_port = free_port().await;
        }
        let (_, first_lease) = server
            .acquire_binding(&redirect(first_port), PageCopy::default())
            .await
            .unwrap();

        let second_redirect = redirect(second_port);
        let waiting = server.acquire_binding(&second_redirect, PageCopy::default());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), waiting)
                .await
                .is_err()
        );
        drop(first_lease);

        let (outcome, second_lease) = server
            .acquire_binding(&second_redirect, PageCopy::default())
            .await
            .unwrap();
        assert_eq!(outcome, BindOutcome::Bound { port: second_port });
        drop(second_lease);
        assert!(!server.is_running());
        server.stop().await;
    }

    #[tokio::test]
    async fn cancelling_a_binding_retry_releases_its_registry_reservation() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let foreign = TcpListener::bind((HOST, 0)).await.unwrap();
        let port = foreign.local_addr().unwrap().port();
        let redirect = redirect(port);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                server.acquire_binding(&redirect, PageCopy::default()),
            )
            .await
            .is_err(),
            "the acquire should still be retrying the occupied port"
        );
        drop(foreign);

        let (outcome, lease) = server
            .acquire_binding(&redirect, PageCopy::default())
            .await
            .unwrap();
        assert_eq!(outcome, BindOutcome::Bound { port });
        drop(lease);
        assert!(!server.is_running());
        server.stop().await;
    }

    #[tokio::test]
    async fn a_state_we_never_minted_is_refused_as_csrf_and_never_resolves() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        let pending = server.begin("mine", "tracker");
        let waiter = tokio::spawn(pending.wait());

        // Missing state entirely.
        let (head, body) = get(port, &format!("{TEST_CALLBACK_PATH}?code=abc")).await;
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(body.contains(PAGE_MISSING_STATE), "{body}");
        // A state that is not ours.
        let (head, body) = get(
            port,
            &format!("{TEST_CALLBACK_PATH}?code=abc&state=attacker"),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(body.contains(PAGE_UNKNOWN_STATE), "{body}");
        // State but no code.
        let (head, body) = get(port, &format!("{TEST_CALLBACK_PATH}?state=mine")).await;
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(body.contains(PAGE_NO_CODE), "{body}");
        // Wrong path.
        let (head, _) = get(port, "/nope?code=abc&state=mine").await;
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");

        // Through all of that our waiter is still pending — no code leaked.
        assert!(!waiter.is_finished());
        server.cancel_pending("tracker");
        assert_eq!(waiter.await.unwrap().unwrap_err(), ERR_CANCELLED);
        server.stop().await;
    }

    #[tokio::test]
    async fn reflected_query_values_are_escaped_on_the_error_page() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        let pending = server.begin("st", "srv");
        let waiter = tokio::spawn(pending.wait());

        // A hostile authorization server reflecting a script tag back at the
        // user's browser on our loopback origin.
        let (head, body) = get(
            port,
            &format!(
                "{TEST_CALLBACK_PATH}?state=st&error=denied&error_description=\
                 %3Cscript%3Ealert(1)%3C%2Fscript%3E"
            ),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(!body.contains("<script>"), "unescaped payload: {body}");
        assert!(
            body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{body}"
        );
        // The waiter is rejected with the server's message, undecorated.
        assert_eq!(
            waiter.await.unwrap().unwrap_err(),
            "<script>alert(1)</script>"
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn timeout_reports_the_public_error_and_releases_the_entry() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        // A second flow keeps the server up past the timeout so the late-code
        // assertion below still has a socket to talk to.
        let keeper = tokio::spawn(server.begin("keep", "other").wait());
        // Same code path as the 5-minute production deadline, 30ms of it.
        let pending = server.begin_with_timeout("slow", "srv", Duration::from_millis(30));
        assert_eq!(pending.wait().await.unwrap_err(), ERR_TIMEOUT);
        // The expired entry is gone: a late code finds no waiter.
        let (head, body) = get(port, &format!("{TEST_CALLBACK_PATH}?code=late&state=slow")).await;
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(body.contains(PAGE_UNKNOWN_STATE), "{body}");
        server.cancel_pending("other");
        assert_eq!(keeper.await.unwrap().unwrap_err(), ERR_CANCELLED);
        server.stop().await;
    }

    #[tokio::test]
    async fn stop_rejects_pending_and_rebinding_a_new_endpoint_works() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let first = free_port().await;
        server.ensure_running(&redirect(first)).await.unwrap();
        let waiter = tokio::spawn(server.begin("s1", "srv").wait());
        server.stop().await;
        assert_eq!(waiter.await.unwrap().unwrap_err(), ERR_STOPPED);
        assert!(!server.is_running());

        let second = free_port().await;
        server.ensure_running(&redirect(second)).await.unwrap();
        // Idempotent for the same endpoint.
        server.ensure_running(&redirect(second)).await.unwrap();
        assert!(server.is_running());
        let waiter = tokio::spawn(server.begin("s2", "srv").wait());
        let (head, _) = get(second, &format!("{TEST_CALLBACK_PATH}?code=ok&state=s2")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(waiter.await.unwrap().unwrap(), "ok");
        server.stop().await;
    }

    #[tokio::test]
    async fn cancel_is_a_noop_without_a_pending_entry() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        server.cancel_pending("nothing-here");
        assert!(!server.is_running());
    }

    /// A cancel that lands in the discovery / registration window finds
    /// nothing pending. `stop_if_idle` used to sit INSIDE the `if let
    /// Some(tx)` arm, so it never ran and the listener stayed bound for the
    /// embedding's lifetime.
    #[tokio::test]
    async fn cancel_before_begin_still_releases_the_binding() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        assert!(server.is_running());
        server.cancel_pending("srv"); // nothing registered yet
        assert!(!server.is_running());
        assert!(!is_port_in_use(port).await);
    }

    /// A `wait` future can be dropped mid-flight (the client closed the
    /// stream, the enclosing flow was cancelled). Without `Drop` the
    /// `pending` entry survived forever, and `stop_if_idle` refuses to
    /// release while anything is pending — so one dropped wait pinned the
    /// loopback binding for the embedding's lifetime.
    #[tokio::test]
    async fn dropping_a_pending_wait_unregisters_and_releases_the_port() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        let port = free_port().await;
        server.ensure_running(&redirect(port)).await.unwrap();
        let waiter = tokio::spawn(server.begin("dropped", "srv").wait());
        // Let it actually park on the receiver before killing it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(server.is_running());
        waiter.abort();
        let _ = waiter.await;

        assert!(!server.is_running(), "the binding survived a dropped wait");
        assert!(!is_port_in_use(port).await);
        // And the state index is clean: a later flow reusing the name is not
        // pointed at the dead entry.
        assert!(server.reg().pending.is_empty());
        assert!(server.reg().name_to_state.is_empty());
    }

    #[tokio::test]
    async fn separate_servers_do_not_share_callback_state_or_cancellation() {
        let _g = serial().await;
        let first = OAuthCallbackServer::new();
        let second = OAuthCallbackServer::new();
        let first_port = free_port().await;
        let mut second_port = free_port().await;
        while second_port == first_port {
            second_port = free_port().await;
        }
        first.ensure_running(&redirect(first_port)).await.unwrap();
        second.ensure_running(&redirect(second_port)).await.unwrap();

        // Reusing both indexes is safe because each embedding owns its
        // registry. Cancelling one must neither resolve nor unregister the
        // other.
        let first_waiter = tokio::spawn(first.begin("same-state", "same-name").wait());
        let second_waiter = tokio::spawn(second.begin("same-state", "same-name").wait());
        first.cancel_pending("same-name");
        assert_eq!(first_waiter.await.unwrap().unwrap_err(), ERR_CANCELLED);
        assert!(!first.is_running());
        assert!(second.is_running());
        assert!(!second_waiter.is_finished());

        let (head, _) = get(
            second_port,
            &format!("{TEST_CALLBACK_PATH}?code=second&state=same-state"),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(second_waiter.await.unwrap().unwrap(), "second");
        assert!(!second.is_running());
    }

    /// `is_running()` only answers whether this instance has a binding; it
    /// cannot prove that a requested endpoint was bound. `ensure_running`
    /// reports the bind outcome under the same lock that observed the port.
    #[tokio::test]
    async fn a_squatted_port_is_reported_even_while_another_endpoint_is_bound() {
        let _g = serial().await;
        let server = OAuthCallbackServer::new();
        // Another flow's legitimate binding.
        let other = free_port().await;
        assert_eq!(
            server.ensure_running(&redirect(other)).await.unwrap(),
            BindOutcome::Bound { port: other }
        );
        // Ours is held by a foreign process.
        let squatter = TcpListener::bind((HOST, 0)).await.unwrap();
        let mine = squatter.local_addr().unwrap().port();
        // The old predicate: a binding exists, so the guard passed.
        assert!(server.is_running());
        assert_eq!(
            server.ensure_running(&redirect(mine)).await.unwrap(),
            BindOutcome::PortHeldByForeignProcess { port: mine }
        );
        drop(squatter);
        server.stop().await;
    }
}

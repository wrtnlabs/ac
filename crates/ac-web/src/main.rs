//! The web harness: AC's third host (after the CLI and the test suites) and
//! the standing proof that the boundary is the ACP protocol, not an API.
//!
//! The browser is a real ACP client. Each WebSocket connection carries plain
//! newline-free JSON-RPC frames (one message per text frame) into the same
//! `ac_acp::agent(...)` that also serves stdio — this binary contains zero
//! agent logic. The only non-ACP surface is `/api/config`: a host-side
//! convenience for the session picker (listing sessions is a host UI concern;
//! the conversation itself is all protocol).
//!
//!   OPENROUTER_API_KEY=… cargo run -p ac-web -- --dir /path/to/workspace
//!
//! Flags: --dir <sandbox root> (default: cwd), --model <id>, --port <u16>,
//! --db <path> (default: ~/.ac/ac-web/<workspace-hash>.db — never inside
//! --dir), --web-search.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ac_acp::{AcpOptions, SessionFactory, SessionParts};
use ac_provider::{Provider, ServerTool};
use ac_provider_openrouter::OpenRouter;
use ac_runtime::AgentConfig;
use ac_store::SqliteStore;
use ac_tool::{SubtreePolicy, ToolCtx};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use futures::{SinkExt, StreamExt};

struct App {
    factory: SessionFactory,
    store: Arc<SqliteStore>,
    model: String,
    dir: PathBuf,
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("set OPENROUTER_API_KEY (the agent needs a provider)"))?;

    let dir = args.dir.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "--dir {}: {e} (must exist; it is the sandbox root)",
            args.dir.display()
        )
    })?;
    // The DB must live OUTSIDE the sandbox root — inside it, the agent's own
    // file tools (or an injected prompt) could read, corrupt, or delete every
    // session's history.
    let db_path = match args.db {
        Some(db) => db,
        None => default_db_path(&dir)?,
    };
    if let Ok(resolved) = db_path.parent().unwrap_or(Path::new(".")).canonicalize()
        && resolved.starts_with(&dir)
    {
        anyhow::bail!(
            "--db {} is inside the sandbox root {} — the agent could destroy its own session \
             store; pick a path outside it",
            db_path.display(),
            dir.display()
        );
    }
    let store = Arc::new(SqliteStore::open(&db_path)?);

    let provider = Arc::new(OpenRouter::new(api_key));
    let mut server_tools = Vec::new();
    if args.web_search {
        let web_search = ServerTool::WebSearch {
            max_results: Some(5),
        };
        if provider.supports_server_tool(&web_search) {
            server_tools.push(web_search);
        }
    }

    let model = args.model.clone();
    let sandbox_root = dir.clone();
    let factory: SessionFactory = Arc::new(move |cwd: &Path| {
        // Defense in depth behind the WS Origin check: session cwd comes off
        // the wire, and this host only ever serves --dir. The shipped UI only
        // sends config.cwd, so this costs nothing.
        let cwd = cwd
            .canonicalize()
            .map_err(|e| format!("cwd {}: {e}", cwd.display()))?;
        if !cwd.starts_with(&sandbox_root) {
            return Err(format!(
                "cwd {} is outside the sandbox root {}",
                cwd.display(),
                sandbox_root.display()
            ));
        }
        let policy = SubtreePolicy::new(&cwd).map_err(|e| e.to_string())?;
        Ok(SessionParts {
            provider: provider.clone(),
            registry: Arc::new(ac_cli::generic_registry()),
            config: AgentConfig {
                model: model.clone(),
                system: Some(ac_cli::SYSTEM_PROMPT.to_string()),
                max_iterations: ac_cli::MAX_ITERATIONS,
                server_tools: server_tools.clone(),
                ..Default::default()
            },
            ctx: Arc::new(ToolCtx::new(Arc::new(policy))),
        })
    });

    let app = Arc::new(App {
        factory,
        store,
        model: args.model,
        dir,
        port: args.port,
    });

    let router = Router::new()
        .route("/", get(index))
        .route("/api/config", get(config))
        .route("/ws", get(ws_upgrade))
        .with_state(app.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("ac-web: http://{addr}  (sandbox: {})", app.dir.display());
    axum::serve(listener, router).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
}

async fn config(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let sessions = app
        .store
        .list_sessions(50)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "title": s.title,
                "updated_at_ms": s.updated_at_ms,
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "model": app.model,
        "cwd": app.dir,
        "sessions": sessions,
    }))
}

async fn ws_upgrade(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    // WebSockets are exempt from the browser same-origin policy: without this
    // check, ANY web page the user visits could connect to localhost and
    // drive a shell-capable agent. Only our own served page may connect.
    let allowed = [
        format!("http://127.0.0.1:{}", app.port),
        format!("http://localhost:{}", app.port),
    ];
    let origin_ok = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| allowed.iter().any(|a| a == origin));
    if !origin_ok {
        return (
            StatusCode::FORBIDDEN,
            "cross-origin WebSocket connections are not allowed",
        )
            .into_response();
    }
    upgrade
        .on_upgrade(move |socket| ws_acp(app, socket))
        .into_response()
}

/// One WebSocket = one ACP connection. Frames are whole JSON-RPC messages;
/// `Lines` on the agent side treats each as one line.
async fn ws_acp(app: Arc<App>, socket: WebSocket) {
    let (sender, receiver) = socket.split();

    let outgoing = sender
        .sink_map_err(std::io::Error::other)
        .with(|line: String| {
            futures::future::ready(Ok::<_, std::io::Error>(Message::Text(line.into())))
        });
    let incoming = receiver.filter_map(|frame| {
        futures::future::ready(match frame {
            Ok(Message::Text(text)) => Some(Ok(text.to_string())),
            Ok(_) => None,
            Err(e) => Some(Err(std::io::Error::other(e))),
        })
    });

    let mut options = AcpOptions::new(app.factory.clone());
    options.store = Some(app.store.clone());

    use ac_acp::acp::{ConnectTo, Lines};
    if let Err(e) = ac_acp::agent(options)
        .connect_to(Lines::new(outgoing, incoming))
        .await
    {
        eprintln!("ac-web: connection ended with error: {e}");
    }
}

struct Args {
    dir: PathBuf,
    model: String,
    port: u16,
    db: Option<PathBuf>,
    web_search: bool,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut parsed = Self {
            dir: PathBuf::from("."),
            model: "anthropic/claude-haiku-4.5".to_string(),
            port: 8787,
            db: None,
            web_search: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dir" => parsed.dir = required(&arg, args.next())?.into(),
                "--model" => parsed.model = required(&arg, args.next())?,
                "--port" => parsed.port = required(&arg, args.next())?.parse()?,
                "--db" => parsed.db = Some(required(&arg, args.next())?.into()),
                "--web-search" => parsed.web_search = true,
                other => anyhow::bail!("unknown flag: {other}"),
            }
        }
        Ok(parsed)
    }
}

fn required(flag: &str, value: Option<String>) -> anyhow::Result<String> {
    value.ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

/// `~/.ac/ac-web/<hash-of-sandbox-dir>.db` — stable per workspace, never
/// inside it.
fn default_db_path(dir: &Path) -> anyhow::Result<PathBuf> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME not set; pass --db"))?;
    let mut hasher = DefaultHasher::new();
    dir.hash(&mut hasher);
    Ok(PathBuf::from(home)
        .join(".ac")
        .join("ac-web")
        .join(format!("{:016x}.db", hasher.finish())))
}

//! Demo host: serves an AC agent to a stock `useChat` React app over the AI
//! SDK UI Message Stream Protocol. The React app runs on Vite and proxies
//! `/api/*` here.
//!
//! Unlike the ACP host, this needs no session slots or cancel-token juggling:
//! the AI SDK model is server-owns-history-keyed-by-id, so every `/api/chat`
//! is a fresh `Session::resume(load_messages(id)) → run → persist`. The store
//! is the continuity; the request is stateless.
//!
//!   OPENROUTER_API_KEY=… cargo run -p ac-ai-sdk -- --dir /path/to/workspace
//!
//! Flags: --dir <sandbox root>, --model <id>, --port <u16> (default 8790),
//! --db <path> (default ~/.ac/ac-ai-sdk/<hash>.db — never inside --dir),
//! --allow-origin <origin> (repeatable; the Vite dev origin is allowed by
//! default), --web-search, --image-gen (register the image_gen host tool;
//! reuses OPENROUTER_API_KEY), --image-model <id> (default
//! google/gemini-2.5-flash-image).
//!
//! `GET /api/files/{*path}` serves workspace files under the same containment
//! the tools write through, so the UI can display saved artifacts (e.g.
//! generated images) straight from disk.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ac_ai_sdk::{ChunkEncoder, DONE, hydrate_messages, user_text};
use ac_provider::{Provider, ServerTool};
use ac_provider_openrouter::OpenRouter;
use ac_runtime::{AgentConfig, AgentEvent, Session};
use ac_store::SqliteStore;
use ac_tool::{SubtreePolicy, ToolCtx};
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Router, body::Bytes};
use tokio::sync::mpsc;

mod tools;

struct App {
    store: Arc<SqliteStore>,
    provider: Arc<dyn Provider>,
    server_tools: Vec<ServerTool>,
    model: String,
    dir: PathBuf,
    allowed_origins: Vec<String>,
    api_key: String,
    image_gen: bool,
    image_model: String,
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
    // Keep the session DB OUT of the sandbox — the agent's own file tools must
    // not be able to read or wreck every session's history. Fail CLOSED: a
    // relative `--db` or a not-yet-existing parent must still be proven outside
    // the sandbox (an absolute lexical resolve needs no existing dir, unlike
    // canonicalize).
    let db_path = match args.db {
        Some(db) => db,
        None => default_db_path(&dir)?,
    };
    let abs_db = std::path::absolute(&db_path)
        .map_err(|e| anyhow::anyhow!("--db {}: {e}", db_path.display()))?;
    if abs_db.starts_with(&dir) {
        anyhow::bail!(
            "--db {} is inside the sandbox root {} — pick a path outside it",
            abs_db.display(),
            dir.display()
        );
    }
    let store = Arc::new(SqliteStore::open(&abs_db)?);

    let provider: Arc<dyn Provider> = Arc::new(OpenRouter::new(api_key.clone()));
    let mut server_tools = Vec::new();
    if args.web_search {
        let web_search = ServerTool::WebSearch {
            max_results: Some(5),
        };
        if provider.supports_server_tool(&web_search) {
            server_tools.push(web_search);
        }
    }

    let mut allowed_origins = vec![
        format!("http://127.0.0.1:{}", args.port),
        format!("http://localhost:{}", args.port),
        // Vite's default dev origin — the demo's normal setup.
        "http://localhost:5173".to_string(),
        "http://127.0.0.1:5173".to_string(),
    ];
    allowed_origins.extend(args.allow_origin);

    let app = Arc::new(App {
        store,
        provider,
        server_tools,
        model: args.model,
        dir,
        allowed_origins,
        api_key,
        image_gen: args.image_gen,
        image_model: args.image_model,
    });

    let router = Router::new()
        .route("/", get(root))
        .route("/api/config", get(config))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", get(session_messages))
        .route("/api/chat", post(chat))
        .route("/api/files/{*path}", get(file))
        .with_state(app.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "ac-ai-sdk: http://{addr}  (sandbox: {})\n  point a useChat app at /api/chat (Vite proxy)",
        app.dir.display()
    );
    axum::serve(listener, router).await?;
    Ok(())
}

async fn root() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ac-ai-sdk host is up. Run the React demo (examples/web-react) with `pnpm dev`; \
         it proxies /api here.\n",
    )
}

async fn config(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "model": app.model, "cwd": app.dir }))
}

async fn sessions(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let sessions = app
        .store
        .list_sessions(50)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "title": s.title,
                "updatedAtMs": s.updated_at_ms,
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "sessions": sessions }))
}

async fn session_messages(State(app): State<Arc<App>>, AxPath(id): AxPath<String>) -> Response {
    match app.store.load_messages(&id) {
        Ok(history) => {
            Json(serde_json::json!({ "messages": hydrate_messages(&history) })).into_response()
        }
        // Unknown id (never prompted) is an empty transcript, not an error.
        Err(ac_store::StoreError::UnknownSession(_)) => {
            Json(serde_json::json!({ "messages": [] })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn chat(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    // Same-origin discipline: a POST with tools behind it is a side-effecting
    // request, so a cross-origin browser Origin is refused. A missing Origin
    // (curl / server-to-server) is not a drive-by vector, so it's allowed.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !app.allowed_origins.iter().any(|a| a == origin)
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad JSON: {e}")).into_response(),
    };
    let Some(id) = request
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "missing chat id").into_response();
    };
    // The client is configured to send only the last message (+ id).
    let message = request
        .get("message")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let prompt = user_text(&message);
    if prompt.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty prompt").into_response();
    }

    // Adopt the client's chat id as the session id; title new sessions.
    match app.store.create_session_with_id(&id, None) {
        Ok(true) => {
            let title: String = prompt.chars().take(60).collect();
            let _ = app.store.rename_session(&id, &title);
        }
        Ok(false) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }

    let history = match app.store.load_messages(&id) {
        Ok(h) => h,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let history_len = history.len();

    let policy = match SubtreePolicy::new(&app.dir) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));
    let cancel = ctx.cancel.clone();

    let mut registry = ac_cli::generic_registry();
    let mut system = ac_cli::SYSTEM_PROMPT.to_string();
    if app.image_gen {
        registry.register(tools::ImageGen::new(
            app.api_key.clone(),
            app.image_model.clone(),
        ));
        system.push_str(
            " An image_gen tool is available: it generates an image from a text prompt and \
             saves it under generated/ in the workspace; refer to the image by the path it \
             returns.",
        );
    }

    let config = AgentConfig {
        model: app.model.clone(),
        system: Some(system),
        max_iterations: ac_cli::MAX_ITERATIONS,
        server_tools: app.server_tools.clone(),
        ..Default::default()
    };
    let mut session = Session::resume(
        app.provider.clone(),
        Arc::new(registry),
        ctx,
        config,
        history,
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    // A second sender so a runtime failure (provider error, timeout,
    // max-iterations) becomes a visible `error` chunk instead of a clean
    // finish — run_turn signals those via its Err, not on the sink.
    let err_tx = tx.clone();
    let store = app.store.clone();
    let session_id = id.clone();
    // The turn runs independently of the response stream: if the client aborts
    // (stop()), the stream drops, its cancel drop-guard fires, run_turn ends,
    // and this task still persists whatever happened.
    tokio::spawn(async move {
        if let Err(e) = session.run_turn(prompt, tx).await {
            let _ = err_tx.send(AgentEvent::Error(e.to_string()));
        }
        // Drop the last sender so the stream closes and emits [DONE] promptly;
        // persistence below is detached from the response.
        drop(err_tx);
        let messages = session.messages();
        if messages.len() > history_len
            && let Err(e) = store.append_messages(
                &session_id,
                &messages[history_len..],
                Some(history_len as u64),
            )
        {
            // A lost-update race (concurrent turns on one chat id) surfaces
            // here after the stream already closed — visible in logs, not to
            // the client. See CLAUDE.md "detected, not prevented".
            eprintln!("ac-ai-sdk: persist failed for session {session_id}: {e}");
        }
    });

    let message_id = uuid::Uuid::new_v4().simple().to_string();
    // Cancels the turn the moment the client disconnects and this stream drops.
    let guard = cancel.drop_guard();
    let stream = async_stream::stream! {
        let mut encoder = ChunkEncoder::new(message_id);
        let _guard = guard;
        for chunk in encoder.start() {
            yield sse_frame(&chunk);
        }
        while let Some(event) = rx.recv().await {
            for chunk in encoder.encode(event) {
                yield sse_frame(&chunk);
            }
        }
        for chunk in encoder.finish() {
            yield sse_frame(&chunk);
        }
        yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {DONE}\n\n")));
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("x-vercel-ai-ui-message-stream", "v1")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .expect("valid response")
}

fn sse_frame(chunk: &serde_json::Value) -> Result<Bytes, std::io::Error> {
    Ok(Bytes::from(format!("data: {chunk}\n\n")))
}

async fn file(State(app): State<Arc<App>>, AxPath(path): AxPath<String>) -> Response {
    let policy = match SubtreePolicy::new(&app.dir) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let served = match tools::resolve_served_file(&policy, &path) {
        Ok(s) => s,
        Err(tools::ServeError::Forbidden) => {
            return (StatusCode::FORBIDDEN, "path not allowed").into_response();
        }
        Err(tools::ServeError::NotFound) => {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
        Err(tools::ServeError::TooLarge) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "file exceeds the serving cap",
            )
                .into_response();
        }
    };
    let bytes = match tokio::fs::read(&served.path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    // Re-checked after the read: the file may have grown since the stat.
    if bytes.len() as u64 > tools::SERVE_CAP {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "file exceeds the serving cap",
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, served.content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

struct Args {
    dir: PathBuf,
    model: String,
    port: u16,
    db: Option<PathBuf>,
    allow_origin: Vec<String>,
    web_search: bool,
    image_gen: bool,
    image_model: String,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut parsed = Self {
            dir: PathBuf::from("."),
            model: "anthropic/claude-haiku-4.5".to_string(),
            port: 8790,
            db: None,
            allow_origin: Vec::new(),
            web_search: false,
            image_gen: false,
            image_model: "google/gemini-2.5-flash-image".to_string(),
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--dir" => parsed.dir = required(&arg, args.next())?.into(),
                "--model" => parsed.model = required(&arg, args.next())?,
                "--port" => parsed.port = required(&arg, args.next())?.parse()?,
                "--db" => parsed.db = Some(required(&arg, args.next())?.into()),
                "--allow-origin" => parsed.allow_origin.push(required(&arg, args.next())?),
                "--web-search" => parsed.web_search = true,
                "--image-gen" => parsed.image_gen = true,
                "--image-model" => parsed.image_model = required(&arg, args.next())?,
                other => anyhow::bail!("unknown flag: {other}"),
            }
        }
        Ok(parsed)
    }
}

fn required(flag: &str, value: Option<String>) -> anyhow::Result<String> {
    value.ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn default_db_path(dir: &Path) -> anyhow::Result<PathBuf> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME not set; pass --db"))?;
    let mut hasher = DefaultHasher::new();
    dir.hash(&mut hasher);
    Ok(PathBuf::from(home)
        .join(".ac")
        .join("ac-ai-sdk")
        .join(format!("{:016x}.db", hasher.finish())))
}

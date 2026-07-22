//! Agent-side ACP: any ACP client (Zed, a web UI, a test harness) can host an
//! AC agent. This crate is protocol glue only — the ACP wire on one side, the
//! `ac-runtime` loop on the other, and nothing app-shaped in between.
//!
//! **The protocol is the boundary.** Hosts inject what varies through
//! [`AcpOptions`]: a [`SessionFactory`] that builds the provider/registry/
//! policy for a new session's `cwd`, and an optional [`SqliteStore`] that
//! turns on persistence (and with it the `session/load` capability). The kit
//! never learns what the host is.
//!
//! Concurrency shape (dictated by the SDK): request handlers must return
//! immediately or they block the dispatch loop — so `session/prompt` spawns
//! the turn, streams [`AgentEvent`]s back as `session/update` notifications,
//! and responds from the spawned task. `session/cancel` flips the session's
//! cancel token; a cancelled turn responds `StopReason::Cancelled` (a normal
//! response, per spec), and the session is rebuilt from its own history with
//! a fresh token so the next prompt works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ac_provider::Provider;
use ac_runtime::{AgentConfig, AgentEvent, RuntimeError, Session};
use ac_store::SqliteStore;
use ac_tool::{ToolCtx, ToolRegistry};
use ac_types::{ContentPart, Message, Role};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ResourceLink, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Error, Stdio};
use tokio::sync::mpsc;

/// Re-exported so hosts reach the ACP SDK (transports, schema types) without
/// their own dependency.
pub use agent_client_protocol as acp;

/// Everything session-shaped a host must supply for a new session rooted at
/// `cwd`. The factory runs on `session/new` and `session/load` — and again
/// after a cancelled turn, to remint the context (a `CancellationToken`
/// cannot be un-cancelled).
pub struct SessionParts {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub config: AgentConfig,
    pub ctx: Arc<ToolCtx>,
}

pub type SessionFactory =
    Arc<dyn Fn(&Path) -> Result<SessionParts, String> + Send + Sync + 'static>;

pub struct AcpOptions {
    pub factory: SessionFactory,
    /// Persistence. `Some` also advertises the `loadSession` capability.
    pub store: Option<Arc<SqliteStore>>,
    /// Context window size reported in `usage_update` notifications (ACP
    /// reports context occupancy; the provider reports raw token counts).
    pub context_window: u64,
}

impl AcpOptions {
    pub fn new(factory: SessionFactory) -> Self {
        Self {
            factory,
            store: None,
            context_window: 200_000,
        }
    }
}

struct SessionSlot {
    /// Serializes turns: `session/prompt` holds this for the whole turn.
    entry: tokio::sync::Mutex<SessionEntry>,
    /// The live turn's cancel token — reachable while `entry` is held by the
    /// turn itself, which is exactly when `session/cancel` needs it.
    cancel: Mutex<tokio_util::sync::CancellationToken>,
}

struct SessionEntry {
    session: Session,
    cwd: PathBuf,
    /// How many of `session.messages()` are already in the store.
    persisted: usize,
}

#[derive(Clone)]
struct Shared {
    factory: SessionFactory,
    store: Option<Arc<SqliteStore>>,
    context_window: u64,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionSlot>>>>,
}

impl Shared {
    fn mint_id(&self) -> Result<String, String> {
        match &self.store {
            Some(store) => store
                .create_session(None)
                .map(|record| record.id)
                .map_err(|e| e.to_string()),
            None => Ok(uuid::Uuid::new_v4().simple().to_string()),
        }
    }

    fn build_slot(&self, cwd: &Path, history: Vec<Message>) -> Result<Arc<SessionSlot>, String> {
        let parts = (self.factory)(cwd)?;
        let cancel = parts.ctx.cancel.clone();
        let persisted = history.len();
        let session = Session::resume(
            parts.provider,
            parts.registry,
            parts.ctx,
            parts.config,
            history,
        );
        Ok(Arc::new(SessionSlot {
            entry: tokio::sync::Mutex::new(SessionEntry {
                session,
                cwd: cwd.to_path_buf(),
                persisted,
            }),
            cancel: Mutex::new(cancel),
        }))
    }

    fn slot(&self, id: &str) -> Option<Arc<SessionSlot>> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .get(id)
            .cloned()
    }
}

/// Build the ACP agent. The returned value is an ACP "connectable": drive it
/// with `.connect_to(Stdio::new())`, a `Lines` transport over a WebSocket, or
/// hand it to a `Client.builder().connect_with(...)` as an in-process test
/// transport.
pub fn agent(options: AcpOptions) -> impl ConnectTo<Client> {
    let shared = Shared {
        factory: options.factory,
        store: options.store,
        context_window: options.context_window,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let init_shared = shared.clone();
    let new_shared = shared.clone();
    let load_shared = shared.clone();
    let prompt_shared = shared.clone();
    let cancel_shared = shared;

    Agent
        .builder()
        .name("ac")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version).agent_capabilities(
                        AgentCapabilities::new().load_session(init_shared.store.is_some()),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                let shared = new_shared.clone();
                let id = match shared.mint_id() {
                    Ok(id) => id,
                    Err(e) => return responder.respond_with_internal_error(e),
                };
                if let Some(store) = &shared.store {
                    let meta = serde_json::json!({ "cwd": req.cwd });
                    let _ = store.set_meta(&id, &meta);
                }
                let slot = match shared.build_slot(&req.cwd, Vec::new()) {
                    Ok(slot) => slot,
                    Err(e) => {
                        // Don't leave a ghost row a recents picker would
                        // list forever.
                        if let Some(store) = &shared.store {
                            let _ = store.delete_session(&id);
                        }
                        return responder.respond_with_internal_error(e);
                    }
                };
                shared
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .insert(id.clone(), slot);
                responder.respond(NewSessionResponse::new(SessionId::new(id)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                let shared = load_shared.clone();
                if shared.store.is_none() {
                    return responder.respond_with_error(Error::method_not_found());
                }
                // Store reads + the O(history) replay must not stall the
                // dispatch loop (which also carries session/cancel for other
                // sessions) — same shape as the prompt handler.
                cx.spawn({
                    let cx = cx.clone();
                    async move {
                        run_load(shared, req, cx, responder);
                        Ok(())
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let shared = prompt_shared.clone();
                let session_id = req.session_id.clone();
                let Some(slot) = shared.slot(&session_id.0) else {
                    return responder.respond_with_error(Error::resource_not_found(Some(
                        session_id.0.to_string(),
                    )));
                };
                let user_text = prompt_text(&req);

                // The handler must return immediately — doing the turn inline
                // would block the dispatch loop and session/cancel with it.
                cx.spawn({
                    let cx = cx.clone();
                    async move {
                        run_prompt(shared, slot, session_id, user_text, cx, responder).await;
                        Ok(())
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx: ConnectionTo<Client>| {
                // Only cancel when a turn is actually in flight (the entry
                // mutex is held exactly for the duration of a turn). An idle
                // cancel — stop button racing turn completion — must NOT
                // poison the live token, or the next prompt would be answered
                // with a spurious Cancelled.
                if let Some(slot) = cancel_shared.slot(&n.session_id.0)
                    && slot.entry.try_lock().is_err()
                {
                    slot.cancel.lock().expect("cancel lock").cancel();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Serve the agent over stdio — the standard ACP hosting shape (Zed et al.
/// spawn the agent binary and speak newline-delimited JSON-RPC).
pub async fn serve_stdio(options: AcpOptions) -> agent_client_protocol::Result<()> {
    agent(options).connect_to(Stdio::new()).await
}

/// The spawned body of `session/load`: verify, replay, rebuild, respond.
fn run_load(
    shared: Shared,
    req: LoadSessionRequest,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<LoadSessionResponse>,
) {
    let store = shared.store.clone().expect("checked by the handler");
    let id = req.session_id.0.to_string();

    // Loading over a session whose turn is running would fork it: the
    // orphaned slot would keep streaming and persisting while the map points
    // at the replacement, and its turn would become uncancellable.
    if let Some(existing) = shared.slot(&id)
        && existing.entry.try_lock().is_err()
    {
        let _ = responder.respond_with_internal_error(
            "a turn is in flight for this session; cancel it before loading",
        );
        return;
    }

    match store.get_session(&id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = responder.respond_with_error(Error::resource_not_found(Some(id.clone())));
            return;
        }
        Err(e) => {
            let _ = responder.respond_with_internal_error(e);
            return;
        }
    }
    let history = match store.load_messages(&id) {
        Ok(history) => history,
        Err(e) => {
            let _ = responder.respond_with_internal_error(e);
            return;
        }
    };
    // Replay history as ordinary session/update notifications — the
    // protocol's contract for load — before responding.
    for update in replay_updates(&history) {
        let _ = cx.send_notification(SessionNotification::new(req.session_id.clone(), update));
    }
    let slot = match shared.build_slot(&req.cwd, history) {
        Ok(slot) => slot,
        Err(e) => {
            let _ = responder.respond_with_internal_error(e);
            return;
        }
    };
    shared
        .sessions
        .lock()
        .expect("sessions lock")
        .insert(id, slot);
    let _ = responder.respond(LoadSessionResponse::new());
}

async fn run_prompt(
    shared: Shared,
    slot: Arc<SessionSlot>,
    session_id: SessionId,
    user_text: String,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<PromptResponse>,
) {
    // A stale slot Arc can carry an already-cancelled token right after a
    // cancel + rebuild; prefer the rebuilt slot from the map.
    let slot = if slot.cancel.lock().expect("cancel lock").is_cancelled() {
        match shared.slot(&session_id.0) {
            Some(fresh) if !fresh.cancel.lock().expect("cancel lock").is_cancelled() => fresh,
            _ => {
                let _ = responder.respond_with_internal_error(
                    "the session's context is cancelled; reload the session",
                );
                return;
            }
        }
    } else {
        slot
    };

    // A second prompt while a turn is in flight is a client error, not a
    // queue: fail it fast instead of silently serializing behind the lock.
    let Ok(mut entry) = slot.entry.try_lock() else {
        let _ =
            responder.respond_with_internal_error("a turn is already in flight for this session");
        return;
    };

    // First prompt of an untitled session names it — recents lists need a
    // human handle, and the host can always rename later.
    if let Some(store) = &shared.store
        && let Ok(Some(record)) = store.get_session(&session_id.0)
        && record.title.is_none()
    {
        let title: String = user_text.chars().take(60).collect();
        if !title.is_empty() {
            let _ = store.rename_session(&session_id.0, &title);
        }
    }

    // Persist the user's prompt BEFORE the turn: a connection that dies
    // mid-turn must not lose what the user typed. run_turn pushes the same
    // message into live history, so the counter arithmetic stays aligned.
    // The seq CAS turns a concurrent writer (another connection on the same
    // stored session) into a detectable conflict instead of a silent fork.
    if let Some(store) = &shared.store {
        let user_message = Message::text(Role::User, user_text.clone());
        match store.append_messages(
            &session_id.0,
            std::slice::from_ref(&user_message),
            Some(entry.persisted as u64),
        ) {
            Ok(_) => entry.persisted += 1,
            Err(ac_store::StoreError::SeqConflict { .. }) => {
                let _ = responder.respond_with_internal_error(
                    "the session's history advanced elsewhere; reload the session",
                );
                return;
            }
            // Transient store failure: run the turn anyway — the post-turn
            // persist retries the whole delta.
            Err(_) => {}
        }
    }

    // Scoped so the turn future's borrow of `entry` ends before persistence.
    let result = {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let turn = entry.session.run_turn(user_text, tx);
        tokio::pin!(turn);

        // Drain events into notifications while the turn runs; the sender
        // lives inside `turn`, so `rx` closes when the turn future completes.
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => {
                        if let Some(update) = event_update(event, shared.context_window) {
                            let _ = cx.send_notification(SessionNotification::new(
                                session_id.clone(),
                                update,
                            ));
                        }
                    }
                    None => break (&mut turn).await,
                },
                result = &mut turn => {
                    // Flush anything still queued before reporting the outcome.
                    while let Ok(event) = rx.try_recv() {
                        if let Some(update) = event_update(event, shared.context_window) {
                            let _ = cx.send_notification(SessionNotification::new(
                                session_id.clone(),
                                update,
                            ));
                        }
                    }
                    break result;
                }
            }
        }
    };

    let mut persist_conflict = false;
    if let Some(store) = &shared.store {
        let messages = entry.session.messages();
        if messages.len() > entry.persisted {
            match store.append_messages(
                &session_id.0,
                &messages[entry.persisted..],
                Some(entry.persisted as u64),
            ) {
                Ok(_) => entry.persisted = messages.len(),
                Err(ac_store::StoreError::SeqConflict { .. }) => persist_conflict = true,
                Err(_) => {}
            }
        }
    }

    let response = if persist_conflict {
        Err("the session's history advanced elsewhere; reload the session".to_string())
    } else {
        match &result {
            Ok(stop) => Ok(PromptResponse::new(map_stop(*stop))),
            Err(RuntimeError::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
            Err(RuntimeError::MaxIterations(_)) => {
                Ok(PromptResponse::new(StopReason::MaxTurnRequests))
            }
            Err(
                e @ (RuntimeError::Timeout
                | RuntimeError::Completion(_)
                | RuntimeError::Compaction(_)),
            ) => Err(e.to_string()),
        }
    };

    // A cancelled token can't be reset: rebuild the session from its own
    // history with a fresh context so the next prompt works.
    if matches!(result, Err(RuntimeError::Cancelled)) {
        let history = entry.session.messages();
        let persisted = entry.persisted;
        let cwd = entry.cwd.clone();
        drop(entry);
        match shared.build_slot(&cwd, history) {
            Ok(new_slot) => {
                new_slot.entry.try_lock().expect("fresh slot").persisted = persisted;
                shared
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .insert(session_id.0.to_string(), new_slot);
            }
            Err(_) => {
                // A stale cancelled slot must not linger — better an honest
                // resource_not_found on the next prompt than an endless run
                // of spurious Cancelled responses.
                shared
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .remove(&*session_id.0);
            }
        }
    }

    let _ = match response {
        Ok(response) => responder.respond(response),
        Err(message) => responder.respond_with_internal_error(message),
    };
}

fn prompt_text(req: &PromptRequest) -> String {
    let mut text = String::new();
    for block in &req.prompt {
        let rendered = match block {
            ContentBlock::Text(t) => t.text.clone(),
            // Baseline ACP contract: agents MUST accept resource links.
            // Render them as references the model can see.
            ContentBlock::ResourceLink(link) => format!("{} ({})", link.name, link.uri),
            // Image/audio/embedded blocks are legitimately unsupported —
            // PromptCapabilities are not advertised.
            _ => continue,
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&rendered);
    }
    text
}

fn map_stop(stop: ac_types::StopReason) -> StopReason {
    match stop {
        ac_types::StopReason::EndTurn | ac_types::StopReason::ToolUse => StopReason::EndTurn,
        ac_types::StopReason::MaxTokens => StopReason::MaxTokens,
        ac_types::StopReason::Refusal => StopReason::Refusal,
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read_file" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "list_files" | "glob" | "grep" => ToolKind::Search,
        "shell" => ToolKind::Execute,
        "fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())))
}

fn event_update(event: AgentEvent, context_window: u64) -> Option<SessionUpdate> {
    match event {
        AgentEvent::Text(text) => Some(SessionUpdate::AgentMessageChunk(text_chunk(text))),
        AgentEvent::Thinking(text) => Some(SessionUpdate::AgentThoughtChunk(text_chunk(text))),
        AgentEvent::ToolCall { id, name, input } => Some(SessionUpdate::ToolCall(
            ToolCall::new(id, name.clone())
                .kind(tool_kind(&name))
                .status(ToolCallStatus::InProgress)
                .raw_input(input),
        )),
        AgentEvent::ToolResult {
            id,
            output,
            is_error,
            ..
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new()
                .status(if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .raw_output(serde_json::Value::String(output)),
        ))),
        AgentEvent::Citation { url, title } => {
            let name = title.unwrap_or_else(|| url.clone());
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::ResourceLink(ResourceLink::new(name, url)),
            )))
        }
        AgentEvent::Usage(usage) => {
            // TokenUsage contract: the cache fields are subsets of
            // input_tokens — adding them would double-count warm-cache turns.
            let used = usage.input_tokens + usage.output_tokens;
            Some(SessionUpdate::UsageUpdate(UsageUpdate::new(
                used,
                context_window,
            )))
        }
        // The stop reason rides the PromptResponse; errors ride the JSON-RPC
        // error response. Neither is a session update. Compaction is recorded in
        // the session log and surfaced live on other transports; an ACP-native
        // notice for it is a deferred follow-up, not a conversational message.
        AgentEvent::TurnComplete { .. } | AgentEvent::Error(_) | AgentEvent::Compacted { .. } => {
            None
        }
    }
}

/// History → the update stream `session/load` replays. Tool calls replay as
/// already-completed.
fn replay_updates(history: &[Message]) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for message in history {
        for part in &message.content {
            match (message.role, part) {
                (Role::User, ContentPart::Text { text }) => {
                    updates.push(SessionUpdate::UserMessageChunk(text_chunk(text.clone())));
                }
                (_, ContentPart::ToolResult(result)) => {
                    updates.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        result.tool_use_id.clone(),
                        ToolCallUpdateFields::new()
                            .status(if result.is_error {
                                ToolCallStatus::Failed
                            } else {
                                ToolCallStatus::Completed
                            })
                            .raw_output(serde_json::Value::String(result.content.clone())),
                    )));
                }
                (Role::Assistant, ContentPart::Text { text }) => {
                    updates.push(SessionUpdate::AgentMessageChunk(text_chunk(text.clone())));
                }
                (Role::Assistant, ContentPart::Thinking { text, .. }) => {
                    updates.push(SessionUpdate::AgentThoughtChunk(text_chunk(text.clone())));
                }
                (Role::Assistant, ContentPart::ToolUse(tool_use)) => {
                    updates.push(SessionUpdate::ToolCall(
                        ToolCall::new(tool_use.id.clone(), tool_use.name.clone())
                            .kind(tool_kind(&tool_use.name))
                            .raw_input(tool_use.input.clone()),
                    ));
                }
                _ => {}
            }
        }
    }
    updates
}

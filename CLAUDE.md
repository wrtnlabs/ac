# AC (Agent Core) — architecture doctrine

AC is an **app-agnostic AI agent runtime**: providers, the agent loop, hard built-in tools, skills, MCP, sandboxing, and an ACP serving layer — as a kit. It is UI-free like codex-rs's `core`, consumable like a library (which codex-core is not), and never welded to a host framework (the mistake that makes Zed's agent crates unusable outside Zed). AC must work for any host — a desktop app, an editor plugin, a headless CLI — without knowing which one it is serving.

Crates are `ac-*`. The workspace is source-public but not published to crates.io (`publish = false`).

## The one rule that keeps this honest

**No `ac-*` crate may ever name a consumer concept.** No host-application domain semantics, no host-specific tools. Apps reach in through the five seams below; the dependency arrow never points outward. If a change needs the kit to know about a host, the change is wrong — extend a seam instead. A second toy host (the `ac-cli` generic agent) must stay alive forever as the proof.

## Crate map (dependency arrows point down)

```
ac-cli                     smoke binary / generic host (phase 1: raw completion; later: full generic agent)
ac-acp                     ACP Agent-side impl (adopt agent-client-protocol crate) — phase 4
ac-runtime                 THE LOOP: Session/Turn/Task, step hooks, tool router, read-before-write,
                           compaction, cancellation, typed event stream — phase 2
ac-tools                   hard built-ins: read/write/edit file, ls/glob, grep, shell, fetch — phase 2
ac-tool                    Tool trait, type-erased ToolDyn, registry, JSON-schema spec serialization — phase 2
ac-skills                  SKILL.md (agentskills.io) parser + layered user/project/bundled resolver — phase 3
ac-mcp                     LIVE: rmcp 2.x adapter — McpConnection discovers server tools and registers
                           them as RawTool entries in the same registry as built-ins; errors-as-data,
                           cancel-raced calls, annotations untrusted by default — phase 3
ac-sandbox                 seatbelt (macOS) / landlock+seccompiler (Linux) mechanism; policy injected — phase 3
ac-store                   SessionStore trait + rusqlite impl (+ later JSONL rollout) — phase 3
ac-provider-openrouter     wire crate: reqwest + eventsource-stream SSE, cache_control breakpoints,
                           usage accounting, retry taxonomy — phase 1 (live)
ac-provider                Provider trait (one required stream_completion), CompletionRequest — phase 1 (live)
ac-types                   zero-dep foundation: messages, content parts, CompletionEvent, ToolSpec,
                           TokenUsage, error taxonomy — phase 1 (live)
```

## The five injection seams (how apps plug in)

1. **PathPolicy** — built-in fs tools are compiled in but never decide *where* they may act; the host implements `resolve_read/resolve_write`. (A generic host allows the cwd subtree; a document-oriented host can confine writes to a project directory its own tools select at runtime.)
2. **Step hooks** — per-loop-iteration hook to pin a forced tool, swap model, filter tools, edit system prompt (the AI-SDK `prepareStep` equivalent; forced step chains — e.g. "the first tool call must be the host's project-selection tool" — live host-side).
3. **Typed ctx Extensions** — a type-map slot on the run context so host tools carry host state without freezing the ctx struct (the codex `extension_data` pattern).
4. **Tool registration** — three sources, one registry: hard built-ins, host tools, MCP tools. Compiled-in tools use the typed `Tool` trait (schema derived via schemars); wire-discovered tools use `RawTool` (runtime spec passed through verbatim, input validated by the tool itself). Every tool gets a capability classification (read-only vs mutating) — enforced kit-level. MCP `ToolAnnotations` are server-claimed hints: MCP tools default to `Mutating` regardless of `readOnlyHint`, and a host honors the hint only via an explicit `trust_annotations` opt-in — a read-only permission mode must not be bypassable by a lying server.
5. **SandboxPolicy + SessionStore** — mechanism in the kit, policy/storage location from the host.

The kit ships **no prompts**. System prompt is host-supplied; the kit contributes tool specs only. Templates (when needed) use minijinja.

## Buy-vs-build ledger (verified 2026-07-20; don't relitigate without new evidence)

| Piece | Decision |
| --- | --- |
| Serving protocol | ADOPT `agent-client-protocol` (ACP, 1.x) — de-facto agent↔client standard (Zed, JetBrains, Copilot/Gemini CLI). Keep the SDK behind our own seam; pin minors. |
| MCP | ADOPT `rmcp` 2.x (official org; codex + goose use it). Pin the major — it bumps fast. Copy codex `rmcp-client`'s OAuth/keyring + transport patterns, not its code (entangled). |
| Sandbox | BUILD ~1–2 KLOC over ADOPTED primitives: `landlock` + `seccompiler` (Linux), `sandbox-exec` Seatbelt profiles (macOS). All cross-platform sandbox crates are dead. Copy Anthropic sandbox-runtime's egress-proxy design (Node-only, not consumable). |
| Provider wire | BUILD (reqwest + `eventsource-stream`) — the codex/goose norm. No official Anthropic Rust SDK exists. `openrouter-rs` is the fork-ready fallback; avoid `rig`. |
| Tool schemas | ADOPT `schemars` 1.x. Tokens: `tiktoken-rs` rough estimates only (no accurate Claude tokenizer; trust server usage events). |
| JSON-RPC framing | BUILD (~200–400 lines over serde_json + tokio codec) — jsonrpsee has no stdio transport; ACP and rmcp both hand-roll. |
| Skills | BUILD small (spec surface is tiny; existing crates are micro-adoption). |
| Persistence | ADOPT `rusqlite` (bundled). Later: codex-style JSONL rollout for replay/fork. |

**License rule:** Zed's agent/model/sandbox crates are **GPL-3.0-or-later** — copy their *designs* (unified `LanguageModelCompletionEvent` shape, `Sandbox::wrap` API), NEVER their code. codex-rs is Apache-2.0 and vendor-friendly (cleanest lift: `execpolicy`; `apply-patch`/`sandboxing` are entangled with codex internals — reference only). The `codex-*` crates ON crates.io are a stale third-party fork — never depend on them.

## Known deferred gaps (own them when the phase lands)

- **Egress / SSRF:** `fetch` and `shell` reach any network the host process can (e.g. `http://169.254.169.254` metadata, loopback). Containment today is filesystem-only. The domain-allowlist egress proxy is `ac-sandbox`'s job (mirror Anthropic sandbox-runtime's proxy design) — do not paper over it with ad-hoc IP checks in the tools.
- **No OS process sandbox yet:** `shell` is cwd-contained and reaps its process group, but the command can read/write anything the host user can. `ac-sandbox` (seatbelt / landlock+seccompiler) closes this.
- **Truncated-stream detection:** the loop treats a stream that ends without an explicit `Stop` as a clean `EndTurn`. Acceptable while the provider contract guarantees `Stop`; revisit if a provider can end early.
- **MCP surface is tools-only, snapshot-at-register:** resources/prompts/sampling/elicitation are not surfaced; `toolListChanged` notifications are ignored (a host refreshes by re-running `register_tools`); remote servers (streamable-HTTP transport + OAuth) are not wired — child-process stdio and in-process transports are. Copy codex `rmcp-client`'s OAuth/keyring patterns when remote lands.

## Reference reading

- [zed](https://github.com/zed-industries/zed) — `crates/language_model_core` (the event enum to mirror), `crates/anthropic` (hand-rolled SSE shape), `crates/agent/src/thread.rs` `run_turn_internal` (concurrent-tools select! loop), `crates/sandbox` (best command-wrapper API design).
- [codex](https://github.com/openai/codex) `codex-rs/` — `core/src/session/` + `core/src/tasks/` (Session/Turn/Task model), `core/src/tools/{registry,router}.rs`, `sandboxing/src/seatbelt.rs`, `app-server-protocol` (versioned JSON-RPC discipline), `rollout/` (JSONL persistence).
- Vercel eve (TS, concepts only): per-tool declarative approval policies (`never/once/always/predicate`), lazy markdown skills, channels-as-adapters, MCP-as-one-connection-kind.

## Conventions

- Edition 2024, workspace deps pinned in the root `Cargo.toml`. `cargo check` + `cargo test` + `cargo clippy` must stay green on every commit.
- Errors that tools return to the model are **data, not `Err`** (Zed's `Result<Output, Output>` trick) — reserve `Err` for infrastructure failure.
- Token accounting is server-reported (`UsageUpdate` events); never client-tokenize for truth.
- Comments: only for non-obvious *why*. No restating code.

## Consumers

Hosts pin AC as a git submodule (or path dependency) until it is distributed properly. Host-side tiers — app tools, prompts, daemon/CLI binaries — live in the host's own repository, never here.

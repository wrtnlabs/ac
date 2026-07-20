# AC (Agent Core) — architecture doctrine

AC is an **app-agnostic AI agent runtime**: providers, the agent loop, hard built-in tools, skills, MCP, sandboxing, and an ACP serving layer — as a kit. It is UI-free like codex-rs's `core`, consumable like a library (which codex-core is not), and never welded to a host framework (the mistake that makes Zed's agent crates unusable outside Zed). Canvas is the first consumer; AC must work for a host that has never heard of slides.

Codename only — this workspace is not published. Crates are `ac-*`.

## The one rule that keeps this honest

**No `ac-*` crate may ever name a consumer concept.** No "canvas", "deck", "slide", "workspace/project" semantics, no host-specific tools. Apps reach in through the five seams below; the dependency arrow never points outward. If a change needs the kit to know about a host, the change is wrong — extend a seam instead. A second toy host (the `ac-cli` generic agent) must stay alive forever as the proof.

## Crate map (dependency arrows point down)

```
ac-cli                     smoke binary / generic host (phase 1: raw completion; later: full generic agent)
ac-acp                     ACP Agent-side impl (adopt agent-client-protocol crate) — phase 4
ac-runtime                 THE LOOP: Session/Turn/Task, step hooks, tool router, read-before-write,
                           compaction, cancellation, typed event stream — phase 2
ac-tools                   hard built-ins: read/write/edit file, ls/glob, grep, shell, fetch — phase 2
ac-tool                    Tool trait, type-erased ToolDyn, registry, JSON-schema spec serialization — phase 2
ac-skills                  SKILL.md resolver (port of Canvas's @workspace/skills layered resolver) — phase 3
ac-mcp                     thin adapter over rmcp: discovered MCP tools -> ToolDyn in the registry — phase 3
ac-sandbox                 seatbelt (macOS) / landlock+seccompiler (Linux) mechanism; policy injected — phase 3
ac-store                   SessionStore trait + rusqlite impl (+ later JSONL rollout) — phase 3
ac-provider-openrouter     wire crate: reqwest + eventsource-stream SSE, cache_control breakpoints,
                           usage accounting, retry taxonomy — phase 1 (live)
ac-provider                Provider trait (one required stream_completion), CompletionRequest — phase 1 (live)
ac-types                   zero-dep foundation: messages, content parts, CompletionEvent, ToolSpec,
                           TokenUsage, error taxonomy — phase 1 (live)
```

## The five injection seams (how apps plug in)

1. **PathPolicy** — built-in fs tools are compiled in but never decide *where* they may act; the host implements `resolve_read/resolve_write`. (Canvas: deck containment + report_workdir gating. Generic host: cwd subtree.)
2. **Step hooks** — per-loop-iteration hook to pin a forced tool, swap model, filter tools, edit system prompt (the AI-SDK-v6 `prepareStep` equivalent; Canvas's forced report_workdir → load_skill chain lives host-side).
3. **Typed ctx Extensions** — a type-map slot on the run context so host tools carry host state without freezing the ctx struct (codex `extension_data` / Canvas `HwpToolCtx` lesson).
4. **Tool registration** — three sources, one registry: hard built-ins, host tools, MCP tools. Every tool gets a capability classification (read-only vs mutating) — enforced kit-level.
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

## Reference reading (local clones, keep pulled)

- `~/Documents/GitHub/zed` — `crates/language_model_core` (the event enum to mirror), `crates/anthropic` (hand-rolled SSE shape), `crates/agent/src/thread.rs` `run_turn_internal` (concurrent-tools select! loop), `crates/sandbox` (best command-wrapper API design).
- `~/Documents/GitHub/codex/codex-rs` — `core/src/session/` + `core/src/tasks/` (Session/Turn/Task model), `core/src/tools/{registry,router}.rs`, `sandboxing/src/seatbelt.rs`, `app-server-protocol` (versioned JSON-RPC discipline), `rollout/` (JSONL persistence).
- Vercel eve (TS, concepts only): per-tool declarative approval policies (`never/once/always/predicate`), lazy markdown skills, channels-as-adapters, MCP-as-one-connection-kind.

## Conventions

- Edition 2024, workspace deps pinned in the root `Cargo.toml`. `cargo check` + `cargo test` + `cargo clippy` must stay green on every commit.
- Errors that tools return to the model are **data, not `Err`** (Zed's `Result<Output, Output>` trick) — reserve `Err` for infrastructure failure.
- Token accounting is server-reported (`UsageUpdate` events); never client-tokenize for truth.
- Comments: only for non-obvious *why*. No restating code.

## Consumer wiring

Canvas consumes AC as a git submodule at `ac/` in `wrtnlabs/canvas` until AC is distributed properly. The Canvas-side tiers (`canvas-deck`, `canvas-tools`, `canvas-app`, daemon/CLI binaries) live in the canvas repo, not here.

# AC (Agent Core) — architecture doctrine

AC is an **app-agnostic AI agent runtime**: providers, the agent loop, hard built-in tools, skills, MCP, sandboxing, and an ACP serving layer — as a kit. It is UI-free like codex-rs's `core`, consumable like a library (which codex-core is not), and never welded to a host framework (the mistake that makes Zed's agent crates unusable outside Zed). AC must work for any host — a desktop app, an editor plugin, a headless CLI — without knowing which one it is serving.

Crates are `ac-*`. The workspace is source-public but not published to crates.io (`publish = false`).

## The one rule that keeps this honest

**No `ac-*` crate may ever name a consumer concept.** No host-application domain semantics, no host-specific tools. Apps reach in through the five seams below; the dependency arrow never points outward. If a change needs the kit to know about a host, the change is wrong — extend a seam instead. A second toy host (the `ac-cli` generic agent) must stay alive forever as the proof.

## Crate map (dependency arrows point down)

```
ac-web                     LIVE: the ACP web harness — axum server bridging WebSocket frames ↔ the
                           same ac-acp agent that serves stdio; hand-written browser ACP client
                           (ui/index.html); zero agent logic in the binary. The EDITOR-ecosystem proof.
ac-ai-sdk                  LIVE: the Vercel AI SDK adapter — lib maps AgentEvent ↔ the v5 UI Message
                           Stream Protocol (UIMessageChunk out, UIMessage hydration in); bin is an axum
                           host serving it over SSE to a stock useChat React app (examples/web-react).
                           The WEB-ecosystem proof. Sibling of ac-acp, not stacked on it.
ac-cli                     smoke binary / generic host (phase 1: raw completion; later: full generic agent)
ac-acp                     LIVE: Agent-side ACP over the official agent-client-protocol crate (~1.2,
                           minor-pinned). initialize/new/prompt/cancel/load; AgentEvent → session/update;
                           prompt work spawned off the dispatch loop; cancelled turns respond
                           StopReason::Cancelled and the session rebuilds from its own history;
                           store present → loadSession capability + persistence + first-prompt titling
ac-runtime                 THE LOOP: Session/Turn/Task, step hooks, tool router, read-before-write,
                           compaction, cancellation, typed event stream — phase 2
ac-tools                   hard built-ins: read/write/edit file, ls/glob, grep, shell, fetch — phase 2
ac-tool                    Tool trait, type-erased ToolDyn, registry, JSON-schema spec serialization — phase 2
ac-skills                  LIVE: SKILL.md skills mirroring the codex-rs architecture (studied
                           2026-07-21) — skills are INJECTED TEXT, not a tool: catalog_markdown()
                           renders "- name: description (file: /abs/SKILL.md)" + usage prose for the
                           system prompt; $name / [$name](path) mentions select skills (unambiguous
                           names only, env-var lookalikes excluded); build_skill_injections() wraps
                           SKILL.md verbatim in <skill><name/><path/>…</skill> for the turn input;
                           the model reads companion files itself at the listed paths. Hand-rolled
                           scalar-only frontmatter (richer YAML skipped with a reason, never
                           mis-parsed; name falls back to the dir name), recursive depth-6 discovery,
                           duplicate names kept (ambiguity blocks plain mentions), dep-light
                           (thiserror only). No load_skill tool, no allowed-tools enforcement, no
                           per-skill permission widening — codex parity
ac-mcp                     LIVE: rmcp 2.x adapter — McpConnection discovers server tools and registers
                           them as RawTool entries in the same registry as built-ins; errors-as-data,
                           cancel-raced calls, annotations untrusted by default — phase 3
ac-sandbox                 LIVE (v1): kernel-enforced OS sandbox for the shell tool via the
                           SandboxLauncher seam — macOS Seatbelt (sandbox-exec) / Linux landlock +
                           seccompiler + setrlimit, self-applied in pre_exec (no bwrap/userns).
                           Filesystem containment + syscall restriction + resource caps + network
                           on/off; fail-closed strict|degraded|off envelope; native Windows honestly
                           off. Domain-egress allowlist is the deferred v2. See docs/ac-sandbox.md
ac-store                   LIVE: rusqlite session+message store — opaque or caller-adopted ids,
                           host-owned meta JSON, seq-ordered message log (seq-CAS append);
                           pairs with Session::resume for reload recovery
                           (+ later JSONL rollout) — phase 3
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

The kit ships **no prompts**, with one scoped exception: system prompt and persona are host-supplied, the kit contributes tool specs — and `ac-skills` ships the model-facing skills catalog + usage prose (`catalog_markdown`, the `<skill>` injection format), because that text IS the skills mechanism (codex ships it in core-skills for the same reason). Hosts opt in by calling it. Templates (when needed) use minijinja.

**Serving is layered, not one protocol.** Clients speak a *wire* to the core; they never link against the runtime. Two wires ship, one per ecosystem, and **both are thin adapters off the one `AgentEvent` stream — neither is stacked on the other**:
- **ACP** (`ac-acp`, out-of-process / editors) — the standardized agent↔host RPC (Zed, JetBrains, stdio). What varies enters via `AcpOptions` (`SessionFactory` + optional `SqliteStore`).
- **The Vercel AI SDK UI Message Stream Protocol** (`ac-ai-sdk`, web/React) — so a stock `@ai-sdk/react` `useChat` app renders an AC agent with zero custom client code. `ChunkEncoder` maps `AgentEvent → UIMessageChunk`.

The AI SDK is two halves and only one overlaps AC: its *server/provider* half (`streamText`) is what AC **replaces** — running AC under it is the force-fit to avoid; its *client/UI* half (`useChat`) is the ecosystem AC **feeds**. A host binary (`ac-web`, `ac-ai-sdk`) is transport glue only; agent logic in it is the smell. UI conveniences (a session-list endpoint) may be host endpoints; the conversation is all protocol.

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

- **OS process sandbox — v1 LIVE (`ac-sandbox`), see [docs/ac-sandbox.md](docs/ac-sandbox.md).** The `shell` tool's command is kernel-contained when the host installs a `SandboxLauncher` on `ToolCtx` (the `ac` binary does by default): macOS Seatbelt / Linux `landlock`+`seccompiler`+`setrlimit` — filesystem containment + syscall restriction + resource caps + network on/off, self-applied in `pre_exec` (no bwrap/userns, dodging the distro trap). Fail-closed with a `strict|degraded|off` mode on the shell result envelope; native Windows is honestly `off`. **The one rule: only ship `actual` (kernel-enforced) mechanisms; where we can't, be loudly `off` — never an advisory approximation called a sandbox.** Anti-pattern to flag: a `shell`-like tool that spawns a child WITHOUT routing through `ctx.sandbox` when one is installed (it silently drops isolation), or a launcher that falls back to an unsandboxed spawn instead of erroring under a fail-closed policy.
- **Egress / SSRF (deferred to `ac-sandbox` v2):** `fetch` and `shell` reach any network the host process can (e.g. `http://169.254.169.254` metadata, loopback). v1's network containment is binary on/off (off = no reachable socket, kernel-enforced); a **domain allowlist** is only honest as the full kernel-block-then-proxy design (sever egress at the kernel, funnel through a host proxy that owns DNS, canonicalize hosts before match) — that is a v2 phase, not ad-hoc IP checks in the tools. See docs/ac-sandbox.md.
- **Queue/steer, fork/rewind, and compaction are studied and specced, not built.** Three designs of record grounded in a full read of codex-rs (2026-07-21): [docs/ac-queue-steer.md](docs/ac-queue-steer.md) (mid-turn user input steers into the running turn at step boundaries — plain user message, no wrapper; queueing stays host-side; cancel drops pending steers and records a deliberate-interrupt marker), [docs/ac-fork.md](docs/ac-fork.md) (append-only JSONL event log per session — the codex "rollout" — with fork = copy-truncated-prefix + lineage, in-place rewind = an appended logical marker, forks only at turn boundaries), and [docs/ac-compaction.md](docs/ac-compaction.md) (compaction as a handoff to another LLM — one lifecycle, manual/pre-turn/mid-turn/model-switch triggers, summarize/fresh-window strategies, user messages survive verbatim). Implement in that doc-stated dependency order: rollout substrate → fork → compaction → steer (steer touches only the run loop and can land independently). When implementing, the docs are the contract.
- **Truncated-stream detection:** the loop treats a stream that ends without an explicit `Stop` as a clean `EndTurn`. Acceptable while the provider contract guarantees `Stop`; revisit if a provider can end early.
- **Same-session concurrency across connections is detected, not prevented:** two ACP connections (e.g. two browser tabs) can `session/load` the same stored session; a concurrent writer surfaces as a seq-CAS conflict (`StoreError::SeqConflict` → prompt error telling the client to reload) rather than a silent history fork. Prevention needs process-wide shared session state (an `AcpOptions` seam) — do it when a real host needs it.
- **`StopReason::Refusal` keeps the refused turn in history:** the ACP spec says a refused prompt "won't be included in the next prompt", but the kit currently persists and replays it. Honoring it needs a `Session::truncate` + store truncation; deferred until a provider actually emits Refusal in practice.
- **Skills mirror codex-rs, deliberately partially.** The architecture (text injection, mention selection, read-the-file-yourself progressive disclosure) is codex's; these codex subsystems were studied and *deliberately skipped* for now: the `agents/openai.yaml` sidecar (interface/dependencies/policy metadata), `[[skills.config]]` enable/disable rules, the catalog token-budget degradation ladder, implicit-invocation telemetry, plugin namespacing, and remote/orchestrator/environment skill sources. A host that contains reads (like `ac-cli`) grants its skills roots read access statically at build (`ReadGrants` + `SandboxPolicy::read_also`) — skill use never changes policy at runtime, matching codex's removal of skill-scoped permission widening (their #15812). `allowed-tools` is intentionally NOT a kit concept (codex has no such field).
- **MCP surface is tools-only, snapshot-at-register:** resources/prompts/sampling/elicitation are not surfaced; `toolListChanged` notifications are ignored (a host refreshes by re-running `register_tools`); remote servers (streamable-HTTP transport + OAuth) are not wired — child-process stdio and in-process transports are. Copy codex `rmcp-client`'s OAuth/keyring patterns when remote lands.

## Reference reading

- [zed](https://github.com/zed-industries/zed) — `crates/language_model_core` (the event enum to mirror), `crates/anthropic` (hand-rolled SSE shape), `crates/agent/src/thread.rs` `run_turn_internal` (concurrent-tools select! loop), `crates/sandbox` (best command-wrapper API design).
- [codex](https://github.com/openai/codex) `codex-rs/` — `core/src/session/` + `core/src/tasks/` (Session/Turn/Task model), `core/src/tools/{registry,router}.rs`, `sandboxing/src/seatbelt.rs` (Apache-2.0 SBPL profile + seccomp set — **liftable** for `ac-sandbox`, see docs/ac-sandbox.md), `core-skills/` + `skills/` (the skill system `ac-skills` mirrors: catalog render, `$mention` syntax, `<skill>` injection format — studied in full 2026-07-21), `windows-sandbox-rs/` (bespoke restricted-token/firewall Windows sandbox — reference only, ~18k LOC, ships disabled), `app-server-protocol` (versioned JSON-RPC discipline), `rollout/` (JSONL persistence).
- Vercel eve (TS, concepts only): per-tool declarative approval policies (`never/once/always/predicate`), lazy markdown skills, channels-as-adapters, MCP-as-one-connection-kind.

## Conventions

- Edition 2024, workspace deps pinned in the root `Cargo.toml`. `cargo check` + `cargo test` + `cargo clippy` must stay green on every commit.
- Errors that tools return to the model are **data, not `Err`** (Zed's `Result<Output, Output>` trick) — reserve `Err` for infrastructure failure.
- Token accounting is server-reported (`UsageUpdate` events); never client-tokenize for truth.
- Comments: only for non-obvious *why*. No restating code.

## Consumers

Hosts pin AC as a git submodule (or path dependency) until it is distributed properly. Host-side tiers — app tools, prompts, daemon/CLI binaries — live in the host's own repository, never here.

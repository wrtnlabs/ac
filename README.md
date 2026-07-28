# AC — Agent Core

Application-agnostic agent backend framework in Rust. AC owns the reusable
runtime, composition harness, providers, generic tools, skills, MCP, sandbox,
store mechanisms, and serving projections. A host declares only what varies:
its prompts, skills, domain tools, policy, persistence placement, and protocol
mapping. No crate in this workspace may know who its host is.

Read [CLAUDE.md](CLAUDE.md) for the architecture doctrine before touching code.

## Goals, and non-goals

**AC owns what is general and expensive to get right.** The agent loop and its
stop conditions; provider transport (streaming, tool calls, usage, prompt-cache
placement); a tool registry with typed schemas; path containment; an OS-level
sandbox (Seatbelt on macOS, landlock + seccomp + rlimit on Linux); a session
store with WAL and crash-safe sequencing; an append-only rollout log that fork,
rewind, and compaction project from; skills as injected text; MCP as a client.
It also ships a small [`ac-host`](crates/ac-host) composition harness that
assembles those injected pieces into fresh or resumed sessions and drives a
borrowed turn as ordered events plus one terminal result. These are the parts a
host should not re-derive.

**AC deliberately decides none of these for you**, and will not grow an opinion
about them:

- **No UI, and no wire protocol.** AC is a library. If you want a daemon, a
  socket, or an RPC surface, that is yours to define. [`ac-acp`](crates/ac-acp)
  and [`ac-ai-sdk`](crates/ac-ai-sdk) are thin adapters onto one event stream,
  not the boundary itself.
- **No storage semantics.** [`ac-store`](crates/ac-store) owns the container —
  identity, ordering, timestamps, transactions — and treats your `meta` as an
  opaque blob it never parses. Domain fields are yours; so is indexing them.
- **No app vocabulary, and no plugin registry for app kinds.** No crate here
  knows what its host makes. Behaviour is layered through the seams in
  [CLAUDE.md](CLAUDE.md), never through an `if app == …`.
- **No application framework or orchestration DSL.** AC is the agent backend
  framework; it does not own your process, product lifecycle, chains, graphs,
  or prompt language. `ac-host` wires the objects a host selected without
  deciding their domain meaning.

## Where the boundary falls: how you deliver a tool

**A tool's contract is its JSON Schema, not its implementation language.** That
sentence exists because getting it wrong is the most expensive mistake a host
can make with this kit. There are two delivery mechanisms for the same
contract, and the choice is about *coupling*, never about language:

**In-process, as a Rust `Tool`/`RawTool`** — when the tool is intimate with the
kit's own primitives: the path policy, the read-before-write ledger, the
sandbox launcher, per-turn cancellation. A filesystem or shell tool belongs
here, because its correctness *is* the containment.

**Out-of-process, over MCP** — when the tool wraps a library or service and
only exchanges schema-shaped data. Use
[`McpConnection::connect_command`](crates/ac-mcp) for a child process in any
language, or `connect_http_with` for a remote one, then `register_cached` to
register from a cached spec and dial lazily on first call. Set
`RegisterOptions.prefix = ToolPrefix::None` and the server's tool names
register verbatim, so an existing tool keeps the exact name your UI already
matches on.

**Do not port a working tool into Rust in order to "use AC".** A document
parser, a format converter, a vendor SDK wrapper — if it already exists and
speaks JSON in and JSON out, run it as an MCP server and register it. The kit
is Rust because the loop, the containment, and the store are Rust. Your tools
are whatever they already are.

## Known limits worth knowing before you adopt

Facts, not caveats-for-form. Each is a real constraint you would otherwise
discover late:

- **A tool result is a string.** `ToolResult.content` is `String`, so a tool
  cannot hand the model an image block directly; hosts that need pixels in the
  transcript inject them as a separate message today.
- **Sandbox network is binary.** `NetworkMode::{Off, On}` — kernel-enforced,
  but there is no per-domain allowlist. Domain filtering needs a proxy and is
  not built.
- **`deny_paths` nested inside a write root is macOS-only.** Seatbelt is
  last-match-wins, so a deny beats an enclosing allow. Landlock v1 is
  allow-only and has no deny rule, so on Linux a denied path *inside* a granted
  root is not carved out — it is only denied when it falls outside every
  granted root, which is where the kit's own default secret set lives. If your
  policy denies something nested in a write root (say `<repo>/.git/hooks`
  while granting `<repo>`), that boundary holds on macOS and does not on
  Linux. Grant the complement instead of relying on the deny.
- **Server-side tools are provider-shaped.** Web search rides
  `AgentConfig::server_tools`, which the OpenRouter provider encodes as a
  request plugin. Only function tools can enter `tools[]`, so the model cannot
  *call* a server tool by name.
- **`ac-store` has one index** (`updated_at`). Domain queries over your `meta`
  blob are scans; if you need them indexed, keep that data host-side.
- **macOS and Linux only** for the sandbox. Elsewhere it reports `Off` rather
  than pretending.

## Try it

```sh
# Offline end-to-end proof — no network, no API key. Drives the real built-in
# tools over the real runtime loop against a temp dir via a scripted provider.
cargo test -p ac-cli

# Live generic agent — reads/writes/searches files and runs shell, contained
# to <dir> (default: current directory).
OPENROUTER_API_KEY=... cargo run -p ac-cli -- [--model <id>] [--dir <path>] \
  [--skills <dir>] [--skill <name>] [--web-search] "your prompt"
```

`--skills <dir>` points at a directory scanned (recursively, bounded depth) for `SKILL.md` skills: valid skills are advertised in the system prompt as a catalog of `name: description (file: path)` entries (invalid candidates are reported on stderr with a reason), a `$name` mention in the prompt injects that skill's `SKILL.md` into the turn input as a `<skill>` block, and the model reads companion `scripts/`/`references/` itself at the listed paths — there is no skill tool (codex-style text injection). `--skill <name>` selects a skill up front, exactly as if the prompt mentioned `$name`. The reusable resolver also offers an optional ordered direct-child policy with directory-name shadowing; both modes parse real YAML frontmatter, including nested metadata, sequences, flow collections, and block scalars.

Status: the end-to-end agent loop works. `ac` is a generic filesystem agent — the OpenRouter provider wired to the built-in tool registry (`read_file`, `write_file`, `edit_file`, `list_files`, `glob`, `grep`, `shell`, `fetch`) over the `ac-runtime` loop, all writes contained by a path policy. Hosts that expose different tool schemas or result presentation reuse the same `FileTimes`/`FileMutation`, resolved-path read/list, fuzzy-replace, and `execute_shell` primitives rather than building another file or process runtime. Proven offline in `crates/ac-cli/tests/e2e.rs`, which exercises the whole stack (real tools, real loop, real temp dir) via a scripted mock provider and asserts both the on-disk ground truth and the policy safety invariant.

Host assembly uses [`AgentHostBuilder`](crates/ac-host/src/lib.rs): inject the
provider, registry, `ToolCtx`, and `AgentConfig`; optionally declare step hooks,
observation hooks, and reactive sections; then build a fresh or resumed
`Session`. [`TurnPump`](crates/ac-host/src/lib.rs) borrows that session for one
turn and preserves the runtime's event order before yielding exactly one
terminal `Result`. Product protocol mappings, transports, and daemons remain
host-owned; reusable protocol projections live in AC.

`ac-provider-openrouter` also exposes a typed image-generation client. It owns the provider request/response mechanics, decoding, remote-image download, cancellation, timeout, redirect handling, and response-size bounds; a host still owns its tool schema, reference-path authorization, naming, and output sink.

## License

Apache-2.0 — see [LICENSE](LICENSE).

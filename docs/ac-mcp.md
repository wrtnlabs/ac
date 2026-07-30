# RFC: MCP integration — wire-discovered tools in the same registry

**Status:** implemented — specification of record (2026-07-21). **Amended 2026-07-23:** §8
(server configuration format) records the de-facto `mcpServers` JSON object as the portable
contract. The low-level connection API remains format-agnostic; the opt-in managed layer ships
that standard format and stock file persistence. Other tools' native configs are one-way
importers, never the contract.
**Amended 2026-07-24:** the remote-transport deferral (§9) is partially closed. A
streamable-HTTP client connect path ships behind an opt-in `http` cargo feature (bearer token
and extra headers at connect); an observably-unauthorized refusal — HTTP 401/403 or the
transport's own auth-required signals — now surfaces as a distinct auth error class, with
ambiguous failures keeping their existing class. Separately, an offline catalog closes the
connect-time-snapshot gap: a live connection exports its discovered tools as serializable
cached specs, and a cached-registration path registers them connection-free with a
host-injected dialer — first call dials, the batch memoizes the live connection, and a failed
dial is one failed tool result, retried on the next call (§5).
**Amended 2026-07-28:** cached specs may carry a host-chosen provider-safe registry name while
retaining the raw remote call name. A persisted catalog MUST be bound to the exact server
definition that produced it; changing a command, URL, headers, or auth policy invalidates that
server's snapshot before registration. Hosts SHOULD translate config to connections through one
adapter shared by probes, enumeration/OAuth, and lazy dialers (§5, §8).
**Amended 2026-07-28 (managed MCP):** `ac_mcp::managed` now supplies that shared adapter and the
standard control plane: ordered portable config, deterministic fingerprints, a version-2 offline
catalog, lazy mounting, status/auth snapshots, probe, upsert/remove, refresh/backfill, stock
atomic file stores (mode `0600` on Unix), and a generation-safe OAuth credential adapter.
Storage and credential traits remain injectable. Hosts choose paths, inherited stdio environment,
OAuth identity and callback presentation, import sources, and RPC/UI projection (§5, §7, §8).
The opt-in `http` feature also owns application-agnostic OAuth 2.1 mechanics:
protected-resource/authorization-server discovery, dynamic client registration, PKCE,
authorization URL construction, code exchange, the loopback callback state machine, and the
interactive coordinator. The coordinator serializes same-server flows, leases the one callback
endpoint, observes caller and host cancellation throughout the flow, and guarantees pending-state
cleanup. The managed layer supplies a stock semantic credential store and a configured
enumerator; hosts supply client metadata, callback/browser presentation, storage paths or custom
stores, and policy (§7).
**Amended 2026-07-30:** portable stdio definitions also carry `envVars` and `cwd`; remote
definitions carry `envHeaders` and `bearerTokenEnvVar`. The symbolic fields persist environment
*names* and resolve their values through host policy at each dial; literal `env` and `headers`
remain literal durable configuration. AC never reads ambient process state by itself. An explicit
`requalify_catalog` migration may rewrite already-cached provider-visible names without dialing;
absent that call, existing names remain verbatim (§5, §8).
**Amended 2026-07-30 (hardening):** remote MCP requires HTTPS except for HTTP on a literal
loopback IP, before a transport or credential-bearing request is constructed. Discovery is
manually paged with page, cursor, tool-count, per-entry, schema-depth/size, and aggregate
catalog limits. OAuth discovery follows the protected-resource order for pathful issuers,
requires an exact metadata issuer, and validates an authorization response's RFC 9207 `iss`
when present or advertised. OAuth bodies, rendered tool results, and surfaced errors are
bounded; configured secret values are redacted. Stock file writes set final private
permissions before file sync and sync the containing directory after atomic replacement on
Unix. The remaining pre-deserialization transport allocation gap is recorded in §9.
**Requires:** [ac-tools.md](ac-tools.md) (the tool registry, the raw (runtime-described) registration
path, errors-as-data), [ac-provider.md](ac-provider.md) (tool specs ride every sampling request —
the exposure that motivates the name floor defined in §2). **Required by:** hosts that expose
managed MCP tools. **Interacts with:**
[ac-approvals.md](ac-approvals.md) (capability classification is the input to permission
decisions), [ac-security.md](ac-security.md) (the untrusted-counterparty posture).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

MCP puts an ecosystem of tools one process-spawn away. The naive integration builds a second
tool plane for them — separate dispatch, separate permission checks, separate events — and every
downstream contract forks in two. The kit's answer is that there is no second plane: a tool
discovered over the wire enters the **same registry** as the compiled-in built-ins, as a
raw tool — the form of [ac-tools.md](ac-tools.md) §2.1 at a protocol boundary,
that document's third path — and from that moment the run loop cannot tell them apart.

What makes this nontrivial is that an MCP server is an *untrusted counterparty on a wire*:
possibly buggy, possibly slow, possibly hostile — and a session is long-lived and expensive.
Five requirements shape the design:

- **R1 (one plane).** A discovered tool MUST be indistinguishable from a built-in to the run
  loop: dispatched by name, result fed into the next sampling request, events emitted in order.
- **R2 (verbatim contract).** The server's declared description and input schema MUST reach the
  model unaltered, and the kit MUST NOT validate arguments against a schema it does not own —
  the serving tool validates its own inputs.
- **R3 (distrust by default).** Nothing a server *declares* may weaken the host's permission
  posture. A permission mode keyed on tool capability MUST NOT be bypassable by a lying server.
- **R4 (session survival).** Once a message reaches AC's decoded-message boundary, no server
  behavior — crash, hang, garbage, oversized output — may cost more than one failed tool result.
  Failures are data the model sees, never a poisoned session or a terminated turn. The stock
  SDK transports' pre-deserialization allocation gap is explicit in §9.
- **R5 (total accounting).** Discovery-to-registration MUST be fully reported: every discovered
  tool is either registered or skipped with a stated reason. Nothing is dropped silently.

## 2. Model

A **connection** binds one client to one MCP server over a host-chosen transport (the covered
transports are a child process on stdio and, with `http`, Streamable HTTP) under a host-chosen
**server name** `s`. Discovery
takes the server's full paginated tool list at a point in time — a **snapshot**
`D = ⟨d₁, …, dₙ⟩` of declared tools, each `dᵢ` carrying a remote name `tᵢ`, an optional
description, an input schema, and optional **annotations** (the server's self-description:
read-only hints and the like).

**Naming.** Let `Σ = [A-Za-z0-9_-]`. Registry names under the default prefix scheme are

> `ν(s, t) = "mcp__" · s · "__" · t`   with   `Valid(s) ≜ s ≠ ε ∧ "__" ⊄ s ∧ ¬ends(s, "_")`

and `s, t ∈ Σ*`. `Valid` makes the decomposition provably unique: in `s · "__" · t` the first
occurrence of `"__"` cannot lie inside `s` (excluded) and cannot start one position early
(`s` would end in `"_"`, excluded), so it is exactly the delimiter — `ν(s₁,t₁) = ν(s₂,t₂)`
implies `s₁ = s₂ ∧ t₁ = t₂`. Without the trailing-underscore rule, server `a` with tool `_x`
and server `a_` with tool `x` would both register as `mcp__a___x` and silently replace each
other. Server names violating `Valid` are rejected at connect time, before the initialize
handshake.

**The name floor.** Tool specs are resent with every completion request, so *one*
out-of-contract name does not fail one call — it fails **every remaining request of the
session**. Registry names are therefore held to the strictest contract among supported
providers: `^[A-Za-z0-9_-]{1,64}$` (OpenAI-routed models enforce 64 bytes; others allow more).
The check runs on the *prefixed* name — a 61-byte remote name is fine bare and out of contract
once prefixed — and an empty remote name is rejected bare, so it cannot hide behind the prefix
as a delimiter-only registry name.

**Capability.** Every registered tool carries a capability classification, the input to any
permission layer. MCP annotations are **claims, not facts** — the MCP spec itself directs
clients not to make trust decisions on them. So:

> `κ(d) = ReadOnly` iff the host opted into trusting this server's annotations ∧ `d` carries an
> *explicit affirmative* read-only hint; otherwise `κ(d) = Mutating`.

Default distrust satisfies R3 by construction: absent opt-in, every wire tool is mutating, and
even with opt-in an unannotated or negatively-annotated tool stays mutating. Trust is per-server
and per-registration — an explicit host decision, never a default.

## 3. Registration

Registration walks the snapshot in server order and, for each tool: rejects an empty remote
name; forms the registry name (default prefix `ν`; hosts MAY choose verbatim names or a custom
prefix); rejects names violating the floor; rejects a name already present in the registry;
classifies capability per `κ`; and registers the result as a raw tool
whose spec — description and input schema — is the server's **verbatim** (a missing description
becomes an explicit "no description provided" placeholder, not an empty string). Dynamic tools
are first-wins: a built-in, host tool, or earlier MCP tool is never overwritten by later
wire-discovered input. Duplicate and cross-server collisions are reported as skips.

The return value is the full account (R5): the sequence of registry names registered, in server
order, and the sequence of skips, each carrying the remote name and the reason. Hosts SHOULD
surface skips to the operator; the kit MUST NOT drop a discovered tool without reporting it.

## 4. Calls

A call forwards the model's raw JSON arguments to the server's call endpoint. Client-side
validation is *shape only* — arguments must be a JSON object (or absent); everything beyond
that is the server's job against the schema it advertised (R2). Every failure mode is an error
tool result, never a panic and never a turn abort: transport errors, server-declared error
results, unexpected response types, timeouts, cancellation, calls after shutdown, non-object
arguments. The model sees a failed tool; the session continues.

- **Timeout.** Each call carries a per-call deadline (default five minutes, host-configurable).
  On expiry the call fails as error data and the server is sent a cancellation notification.
  An unbounded deadline is permitted but leaves the turn's cancel signal as the only escape
  from a server that accepts a call and never responds.
- **Cancellation.** The remote call races the run's cancel signal. On cancellation the kit
  sends the server a cancellation notification for the specific request — best-effort and
  time-bounded (a transport wedged mid-write MUST NOT hang the very cancellation that exists to
  escape it) — and returns an error result. A possibly-mutating call is told to stop, not
  silently abandoned.
- **Result rendering.** Before projection, the complete serialized `CallToolResult` envelope
  (including structured content, media payloads, and `_meta`) is capped. An accepted result is
  then flattened to the single text block a tool result is: text
  content passes through; text resources contribute their text tagged with their URI; binary,
  image, and audio content is *noted as omitted*, never dropped silently; an empty result falls
  back to the server's structured content, then to an explicit "no content" note. The rendered
  result is capped at 256 KiB — a ceiling this layer owns
  (network fetches, file reads); every built-in bounds its output the same way — truncated on a
  character boundary with a visible truncation note. Results live in the message
  history and are resent every remaining iteration; an unbounded response taxes the whole
  session, not one call.
- **Secret-safe diagnostics.** Connection, transport, server, and rendering failures are
  bounded before projection. Literal or resolved bearer/header values supplied for the
  connection are replaced with a fixed marker, including secrets split by truncation.

## 5. Lifecycle

- **Keepalive.** The connection stays alive while the host's handle *or any registered tool*
  exists — each tool holds the connection, so a registry never contains a dangling tool whose
  transport was dropped out from under it.
- **Death is observable.** A closed-ness probe reports the connection gone whether the host
  shut it down or the server died on its own (child crash, stdin EOF). Hosts poll it to drive
  banners or reconnection; the kit does not reconnect on its own.
- **Shutdown.** Shutting down cancels the connection. Registered tools remain in the registry;
  every subsequent call fails promptly as error data (R4). Transport cleanup — closing, and for
  child processes waiting out then killing the child — runs detached with bounded waits; a host
  that tears down its async runtime immediately after shutdown MAY leave a child that ignores
  stdin-EOF running, and SHOULD keep the runtime alive briefly if that matters.
- **Refresh rebuilds the registry.** The snapshot is point-in-time; the kit does not subscribe to
  list-changed notifications. Dynamic registration is non-destructive and therefore cannot
  express removal or replacement inside an existing registry. Hosts that refresh SHOULD rebuild
  the registry from fresh discovery.
- **Lazy dial (cached catalog).** A live connection MAY export its discovered tools as a
  serializable catalog, and a host MAY register that catalog with no connection at all,
  supplying a dial factory instead. Registration performs zero dials; the first call — by any
  tool of the batch — dials once and the batch memoizes the live connection. A failed dial is
  one failed tool result (R4), never a poisoned batch: the next call retries, and a memoized
  connection observed dead is re-dialed rather than kept as a corpse. Name floor, prefixing,
  capability distrust, and skip accounting are identical to live registration (I4–I6); the
  cached spec carries the server's read-only claim so the trust opt-in composes unchanged.
  A cached spec MAY additionally carry a host-chosen `registry_name`; dispatch still sends its
  raw `name` to the server.
- **Catalog identity is enforced at persistence.** `CachedToolSpec` describes tools, not the
  connection configuration that produced them. A persisted catalog MUST store the exact
  definition fingerprint beside each server and refuse snapshots that no longer match. The
  managed layer does this automatically. A custom control plane has the same obligation.
  Successful zero-tool enumeration still needs an identity record, or it is retried every boot.

### 5.1 Managed control plane

`ac_mcp::managed` is the standard application-agnostic assembly of these mechanics. It is
available with the `managed` feature (`managed` currently includes `http`) and exposes:

- `ManagedMcp<S: StateStore, C: CredentialStore>`, with a synchronous `open` for the stock file
  stores and an async constructor for injected stores;
- ordered `mcpServers` config, definition fingerprints, and a config-bound version-2 catalog;
- one `ConnectionPolicy` supplying a baseline stdio environment, an optional host-provided
  environment-value resolver, separate connect and tool-discovery timeouts, and stderr policy to
  probes, refresh, OAuth enumeration, and lazy dialers alike. The resolver supplies values for
  configured `envVars`, `envHeaders`, and `bearerTokenEnvVar` names at connection time; AC never
  consults ambient process state itself. Timed-out or cancelled discovery always shuts down its
  connection;
- config mutation, credential removal, catalog invalidation, refresh and offline-first backfill
  under a single mutation order;
- `pending`, `cached`, `failed`, and `needs-auth` server snapshots plus credential status;
- connection-free mounting from one explicit `CatalogSnapshot`, including a non-destructive
  stock `tool_search`, an exact gated-name set for `ConditionalToolsHook`, search-install
  accounting, and an optional host presentation transform; convenience methods load once and
  delegate to the snapshot API;
- explicit catalog requalification through the configured `CatalogNamePolicy`, rewriting current
  derived names without a server dial for hosts intentionally migrating a public name contract;
- a generation-bound `OAuthFlowStore` and high-level authentication that derives endpoint,
  enabled state, scope, and client credentials from one locked durable-definition snapshot.

Live enumeration is bounded independently of result rendering: at most 512 tools and 512 pages;
pagination cursors, names, descriptions, schemas, schema depth, each complete serialized tool
definition (including optional title, output schema, icons, execution/annotation data, and
`_meta`), and aggregate retained catalog bytes each have explicit ceilings. Repeated cursors and
a continuation after the tool ceiling are protocol failures. These checks apply as pages are
decoded and prevent unbounded retained catalog state; they do not repair the SDK transport
caveat in §9.

The stock `FileStateStore` and `FileCredentialStore` keep read-only boot/status reads tolerant,
but mutations use strict read-for-update and refuse to replace unreadable, malformed, or
wrong-shaped files. Config mutations likewise refuse a partially rejected registry. Writes use
same-volume atomic replacement; final files are mode `0600` and the fixed temporary directory is
mode `0700` on Unix. Windows receives atomic replacement but this layer does not install an
owner-only ACL. `ManagedPaths::control_paths` returns the final files plus deduplicated private
temporary directories so an embedding can deny all of them to agent tools.

Credentials are bound to the exact full server-definition fingerprint, not only server name and
URL. Changed definitions invalidate flow generations before mutation, and authentication captures
the definition plus flow generation under the same config lock as remove/upsert. URL-only
unscoped rows are never claimed by bearer reads or candidate probes; a host intentionally
migrating such data must opt in via `claim_unscoped_credentials` (or the stock synchronous
startup variant).

Both connection and provider-visible names preserve already-valid inputs. Whenever normalization
changes an input (or catalog truncation is required), the managed layer appends a deterministic
digest suffix. Distinct portable config keys or remote tool names therefore do not silently
collapse merely because punctuation or delimiter repair produced the same readable prefix.
`CatalogNamePolicy` lets an embedding preserve an established deterministic public-name contract
for future enumerations. Existing catalog names remain verbatim by default; the only exception is
an embedding's explicit `requalify_catalog` migration.

`ManagedMcp` deliberately has no application defaults. It does not choose a home directory,
inherit ambient environment variables, discover another application's files, pick an OAuth
callback port/path, supply product identity or browser copy, emit RPC payloads, or decide how a
catalog description appears in a search UI.

## 6. Invariants

- **I1 (uniform dispatch).** After registration, no run-loop behavior distinguishes a wire tool
  from a built-in: same registry, same dispatch-by-name, same result feedback, same events.
- **I2 (verbatim spec).** Description and input schema reach the model exactly as the server
  declared them; the kit derives nothing and validates only object-shape.
- **I3 (failure is data).** No MCP condition — transport, protocol, timeout, shutdown, lies —
  produces anything other than a model-visible error tool result.
- **I4 (default distrust).** With annotations untrusted, no registered wire tool classifies as
  read-only; with trust opted in, only an explicit affirmative claim upgrades.
- **I5 (name safety).** Every registered name matches `^[A-Za-z0-9_-]{1,64}$`, and every
  default-prefixed name decomposes uniquely into (server, tool).
- **I6 (total accounting).** registered ⊎ skipped = discovered, and every skip carries its
  remote name and reason.
- **I6a (non-destructive dynamic tools).** A wire-discovered tool never replaces an existing
  registry entry; the first entry wins and every later collision is reported as skipped.
- **I7 (bounded results).** No accepted decoded result has a complete serialized envelope above
  its protocol cap; no rendered result exceeds its text cap plus a bounded truncation note, cut
  on a character boundary.
- **I7a (bounded catalogs).** No decoded discovery snapshot retained by AC exceeds the
  configured page/tool/entry/schema/depth/aggregate ceilings.
- **I8 (prompt failure after death).** A call on a shut-down or dead connection fails promptly
  — it never hangs the turn.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| Handshake, discovery, registration, name floor, prefix decomposition | kit |
| Portable server types, config fingerprint, catalog identity/invalidation | managed kit |
| Transport translation, probe, lazy dialers, refresh/backfill, mutation ordering | managed kit |
| Cached registration, stock `tool_search`, gated-name accounting | managed kit |
| Config/catalog/credential persistence mechanism | injected stores; stock file stores ship in kit |
| Storage locations and sandbox deny-read wiring | host |
| Baseline stdio environment, symbolic value resolver, timeouts, stderr mode, trust opt-in | host policy consumed by managed kit |
| Applying `envVars`, `cwd`, `envHeaders`, and `bearerTokenEnvVar` on every connection path | managed kit |
| Argument validation against the advertised schema | server |
| Result flattening, size cap, cancellation notification | kit |
| Permission decisions over capability ([ac-approvals.md](ac-approvals.md)) | host |
| When to trigger explicit refresh; surfacing status/skips | host |
| Import sources from other applications | host (§8) |
| OAuth metadata discovery, DCR, PKCE, authorization URL, and code exchange | kit (`http`) |
| OAuth loopback routing, CSRF state dispatch, timeout, and cancellation | kit (`http`) |
| OAuth stored-token probe, per-server single-flight, callback lease, cleanup, and re-enumeration sequencing | kit (`http`) |
| OAuth client branding, resolved callback URI, page copy, browser launch | host |
| RPC/UI result mapping and description/search presentation | host |

## 8. Server configuration format

The low-level API takes a **connection**, not a file. The managed API accepts a store and ships
the de-facto portable definition because a user should be able to move the same server block
between tools unchanged. MCP standardizes the wire, not this file; interoperability comes from
following the ecosystem's converged shape rather than inventing an application table.

- **The de-facto shape.** `managed::Config` reads and writes the `mcpServers` JSON object: a map
  from server name to either a stdio definition
  `{ "command": string, "args"?: string[], "env"?: { [k]: string }, "envVars"?: string[], "cwd"?: string }`
  or a remote definition
  `{ "url": string, "headers"?: { [k]: string }, "envHeaders"?: { [header]: string }, "bearerTokenEnvVar"?: string, "oauth"?: false | object }`.
  Strict per-entry
  parsing reports malformed definitions while valid siblings remain readable; tolerant reads
  never prevent boot, while mutations refuse any rejected registry rather than wiping it.
  `ServerConfig::parse_value` is the shared host-ingress parser: it enforces the
  `command`-XOR-`url` transport choice before decoding the selected concrete
  shape and returns stable typed errors instead of serde's untagged-enum
  fallback. Authored map order survives read/mutate/write.
- **Literal versus symbolic values.** `env` and `headers` contain literal durable values; an
  embedding must treat its config store accordingly. `envVars` contains host-environment names
  copied into a stdio child at dial time. `envHeaders` maps each HTTP header name to a
  host-environment name, and `bearerTokenEnvVar` names the source of the HTTP bearer token. Only
  the names are serialized for those symbolic fields. Their values come from
  `ConnectionPolicy::with_environment_value_resolver`; a missing resolver or unresolved name
  contributes no value. AC never calls `std::env` on its own.
- **Not a bespoke application table.** Embedding the same fields inside a host's own application
  config (a TOML `[mcp_servers.…]` table, say) is a valid host choice but a worse default: it
  couples the definition to one tool's config syntax, home directory, and surrounding keys, and
  loses the paste-portability the JSON shape exists for. A host that keeps a broader config file
  SHOULD still accept the standalone `mcpServers` JSON alongside it.
- **Other tools' configs are importers, not the contract.** A host MAY read another tool's native
  config — a JSON `mcpServers` file, or a foreign application config carrying an equivalent table
  — and fold definitions into its own store as a one-way convenience. Import locations and
  enabled sources are host policy over untrusted input; unmodeled keys are dropped. The managed
  runtime sees only validated `ServerConfig` values.
- **Transport reach.** The stdio definition maps onto the child-process connect path. AC clears
  the child's inherited environment, applies the policy's baseline, then configured symbolic
  `envVars`, then literal `env` (last writer wins), and applies `cwd` when present. A remote `url`
  maps onto the Streamable HTTP connect path (opt-in `http` feature). Literal `headers` are
  extended or overridden by resolved `envHeaders`; a resolved `bearerTokenEnvVar` takes precedence
  over a stored OAuth bearer. A host built without the feature MUST still report a remote
  definition as skipped with a stated reason (R5) — never silently.
- **Remote URL floor.** A remote definition must use HTTPS, except that HTTP is permitted for
  a literal loopback IP. Userinfo is rejected. Validation occurs before constructing the
  transport or handing a bearer/header value to it; DNS names such as `localhost` do not count
  as literal loopback.
- **One translation path.** `managed::ConnectionPolicy` and `managed::connect` are reused for
  connectivity tests, live catalog export, authentication re-enumeration, and cached lazy dialers.
  Hosts using the managed layer MUST inject the intended baseline environment, symbolic-value
  resolver, and deadlines once rather than reconstructing transports per operation.

## 9. Deferred

- **Pre-deserialization transport byte ceilings.** `rmcp` 2.2.0's stock stdio transport reads
  a newline-delimited frame into an unbounded `Vec`, and its stock Streamable HTTP client lets
  reqwest/SSE assemble an unbounded response or event before yielding a decoded JSON-RPC
  message. AC's catalog, result, error, and OAuth-body caps run after or outside that framing
  boundary, so a malicious peer can still force a large transient allocation first. Closing
  this requires an upstream bounded-transport seam or AC-owned stdio and HTTP transports; it
  must not be misrepresented as solved by the retained-state caps.
- **Resources, prompts, sampling** — the non-tool MCP primitives. Tools are the seam the run
  loop needs; the rest is host surface until evidence says otherwise.
- **List-changed notifications** — snapshot-plus-host-driven-refresh is the contract today;
  reactive re-registration needs a story for removal (§5) first.
- **OAuth refresh grants** — the `http` feature owns the authorization-code/PKCE protocol,
  loopback callback, and end-to-end interactive coordinator. Token refresh remains deferred.
  AC ships semantic credential traits and a stock JSON store with Unix `0600` modes and
  cross-platform atomic replacement; custom persistence and browser integration remain injected
  seams. Product UI is never part of the core.
- **Cross-process compare-and-swap** — one manager instance serializes read-modify-write
  operations and rejects malformed inputs, but independent processes targeting the same files
  still require a host-selected lock/CAS strategy.

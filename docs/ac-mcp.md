# RFC: MCP integration — wire-discovered tools in the same registry

**Status:** implemented — specification of record (2026-07-21). **Amended 2026-07-23:** §8
(server configuration format) records the de-facto `mcpServers` JSON object as the host contract
to follow — the kit stays format-agnostic (it takes connections, not files), and other tools'
native configs are one-way importers, never the contract.
**Requires:** [ac-tools.md](ac-tools.md) (the tool registry, the raw (runtime-described) registration
path, errors-as-data), [ac-provider.md](ac-provider.md) (tool specs ride every sampling request —
the exposure that motivates the name floor defined in §2). **Required by:** nothing yet. **Interacts with:**
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
- **R4 (session survival).** No server behavior — crash, hang, garbage, oversized output —
  may cost more than one failed tool result. Failures are data the model sees, never a poisoned
  session or a terminated turn.
- **R5 (total accounting).** Discovery-to-registration MUST be fully reported: every discovered
  tool is either registered or skipped with a stated reason. Nothing is dropped silently.

## 2. Model

A **connection** binds one client to one MCP server over a host-chosen transport (the covered
common case is a child process on stdio) under a host-chosen **server name** `s`. Discovery
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
prefix, accepting collisions-replace semantics as their own decision); rejects names violating
the floor; classifies capability per `κ`; and registers the result as a raw tool
whose spec — description and input schema — is the server's **verbatim** (a missing description
becomes an explicit "no description provided" placeholder, not an empty string). Within one
server, a duplicated tool name replaces the earlier entry — the same last-write-wins semantics
as every other registration path.

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
- **Result rendering.** A result is flattened to the single text block a tool result is: text
  content passes through; text resources contribute their text tagged with their URI; binary,
  image, and audio content is *noted as omitted*, never dropped silently; an empty result falls
  back to the server's structured content, then to an explicit "no content" note. The rendered
  result is capped at 256 KiB — a ceiling this layer owns
  (network fetches, file reads); every built-in bounds its output the same way — truncated on a
  character boundary with a visible truncation note. Results live in the message
  history and are resent every remaining iteration; an unbounded response taxes the whole
  session, not one call.

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
- **Refresh is re-registration.** The snapshot is point-in-time; the kit does not subscribe to
  list-changed notifications. Re-running registration replaces same-name entries and adds new
  ones, but replacement cannot express *removal* — a tool the server dropped stays registered
  and fails at call time as error data. Hosts that refresh SHOULD therefore rebuild the
  registry from fresh discovery rather than mutate one in place.

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
- **I7 (bounded results).** No rendered result exceeds the cap plus a bounded truncation note,
  cut on a character boundary.
- **I8 (prompt failure after death).** A call on a shut-down or dead connection fails promptly
  — it never hangs the turn.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| Handshake, discovery, registration, name floor, prefix decomposition | kit |
| Transport choice, server naming, prefix mode, timeout, trust opt-in | host |
| Argument validation against the advertised schema | server |
| Result flattening, size cap, cancellation notification | kit |
| Permission decisions over capability ([ac-approvals.md](ac-approvals.md)) | host |
| Refresh policy — when to re-discover, rebuild vs. mutate | host |
| Surfacing skips and transport death to the operator | host |
| Server-definition config format; importers from other tools | host (§8) |

## 8. Server configuration format

The kit takes a **connection**, not a file: a host builds each connection from a name and a
transport (§2) and never hands the kit a config path or document. Where server definitions come
from is therefore host policy — but the choice is not free, because a portable definition is one
a user can move between tools unchanged. MCP the protocol standardizes the wire, not the config;
the standard to follow is the *de-facto* one the ecosystem converged on, not any single tool's.

- **The de-facto shape.** A host SHOULD read and write server definitions as the `mcpServers`
  JSON object: a map from server name to either a stdio definition
  `{ "command": string, "args"?: string[], "env"?: { [k]: string } }` or a remote definition
  `{ "url": string, "headers"?: { [k]: string } }` (a host MAY tag the transport with a `type`
  discriminant). This is the shape desktop MCP hosts, editors, and coding agents already emit, so
  a user can paste a server block from any of them; adopting it is the difference between a config
  a user already has and one they must translate.
- **Not a bespoke application table.** Embedding the same fields inside a host's own application
  config (a TOML `[mcp_servers.…]` table, say) is a valid host choice but a worse default: it
  couples the definition to one tool's config syntax, home directory, and surrounding keys, and
  loses the paste-portability the JSON shape exists for. A host that keeps a broader config file
  SHOULD still accept the standalone `mcpServers` JSON alongside it.
- **Other tools' configs are importers, not the contract.** A host MAY read another tool's native
  config — a JSON `mcpServers` file, or a foreign application config carrying an equivalent table
  — and fold the definitions into its own store as a one-way convenience. Such an import is host
  policy over untrusted input: names are re-validated against the floor (§2) and unmodeled keys
  are dropped, never adopted. The kit sees only the resulting connections.
- **Transport reach.** The stdio definition maps onto the child-process connect path; the remote
  `url` definition maps onto the transport-generic connect seam but rides streamable HTTP,
  deferred (§9). Until that lands a host honors the stdio form and reports a remote definition as
  skipped with a stated reason (R5) — never silently.

## 9. Deferred

- **Resources, prompts, sampling** — the non-tool MCP primitives. Tools are the seam the run
  loop needs; the rest is host surface until evidence says otherwise.
- **List-changed notifications** — snapshot-plus-host-driven-refresh is the contract today;
  reactive re-registration needs a story for removal (§5) first.
- **Remote transports and auth** — the connect seam is transport-generic already, but the
  covered, tested path is child-process stdio; streamable HTTP and OAuth are future work.

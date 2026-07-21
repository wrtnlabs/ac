# RFC: The approval model — pre-flight intent classification

**Status:** design of record — accepted, not yet implemented (2026-07-21).
**Requires:** [ac-tools.md](ac-tools.md) (the capability axis of the tool contract).
**Interacts with:** [ac-sandbox.md](ac-sandbox.md) (kernel containment, the layer beneath this
one), [ac-mcp.md](ac-mcp.md) (capability of wire-registered tools),
[ac-security.md](ac-security.md) (threat model).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A host that lets an agent run commands owes its user one question, asked neither always nor
never: *this* command, now — yes or no? The kit already has two enforcement layers, and neither
can ask it.

**Capability classification** (live) is tool-level and static: every tool declares itself
read-only or mutating as part of the tool contract — an unclassified tool cannot exist — and a
tool that arrives over a wire registers as mutating unless the host explicitly opts into
trusting the server's read-only claim. This answers *which tools could change the world*, and
for a dedicated tool it is the whole answer. For a shell tool it answers nothing per call:
any useful shell is mutating, so a capability gate prompts on every command or on none.

**Kernel containment** ([ac-sandbox.md](ac-sandbox.md)) bounds what a spawned command can do —
the blast radius. It is intent-blind by construction: the permitted region must include
everything the agent's legitimate work touches, and harmful commands exist whose effects lie
entirely inside it. A recursive delete of the working tree is within any write-set that permits
the agent to write the working tree.

The gap between them is the **approval question**, and it has its own requirements:

- **R1 (per-command granularity).** The unit of approval MUST be the command, not the tool.
  Two calls of the same tool MUST be able to receive different verdicts.
- **R2 (pre-flight).** The verdict MUST be rendered before any process exists — on stated
  intent, not observed effects. Post-hoc detection is not approval.
- **R3 (host-owned trust).** Which programs are trusted, with which arguments, is a judgment
  about a deployment, not about agents in general. The kit MUST ship the engine and the verdict
  taxonomy and MUST NOT ship a policy.
- **R4 (no silent allow).** No failure of parsing, matching, or validation may yield the
  permissive verdict. Uncertainty escalates.

> **Theorem (necessity of the middle layer).** No approval discipline built from capability
> classification and containment alone is simultaneously safe and usable for a shell tool.
> *Proof sketch:* capability is constant over the tool's calls, so a capability gate violates
> R1 — it degenerates to "always prompt" (unusable) or "never prompt" (unsafe). Containment is
> a predicate on effect *regions*, and the harmful and the harmless coexist inside the
> permitted region, so no strengthening of the region distinguishes them; a sandbox refined
> until it ranks effects by meaning would have to read the command — which *is* the middle
> layer. Hence any discipline satisfying R1 over a single mutating tool must classify the
> command itself. ∎

The consequence: approval UX is not obtained by tightening either existing layer. It is a third
layer — **intent classification** — sitting strictly between them: finer than the tool, earlier
than the kernel.

## 2. Model

**Commands.** A command is a pair `c = (p, ⟨a₁ … aₙ⟩)` — a program name and an argument
vector. A submission to the shell tool is first **lowered**: a shell-wrapper invocation
(`sh -c …` and kin) is parsed into its constituent simple commands `⟨c₁ … cₖ⟩` (pipeline and
list segments each their own `cᵢ`); a compound that cannot be confidently parsed lowers to a
single **unknown** command. A command MUST NOT be classified as its wrapper — `sh -c "rm -rf ."`
is about `rm`, not `sh`.

**Policy.** A policy is a partial function `P : programs ⇀ rule sets`, host-supplied (R3). A
rule types an argument vector into **semantic roles**: literal tokens, flags, options carrying
typed values, positions naming *readable* paths, positions naming *writable* paths, opaque
non-path values, and explicitly unverified remainders. A rule matches `c` only if the entire
vector is consumed by the typed pattern — an unrecognized flag or an unaccounted position is a
non-match, not a partial match (R4). Each rule carries a verdict and MAY carry a justification
(surfaced in prompts and refusals) and example invocations, positive and negative, validated
when the policy loads: a policy that fails its own examples MUST be rejected at load.

**Verdicts.** Verdicts form a totally ordered lattice

> `safe ⊏ prompt ⊏ forbidden`

read as: run without asking ⊏ ask the user ⊏ refuse without asking. Aggregation is the join:

```
verdict(c)      = ⊔ { v(r) : r ∈ P(p), r matches c }     if P is defined at p and some r matches
                = U                                       otherwise (unknown)
verdict(⟨c₁…cₖ⟩) = ⊔ᵢ verdict(cᵢ)
```

A compound is as suspicious as its most suspicious segment; overlapping rules resolve to the
strictest. `U`, the **unknown default**, is a host parameter with kit-defined bounds: `U =
prompt` by default; a host MAY set `U = safe` only while kernel containment is *strict* — the
sandbox absorbs the risk of the unclassified — and MUST NOT while containment is degraded or
off. Strong containment buys tolerance of the *unknown*; it never lowers the verdict of a
command a rule explicitly matched.

**Role containment.** Matching binds role-typed positions to concrete paths, and the bindings
are checked against the same path policy that contains the built-in tools: readable roles must
resolve within read containment, writable roles within write containment. A binding that fails
raises that match's verdict to at least `prompt` (R4). This is the point of roles: `cp a b` is
not one trusted string but a readable `a` and a writable `b`, each judged by where it lands.

**Permission modes.** A mode is a floor `φ : Capability → Verdict` applied per tool call:
a read-only mode is `φ(read-only) = safe, φ(mutating) = prompt` (or `forbidden`, host's
choice); an unrestricted mode floors both at `safe`. For dedicated tools the floor is the
verdict. For the shell tool the intent verdict *replaces* the floor — it may fall below it
(a command whose matched roles are all read-only auto-approves through a mutating tool: the
recovered granularity is the payoff) and may rise above it (`forbidden` refuses outright).
The soundness of falling below the floor is exactly the role check above: the verdict is
grounded in what the command binds, not the tool's worst case.

## 3. Mechanics

Classification runs entirely between the model's tool call and the spawn: lower, match,
validate roles, join, act. `safe` proceeds; `prompt` suspends the call and emits an approval
request on the event stream carrying the command, the matched justifications, and the verdict's
provenance (which rules, which segments, whether the unknown default applied); `forbidden`
returns a refusal to the model as tool output — data, not a crash — including any justification
so the model can route around the refusal legitimately. In a host with no approval channel,
`prompt` MUST resolve to `forbidden`, never to `safe`.

An approval MAY carry **generalization**: "don't ask again for this prefix," appended to the
host's policy as an allow rule for the exact matched prefix. The kit supplies the append
mechanics and one guard: prefixes that name an interpreter or wrapper escape (`sh -c`,
`python -c`, `env`, and kin) are unrulable as allow — such a rule would allow everything, so
the engine MUST refuse to create it. Persistence and scope of amendments are host territory.

A verdict decides prompting and nothing else. `safe` does not widen the path policy, does not
relax the sandbox, does not skip capability accounting — the layers compose by conjunction,
never by substitution.

## 4. Invariants

- **I1 (pre-flight).** The verdict is a pure function of the lowered command, the policy, the
  path policy, and the mode — computed before spawn. A `forbidden` command spawns nothing.
- **I2 (join monotonicity).** For a fixed policy, an additional matched rule, an additional
  segment, or a failed role binding never lowers an aggregate verdict. Policy amendment (§3)
  is the only path by which a verdict may fall between evaluations, and it lowers only `U` —
  an explicit rule-matched verdict is never lowered by generalization. The permissive verdict
  is reachable only by explicit rule match or by the unknown default under strict containment.
- **I3 (no wrapper identity).** No command is classified by its wrapper; lowering precedes
  matching, and an unloweable compound is unknown, not its first word.
- **I4 (fail-toward-prompt).** Every classification failure — parse, match, validation, policy
  load — yields a verdict `⊒ prompt`.
- **I5 (non-substitution).** Intent classification gates approval only. In-process path
  containment and kernel containment hold unconditionally, for every verdict, per
  [ac-sandbox.md](ac-sandbox.md)'s two-layer rule.
- **I6 (capability floor).** A tool without intent classification is gated at `φ(capability)`;
  a read-only-capability call is never prompted by the mode.

## 5. Division of responsibility

| Concern | Owner |
| --- | --- |
| Verdict taxonomy, lattice, join; lowering; role typing and matching | kit |
| Role-containment check (delegating to the path policy) | kit |
| Policy content: rules, justifications, examples, trusted programs | host |
| Unknown default `U` (within kit bounds); permission-mode floors | host |
| Approval UX; amendment persistence and scope | host |
| Blast radius under any verdict | sandbox ([ac-sandbox.md](ac-sandbox.md)) |

## 6. Deferred

- **Network-intent roles** — typing hosts and protocols in argv the way paths are typed, to
  feed the egress-allowlist phase of [ac-sandbox.md](ac-sandbox.md). Deferred with it.
- **Containment escalation on explicit allow** — running a rule-allowed command outside the
  sandbox (the reference runtime supports this for commands that need what the sandbox denies).
  Deliberately excluded from v1: it breaches I5, so it must arrive, if ever, as its own
  reviewed exception.
- **A built-in danger heuristic set** — recognizing notorious argv shapes without a policy.
  Useful, but it is shipped judgment, in tension with R3; adopt only with evidence that hosts
  refuse to write the equivalent rules.
- **Non-POSIX lowering** (PowerShell-flavored wrappers) — mechanical extension of §2's
  lowering, when a consumer needs it.

---
*Provenance: this design distills the exec-policy system of a production agent runtime
(openai/codex, Apache-2.0), studied 2026-07-21 — both its role-typed policy language and its
prefix-rule successor's verdict lattice and approval wiring. The distillation is behavioral —
no code was carried over.*

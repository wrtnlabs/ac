# RFC: The security model

**Status:** doctrine — in force (2026-07-21).
**Aggregates:** [ac-tools.md](ac-tools.md) (in-process containment), [ac-sandbox.md](ac-sandbox.md)
(kernel containment), [ac-approvals.md](ac-approvals.md) (intent classification, designed),
[ac-mcp.md](ac-mcp.md) (third-party servers), [ac-skills.md](ac-skills.md) (content trust),
[ac-fork.md](ac-fork.md) (record integrity).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The kit's job is to let a language model act on a user's machine: read and write files, run
arbitrary programs, reach the network. Every one of those verbs is the attack surface. This
document states the threat model once, assigns each boundary to the specification that owns
it, and fixes the two rules that govern all of them. It contains no mechanisms of its own; it
exists so that no boundary is defined only implicitly.

## 2. Principals and trust

| Principal | Trust | Consequence |
| --- | --- | --- |
| The host application | trusted | supplies policy, prompts, roots; the kit never second-guesses it |
| The user's direct input | trusted as intent | it authorizes; it is not sanitized |
| The model's output | **untrusted instructions** | every proposed action passes containment; nothing executes on the model's authority alone |
| Content the model reads (files, tool results, web) | **untrusted data** | it can steer the model (injection); it must never *widen* what the model can do |
| Third-party tool servers | untrusted, including their self-descriptions | server claims are hints, never grants ([ac-mcp.md](ac-mcp.md)) |
| Skill layer roots | trusted **as prompts**, by explicit host designation | pointing a layer at unvetted content is granting it prompt authority ([ac-skills.md](ac-skills.md)) |

The asymmetry in rows three and four is the core of the model: the model may be *persuaded* by
what it reads — the kit cannot prevent that — but persuasion must never translate into
capability the policy did not already grant. Injection resistance is therefore a *containment*
property, not a filtering property: the kit does not try to detect hostile text; it bounds what
any text can cause.

## 3. The layered containment theorem

Three layers, each answering a different question about a model-proposed action:

1. **Capability & path containment** (in-process, [ac-tools.md](ac-tools.md)): *may this tool
   touch this location?* Judged before any effect, symlink-safe, with writes confined to
   host-granted trees and reads widened only by explicit grants.
2. **Intent classification** ([ac-approvals.md](ac-approvals.md), designed): *is this specific
   command benign, approval-worthy, or forbidden?* Judged before spawn, from the command's
   parsed semantics against host-supplied policy.
3. **Kernel containment** ([ac-sandbox.md](ac-sandbox.md)): *whatever it claimed, what can the
   spawned process actually do?* Enforced by the operating system: filesystem scope, syscall
   surface, resource ceilings, network reachability.

> **T1 (independence).** The layers MUST hold independently: a defect in any one leaves the
> others intact, because they are enforced by different machinery at different times against
> different representations of the action. In-process checks are real for the kit's own code
> and *pretend for a spawned child* — which is why layer 3 exists; kernel enforcement can be
> degraded by platform — which is why layer 1 never relaxes.

> **T2 (no widening at runtime).** No event at runtime — a skill loading, a server connecting,
> a model requesting — may widen any layer's policy. Widening is a host decision made at
> assembly time. (This is the settled negative result on per-skill and per-server permissions:
> declared needs are requests to the *host*, never grants to the *kit*.)

## 4. The two governing rules

- **G1 (never pretend).** A protection that is not actually enforced MUST NOT be presented as
  a protection. Where a mechanism cannot enforce on a platform, the system runs loudly
  degraded or refuses — an advisory check dressed as a sandbox invites the exact reliance it
  cannot support. Enforcement claims are proved by attempted violation
  ([ac-testing.md](ac-testing.md) R4).
- **G2 (fail closed, degrade loudly).** Where policy cannot be evaluated, the action is
  refused. Where a platform cannot fully enforce, the mode is surfaced to host and user on
  every affected action, and choosing to proceed degraded is a host decision, never a silent
  default.

## 5. The boundary register

Every security-relevant boundary, and the specification that owns its mechanics:

| Boundary | Rule | Owner |
| --- | --- | --- |
| Writes | confined to host-granted trees; **never** widened by reads, grants, skills, or model requests | [ac-tools.md](ac-tools.md) |
| Reads | confined to granted trees; widened only by explicit, canonical-path grants | [ac-tools.md](ac-tools.md) |
| Symlinks | resolved before judgment at every layer; a link is never a door | [ac-tools.md](ac-tools.md) |
| Secrets (key material, credential stores) | denied to spawned processes regardless of other grants | [ac-sandbox.md](ac-sandbox.md) |
| Network egress | binary on/off, kernel-enforced; a *domain* allowlist is honest only as kernel-block-plus-proxy and is deferred until buildable that way | [ac-sandbox.md](ac-sandbox.md) |
| Command intent | classified pre-spawn against host policy; unknown commands never default to safe | [ac-approvals.md](ac-approvals.md) |
| Third-party tool names & schemas | validated to the strictest provider contract at registration; one hostile name must not poison the session | [ac-mcp.md](ac-mcp.md) |
| Third-party capability claims | read-only hints honored only by explicit host opt-in | [ac-mcp.md](ac-mcp.md) |
| Injected context | machine-identifiable, so synthetic text is never mistaken for user intent | [ac-context.md](ac-context.md) |
| The session record | append-only; cuts and rewinds are marked, honest, and non-destructive | [ac-fork.md](ac-fork.md) |
| Serving transports | listening sockets verify origin; the session store lives outside every tool-reachable tree | [ac-serving.md](ac-serving.md) |
| Model-visible errors | are data; infrastructure detail (paths, internals) SHOULD NOT leak into them beyond what the model needs | [ac-loop.md](ac-loop.md) |

## 6. Residual risk, stated plainly

What the model within policy can still do, the kit does not prevent: exfiltrate what it can
legitimately read through any egress the host allowed; run any command the policy and sandbox
permit, including expensive or destructive-within-bounds ones; be persuaded by content into
*choosing* badly among permitted actions. These are host-policy and approval-UX territory —
the kit's obligation is that the *bounds* hold and the *record* of what happened is complete.
A host wanting stronger guarantees narrows the grants; it does not get them from denial.

## 7. Deferred

- Kernel-enforced egress allowlisting (the block-plus-proxy design) — [ac-sandbox.md](ac-sandbox.md) v2.
- Audit-grade provenance on the session record (signing, tamper evidence) — no requirement yet.
- Windows kernel containment — honestly absent; the platform runs loudly unenforced (G2).

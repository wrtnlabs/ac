# RFC: The proof doctrine

**Status:** doctrine — in force (2026-07-21).
**Interacts with:** every specification in this directory; the one-rule section of [ac-sandbox.md](ac-sandbox.md) (the actual-vs-pretend rule this doctrine generalizes).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A kit whose claims are *containment holds*, *input is never lost*, *the sandbox enforces* cannot
measure itself with coverage numbers. Its failure mode is specific: **pretend verification** —
tests that assert the code does what the code does, mechanisms whose presence is mistaken for
their enforcement, proofs of the data path that never render the artifact. Each of these has
produced a real, caught defect in this codebase's history: an enforcement layer that reported
strict while enforcing nothing; a parser whose two shipped tests both passed by coincidence of
input shape; a rendering bug invisible to every test that stopped short of rendering. The
doctrine below is the distillation of what caught them.

Requirements on any proof offered for a claim:

- **R1 (ground truth over echo).** A proof MUST assert on *world effects* — the file on disk,
  the byte the model was actually shown, the connection that was actually refused — never
  solely on the system's own report of what it did.
- **R2 (negative-path parity).** Every containment or refusal claim MUST have a violation
  test: the escape attempted and observed to fail, with the absence of the forbidden effect
  asserted, not just the presence of an error message.
- **R3 (shipped-path assembly).** End-to-end proofs MUST assemble the system through the same
  wiring path the shipped binary uses. Two wiring paths — one for tests, one for production —
  drift, and the tests then prove the wrong system.
- **R4 (enforcement is probed, never presumed).** A claim that a mechanism *enforces* MUST be
  verified by attempting a real violation under that mechanism. Availability of an interface
  is not enforcement; a mechanism can accept configuration and enforce nothing.

## 2. The proof classes

Five classes, ordered by cost; a change SHOULD carry proofs from the cheapest class that can
falsify it, and MUST NOT substitute a cheaper class where only a costlier one can.

**P1 — Hermetic unit proofs.** Pure-logic properties: parsers, policy resolution, algebraic
laws of combinators. Deterministic, no I/O beyond a temporary directory. The workhorse; also
the least able to catch integration and behavioral defects — knowing which claims it *cannot*
falsify is part of the discipline.

**P2 — Hermetic end-to-end proofs.** A *scripted provider* — a deterministic stand-in that
replays a planned sequence of model responses and records every request it receives — drives
the real loop, the real tools, the real containment, against a real temporary filesystem. What
makes this class strong is what is real: assertions run against disk ground truth (R1),
against the recorded requests (what the model was actually shown — history feedback, tool
filtering, injected context), and against the event stream a client would consume. The
scripted provider makes model *behavior* an input, so these proofs falsify everything except
model behavior itself.

**P3 — Live proofs.** Opt-in, never in continuous integration, against a real provider and
model. They exist for exactly the failure classes P2 cannot reach: the wire protocol as
actually spoken (a hand-rolled streaming parser has no truth but the live stream), and model
behavior under the kit's real prompts, tool descriptions, and injected context — does the
model *actually* follow the catalog to the file, obey the forced choice, use the injected
skill. A live proof asserts the same ground truth as P2 (disk, history) plus the qualitative
outcome. Cost discipline: smallest sufficient model, smallest sufficient task, never a secret
in a transcript.

**P4 — Kernel proofs.** For enforcement claims (R4): spawn under the real mechanism and
attempt the violation — write outside the tree, read the secret, open the socket, exceed the
limit — asserting both the failure of the attempt and the absence of the effect. Platform
truth requires platform execution: an enforcement layer verified only on the platform where it
happens to be active is unverified elsewhere; run the probe on a system where the mechanism
may be *present but inactive*, because that is precisely the configuration that turns a
presence-check into a fail-open (R4's motivating incident).

**P5 — Adversarial verification.** For findings, designs, and hand-written analyses: an
independent verifier is instructed to *refute* the claim — by re-reading, and where cheap by
executing a reproduction. A claim survives only if refutation fails. This class exists because
authorship is a bias: the author's tests encode the author's model of the input space, and the
two parser tests that "passed by luck" were exactly that model's blind spot. Findings verified
by execution outrank findings verified by inspection.

## 3. Doctrine

- **D1.** A gate, once green, is never weakened to stay green. A failing proof is information
  about the system or about the proof; deleting or loosening it without diagnosis destroys the
  information and MUST NOT happen.
- **D2.** Flaky is broken. A nondeterministic proof proves nothing and trains contributors to
  ignore red; fix it or move the claim to the class that can actually carry it.
- **D3.** Hermetic classes MUST NOT touch the network; the live class MUST NOT run unattended.
  The boundary is what makes the hermetic gate trustworthy and the live class honest about its
  cost.
- **D4.** Proof obligations are part of a design, not an afterthought: a specification in this
  directory that states an invariant SHOULD name the class of proof that checks it, and an
  implementation that lands without that proof has not landed the specification.
- **D5.** When a defect escapes a class, the fix carries a regression proof *in the class that
  should have caught it* — and if no class could have, that is a finding about the doctrine,
  recorded by amending this document.

## 4. Division of responsibility

| Concern | Owner |
| --- | --- |
| P1/P2 proofs for every kit claim; P4 for every enforcement claim | kit |
| The scripted-provider substrate and its request-recording contract | kit |
| P3 live proofs of kit prompts and wire crates | kit (opt-in harness) |
| Proofs of host wiring, host policy choices, host UX | host |
| P5 adversarial verification of nontrivial changes and studies | whoever lands the change |

## 5. Deferred

- Property-based and fuzz testing for the parser-shaped surfaces (frontmatter, mentions, wire
  events) — the natural extension of P1 once the surfaces stabilize.
- A machine-checked link between stated invariants (I-numbers in these documents) and the
  proofs that cover them — worth building when the document set stops moving.

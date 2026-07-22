//! The approval model — pre-flight intent classification ([docs/ac-approvals.md]).
//!
//! A host that lets an agent run commands owes its user one question, asked
//! neither always nor never: *this* command, now — yes or no? The kit already
//! has two enforcement layers and neither can ask it. **Capability
//! classification** (`ac_tool::Capability`) is tool-level and static — for a
//! shell tool it prompts on every command or on none. **Kernel containment**
//! (`ac-sandbox`) bounds the blast radius but is intent-blind: a recursive
//! delete of the working tree lies entirely inside any write-set that permits
//! writing the working tree. The gap between them is the approval question, and
//! it needs a third layer — **intent classification** — finer than the tool,
//! earlier than the kernel.
//!
//! This crate is that layer's kit-owned machinery, and nothing else:
//!
//! - **The verdict lattice** `safe ⊏ prompt ⊏ forbidden` and its join
//!   ([`Verdict`]) — a compound is as suspicious as its most suspicious segment.
//! - **Lowering** ([`lower`]) — a shell submission is parsed into its
//!   constituent simple commands; a wrapper (`sh -c …`, `env …`) is unwrapped so
//!   a command is never classified as its wrapper (I3); anything that cannot be
//!   confidently parsed lowers to a single [`Lowered::Unknown`] (I4).
//! - **The role-typed policy** ([`Policy`], [`Rule`], [`Matcher`]) — a
//!   host-supplied partial function from programs to rules that type an argument
//!   vector into semantic roles, matching only when the *entire* vector is
//!   consumed (R4). Path-typed roles are checked against the same containment
//!   the built-in tools obey ([`RoleContainment`]); a binding that escapes
//!   raises that match to at least `prompt`.
//! - **The engine** ([`classify`]) — lower, match, validate roles, join, and
//!   report provenance; the unknown default `U` and the permission-mode floor
//!   ([`PermissionMode`]); the generalization guard ([`allow_rule_for_prefix`])
//!   that refuses to make an interpreter escape rulable-as-allow.
//!
//! The crate is pure: no loop, no provider, no process, no I/O, no filesystem.
//! It resolves no paths itself — role containment is delegated to a
//! [`RoleContainment`] the host adapts over its path policy. **Acting** on a
//! verdict — suspending a call, emitting an approval request on the event
//! stream, awaiting a human answer — is the integration layer a host wires over
//! this machinery (§3 of the RFC), and lands with a concrete approval channel;
//! [`without_channel`] is the pure rule it applies where no channel exists.

mod command;
mod engine;
mod policy;
mod verdict;

pub use command::{Command, Lowered, is_wrapper_escape, lower};
pub use engine::{
    ApprovalConfig, Classification, GeneralizeError, MatchOutcome, PermissionMode, RoleContainment,
    Segment, allow_rule_for_prefix, classify, without_channel,
};
pub use policy::{Binding, Example, Matcher, Policy, PolicyLoadError, ProgramRules, Role, Rule};
pub use verdict::Verdict;

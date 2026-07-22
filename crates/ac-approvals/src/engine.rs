//! The classification engine ([docs/ac-approvals.md] §2–§3).
//!
//! Classification runs entirely between the model's tool call and the spawn:
//! **lower** the submission, **match** each simple command against the policy,
//! **validate** the bound path roles against containment, **join** the verdicts,
//! and report the provenance an approval request would carry. This module is the
//! pure decision; *acting* on it — proceed, prompt, refuse — is the host's.

use crate::command::{Command, Lowered, is_wrapper_escape, lower};
use crate::policy::{Matcher, Policy, Role, Rule};
use crate::verdict::Verdict;

/// The path-containment predicate approvals delegates role checks to. The host
/// adapts it over the same path policy that contains the built-in tools —
/// `readable` ⟸ a read resolve succeeds, `writable` ⟸ a write resolve succeeds —
/// so a role binding is judged by *where it lands*, not by the tool's worst
/// case. Approvals resolves no paths itself; tests supply a fake.
pub trait RoleContainment {
    fn readable(&self, path: &str) -> bool;
    fn writable(&self, path: &str) -> bool;
}

/// One matched rule's contribution to a segment's verdict, with the provenance an
/// approval request surfaces (§3): which rule, its justification, and any path
/// bindings that failed containment (each of which raised this match to at least
/// `prompt`).
#[derive(Debug, Clone)]
pub struct MatchOutcome {
    pub rule_index: usize,
    pub verdict: Verdict,
    pub justification: Option<String>,
    pub failed_bindings: Vec<String>,
}

/// One lowered simple command's classification. `command` is `None` only for the
/// whole-submission unknown (an unparseable line).
#[derive(Debug, Clone)]
pub struct Segment {
    pub command: Option<Command>,
    pub verdict: Verdict,
    /// True iff no rule matched and the unknown default `U` was applied.
    pub unknown_default_applied: bool,
    pub matches: Vec<MatchOutcome>,
}

/// The result of classifying a shell submission: the aggregate verdict (the join
/// over segments), the per-segment provenance, and whether the whole submission
/// lowered to unknown.
#[derive(Debug, Clone)]
pub struct Classification {
    pub verdict: Verdict,
    pub segments: Vec<Segment>,
    pub lowered_unknown: bool,
}

impl Classification {
    /// The justifications of every matched rule, in segment then rule order —
    /// the full provenance an approval *prompt* shows (§3).
    pub fn justifications(&self) -> Vec<&str> {
        self.segments
            .iter()
            .flat_map(|s| s.matches.iter())
            .filter_map(|m| m.justification.as_deref())
            .collect()
    }

    /// Why the submission is not `safe` — for a refusal or prompt, drawn ONLY
    /// from the segments that raised the verdict. A `safe` segment contributes
    /// nothing, so `echo hi && rm -rf x` never cites "echo is safe" as the reason
    /// it was refused; the reason is the unknown `rm`. Each non-safe segment
    /// yields its forbidding rules' justifications, a failed-role-binding note, or
    /// a "not covered by any rule" note for an unknown-default segment.
    pub fn refusal_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        for seg in &self.segments {
            if seg.verdict == Verdict::Safe {
                continue;
            }
            if seg.unknown_default_applied {
                match &seg.command {
                    Some(cmd) => {
                        reasons.push(format!("{}: not covered by any approval rule", cmd.program))
                    }
                    None => reasons.push("command could not be parsed for classification".into()),
                }
                continue;
            }
            for m in &seg.matches {
                if m.verdict == Verdict::Safe {
                    continue;
                }
                if let Some(j) = &m.justification {
                    reasons.push(j.clone());
                } else if !m.failed_bindings.is_empty() {
                    reasons.push(format!(
                        "path outside the permitted region: {}",
                        m.failed_bindings.join(", ")
                    ));
                }
            }
        }
        reasons
    }
}

/// Classify a shell submission (§3). `unknown` is the host's unknown default `U`
/// — `Prompt` by default; a host MAY pass `Safe` only while kernel containment
/// is strict, and MUST NOT while it is degraded or off (the caller enforces that
/// bound — the engine honors whatever it is handed). The verdict is a pure
/// function of the lowered command, the policy, containment, and `unknown`:
/// nothing spawns here (I1).
pub fn classify(
    line: &str,
    policy: &Policy,
    containment: &dyn RoleContainment,
    unknown: Verdict,
) -> Classification {
    match lower(line) {
        Lowered::Unknown => Classification {
            verdict: unknown,
            segments: vec![Segment {
                command: None,
                verdict: unknown,
                unknown_default_applied: true,
                matches: Vec::new(),
            }],
            lowered_unknown: true,
        },
        Lowered::Commands(commands) => {
            let segments: Vec<Segment> = commands
                .into_iter()
                .map(|cmd| classify_command(cmd, policy, containment, unknown))
                .collect();
            // A compound is as suspicious as its most suspicious segment. An
            // empty command list cannot occur (lowering yields Commands only for
            // ≥1 command), but fail toward the unknown default if it ever did.
            let verdict = Verdict::join_all(segments.iter().map(|s| s.verdict)).unwrap_or(unknown);
            Classification {
                verdict,
                segments,
                lowered_unknown: false,
            }
        }
    }
}

fn classify_command(
    command: Command,
    policy: &Policy,
    containment: &dyn RoleContainment,
    unknown: Verdict,
) -> Segment {
    let mut matches = Vec::new();
    for (rule_index, rule) in policy.rules_for(&command.program).iter().enumerate() {
        let Some(bindings) = rule.structural_match(&command.args) else {
            continue;
        };
        // A structural match; now the role-containment check. A binding that
        // escapes containment raises THIS match to at least `prompt` (R4) —
        // `cp a b` auto-approves only if `a` is readable and `b` writable.
        let mut verdict = rule.verdict;
        let mut failed_bindings = Vec::new();
        for binding in &bindings {
            let contained = match binding.role {
                Role::Read => containment.readable(&binding.token),
                Role::Write => containment.writable(&binding.token),
            };
            if !contained {
                verdict = verdict.join(Verdict::Prompt);
                failed_bindings.push(binding.token.clone());
            }
        }
        matches.push(MatchOutcome {
            rule_index,
            verdict,
            justification: rule.justification.clone(),
            failed_bindings,
        });
    }

    // `verdict(c) = ⊔ { v(r) : r matches c }` if some r matches; else the unknown
    // default `U` — whether the program had rules or none (§2 "otherwise").
    match Verdict::join_all(matches.iter().map(|m| m.verdict)) {
        Some(verdict) => Segment {
            command: Some(command),
            verdict,
            unknown_default_applied: false,
            matches,
        },
        None => Segment {
            command: Some(command),
            verdict: unknown,
            unknown_default_applied: true,
            matches: Vec::new(),
        },
    }
}

/// Host-installed approval configuration, carried to the shell tool through the
/// tool context's typed extensions. Finding one, the shell tool classifies its
/// command line against `policy` (with `unknown` as `U`) before spawning; absent,
/// the tool runs unclassified. `unknown` MUST be `Prompt` unless the host knows
/// kernel containment is strict (§2) — the config records the host's choice, and
/// the shell tool honors it.
#[derive(Debug, Clone)]
pub struct ApprovalConfig {
    pub policy: Policy,
    pub unknown: Verdict,
}

impl ApprovalConfig {
    /// A config with `U = prompt` — the safe default that needs no containment
    /// assumption.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            unknown: Verdict::Prompt,
        }
    }

    /// Set the unknown default `U` (only lower it to `Safe` where containment is
    /// strict — §2).
    pub fn with_unknown(mut self, unknown: Verdict) -> Self {
        self.unknown = unknown;
        self
    }
}

/// A permission mode: the per-capability verdict floor `φ` applied to a
/// *dedicated* tool call (§2). For a read-only mode `φ(read-only) = safe` and
/// `φ(mutating) = prompt` (or `forbidden`, the host's choice); an unrestricted
/// mode floors both at `safe`. For the shell tool the intent verdict from
/// [`classify`] *replaces* this floor — it may fall below it (a command whose
/// matched roles are all read-only) or rise above it (`forbidden`).
///
/// The kit does not know `ac_tool::Capability` (this crate is dependency-free);
/// the host maps its capability to [`PermissionMode::read_only`] /
/// [`PermissionMode::mutating`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionMode {
    pub read_only: Verdict,
    pub mutating: Verdict,
}

impl PermissionMode {
    /// Both capabilities floored at `safe` — nothing is gated by the mode.
    pub fn unrestricted() -> Self {
        Self {
            read_only: Verdict::Safe,
            mutating: Verdict::Safe,
        }
    }

    /// Read-only calls run; mutating calls are gated at `gate` (`Prompt` or
    /// `Forbidden`).
    pub fn read_only(gate: Verdict) -> Self {
        Self {
            read_only: Verdict::Safe,
            mutating: gate,
        }
    }
}

/// §3: in a host with no approval channel, `prompt` MUST resolve to `forbidden`,
/// never to `safe`. The pure rule a caller applies where no channel exists.
pub fn without_channel(verdict: Verdict) -> Verdict {
    match verdict {
        Verdict::Prompt => Verdict::Forbidden,
        other => other,
    }
}

/// Why a generalization was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneralizeError {
    /// The prefix names an interpreter or wrapper escape (`sh -c`, `env`, …); an
    /// allow rule for it would allow arbitrary commands (§3).
    WrapperEscape(String),
    /// The prefix has no program.
    Empty,
}

impl std::fmt::Display for GeneralizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeneralizeError::WrapperEscape(program) => write!(
                f,
                "cannot create an allow rule for {program:?}: it is an interpreter or wrapper \
                 escape, so the rule would allow arbitrary commands"
            ),
            GeneralizeError::Empty => {
                f.write_str("cannot create an allow rule for an empty prefix")
            }
        }
    }
}

impl std::error::Error for GeneralizeError {}

/// Build an allow (`safe`) rule for an exact matched prefix — the mechanics
/// behind an approval's "don't ask again for this prefix" (§3). The guard: a
/// prefix naming an interpreter or wrapper escape is unrulable, because such a
/// rule would allow everything; the engine refuses to create it. The returned
/// rule matches `program` invoked with exactly `literal_args` (as literals) and
/// nothing more; the host appends it to its policy (via [`Policy::amend`]) and
/// owns persistence and scope. Generalization only ever lowers the *unknown*
/// default for this prefix; it never lowers an explicit rule-matched verdict (I2).
pub fn allow_rule_for_prefix(
    program: &str,
    literal_args: &[String],
) -> Result<Rule, GeneralizeError> {
    if program.is_empty() {
        return Err(GeneralizeError::Empty);
    }
    if is_wrapper_escape(program) {
        return Err(GeneralizeError::WrapperEscape(program.to_string()));
    }
    let matchers = literal_args
        .iter()
        .map(|a| Matcher::Literal(a.clone()))
        .collect::<Vec<_>>();
    Ok(Rule::new(matchers, Verdict::Safe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Example, ProgramRules};

    /// Containment that reads two allow-lists of literal tokens.
    struct FakeContainment {
        readable: Vec<&'static str>,
        writable: Vec<&'static str>,
    }
    impl RoleContainment for FakeContainment {
        fn readable(&self, path: &str) -> bool {
            self.readable.contains(&path)
        }
        fn writable(&self, path: &str) -> bool {
            self.writable.contains(&path)
        }
    }

    /// Containment that contains every path — for tests isolating rule verdicts.
    struct AllContained;
    impl RoleContainment for AllContained {
        fn readable(&self, _: &str) -> bool {
            true
        }
        fn writable(&self, _: &str) -> bool {
            true
        }
    }

    fn cp_policy() -> Policy {
        Policy::load([ProgramRules::new(
            "cp",
            [
                Rule::new([Matcher::ReadPath, Matcher::WritePath], Verdict::Safe)
                    .justified("copy is safe when both ends are contained")
                    .with_examples([Example::matching(["a", "b"]), Example::non_matching(["a"])]),
            ],
        )])
        .unwrap()
    }

    #[test]
    fn a_contained_copy_is_safe() {
        let policy = cp_policy();
        let containment = FakeContainment {
            readable: vec!["a"],
            writable: vec!["b"],
        };
        let c = classify("cp a b", &policy, &containment, Verdict::Prompt);
        assert_eq!(c.verdict, Verdict::Safe);
        assert!(!c.lowered_unknown);
    }

    #[test]
    fn an_escaping_write_raises_the_copy_to_prompt() {
        let policy = cp_policy();
        let containment = FakeContainment {
            readable: vec!["a"],
            writable: vec![], // `b` not writable — escapes
        };
        let c = classify("cp a b", &policy, &containment, Verdict::Prompt);
        assert_eq!(c.verdict, Verdict::Prompt);
        assert_eq!(
            c.segments[0].matches[0].failed_bindings,
            vec!["b".to_string()]
        );
    }

    #[test]
    fn an_unknown_command_takes_the_unknown_default() {
        let policy = cp_policy();
        // `rm` has no rule → unknown → the default we pass.
        let prompt = classify("rm x", &policy, &AllContained, Verdict::Prompt);
        assert_eq!(prompt.verdict, Verdict::Prompt);
        assert!(prompt.segments[0].unknown_default_applied);
        // Under strict containment a host MAY set U = safe.
        let safe = classify("rm x", &policy, &AllContained, Verdict::Safe);
        assert_eq!(safe.verdict, Verdict::Safe);
    }

    #[test]
    fn a_compound_takes_its_most_suspicious_segment() {
        let policy = Policy::load([
            ProgramRules::new("echo", [Rule::new([Matcher::Rest], Verdict::Safe)]),
            ProgramRules::new("rm", [Rule::new([Matcher::Rest], Verdict::Forbidden)]),
        ])
        .unwrap();
        let c = classify(
            "echo hi && rm -rf x",
            &policy,
            &AllContained,
            Verdict::Prompt,
        );
        assert_eq!(c.verdict, Verdict::Forbidden);
        assert_eq!(c.segments.len(), 2);
    }

    #[test]
    fn a_naive_safe_rule_for_a_code_interpreter_cannot_auto_approve_it() {
        // A host writes what looks like a reasonable read-only rule: `awk
        // <script> <file> -> Safe`. It MUST NOT auto-approve `system()`, because
        // awk is an escape that lowers to Unknown before the rule is consulted.
        let policy = Policy::load([
            ProgramRules::new(
                "awk",
                [Rule::new(
                    [Matcher::Opaque, Matcher::ReadPath],
                    Verdict::Safe,
                )],
            ),
            ProgramRules::new(
                "find",
                [Rule::new([Matcher::ReadPath, Matcher::Rest], Verdict::Safe)],
            ),
            ProgramRules::new("make", [Rule::new([Matcher::Rest], Verdict::Safe)]),
        ])
        .unwrap();
        for line in [
            "awk 'BEGIN{system(\"rm -rf /\")}' data.txt",
            "find . -delete",
            "make",
        ] {
            let c = classify(line, &policy, &AllContained, Verdict::Prompt);
            assert_eq!(c.verdict, Verdict::Prompt, "{line:?} must not be safe");
            assert!(c.lowered_unknown, "{line:?} should lower to unknown");
        }
    }

    #[test]
    fn a_wrapper_hides_nothing_it_lowers_to_the_inner_command() {
        let policy = Policy::load([ProgramRules::new(
            "rm",
            [Rule::new([Matcher::Rest], Verdict::Forbidden)],
        )])
        .unwrap();
        // `sh -c 'rm ...'` is classified as `rm`, so the forbidden rule applies —
        // NOT as `sh`, which has no rule (that would be merely unknown).
        let c = classify("sh -c 'rm -rf /'", &policy, &AllContained, Verdict::Prompt);
        assert_eq!(c.verdict, Verdict::Forbidden);
    }

    #[test]
    fn an_unparseable_line_is_the_unknown_default_and_flagged() {
        let policy = cp_policy();
        let c = classify("echo $(rm -rf /)", &policy, &AllContained, Verdict::Prompt);
        assert!(c.lowered_unknown);
        assert_eq!(c.verdict, Verdict::Prompt);
        assert!(c.segments[0].command.is_none());
    }

    #[test]
    fn refusal_reasons_cite_only_the_deciding_segments() {
        let policy = Policy::load([ProgramRules::new(
            "echo",
            [Rule::new([Matcher::Rest], Verdict::Safe).justified("echo is safe")],
        )])
        .unwrap();
        // echo (safe) && rm (unknown → prompt); aggregate prompt.
        let c = classify("echo hi && rm x", &policy, &AllContained, Verdict::Prompt);
        let reasons = c.refusal_reasons();
        assert!(
            reasons.iter().any(|r| r.contains("rm")),
            "should cite the unknown rm: {reasons:?}"
        );
        assert!(
            !reasons.iter().any(|r| r.contains("echo is safe")),
            "must not cite the safe segment: {reasons:?}"
        );
    }

    #[test]
    fn justifications_surface_from_matched_rules() {
        let policy = cp_policy();
        let containment = FakeContainment {
            readable: vec!["a"],
            writable: vec!["b"],
        };
        let c = classify("cp a b", &policy, &containment, Verdict::Prompt);
        assert_eq!(
            c.justifications(),
            vec!["copy is safe when both ends are contained"]
        );
    }

    #[test]
    fn without_channel_maps_prompt_to_forbidden_only() {
        assert_eq!(without_channel(Verdict::Prompt), Verdict::Forbidden);
        assert_eq!(without_channel(Verdict::Safe), Verdict::Safe);
        assert_eq!(without_channel(Verdict::Forbidden), Verdict::Forbidden);
    }

    #[test]
    fn permission_modes_floor_by_capability() {
        let ro = PermissionMode::read_only(Verdict::Prompt);
        assert_eq!(ro.read_only, Verdict::Safe);
        assert_eq!(ro.mutating, Verdict::Prompt);
        let open = PermissionMode::unrestricted();
        assert_eq!(open.read_only, Verdict::Safe);
        assert_eq!(open.mutating, Verdict::Safe);
    }

    #[test]
    fn allow_rules_refuse_wrapper_escapes() {
        // A normal prefix rules fine.
        let rule = allow_rule_for_prefix("git", &["status".to_string()]).unwrap();
        assert_eq!(rule.verdict, Verdict::Safe);
        assert!(rule.structural_match(&["status".to_string()]).is_some());
        // Interpreter / wrapper escapes are refused — the §3 guard.
        for escape in ["sh", "bash", "env", "python3", "xargs", "sudo"] {
            assert!(matches!(
                allow_rule_for_prefix(escape, &[]),
                Err(GeneralizeError::WrapperEscape(_))
            ));
        }
        assert!(matches!(
            allow_rule_for_prefix("", &[]),
            Err(GeneralizeError::Empty)
        ));
    }
}

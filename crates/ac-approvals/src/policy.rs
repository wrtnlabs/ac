//! The role-typed policy ([docs/ac-approvals.md] §2).
//!
//! A **policy** is a partial function from program names to rule sets,
//! host-supplied (R3) — the kit ships the engine and the taxonomy, never the
//! content. A **rule** types an argument vector into **semantic roles** via an
//! ordered list of [`Matcher`]s, and matches a command only if the *entire*
//! vector is consumed (R4): an unrecognized flag or an unaccounted position is a
//! non-match, not a partial one. Path-typed roles ([`Matcher::ReadPath`],
//! [`Matcher::WritePath`]) record their bound token so the engine can check it
//! against containment. Each rule carries a [`Verdict`], an optional
//! justification surfaced in prompts and refusals, and [`Example`] invocations
//! validated when the policy loads — a policy that fails its own examples is
//! rejected ([`Policy::load`]).

use std::collections::HashMap;
use std::fmt;

use crate::verdict::Verdict;

/// A semantic role a matcher assigns while consuming argument tokens (§2). The
/// two path roles are the load-bearing ones: they turn a bare token into a
/// *readable* or *writable* position the engine checks against containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Exactly this literal token (a subcommand like `status`, or a fixed flag).
    Literal(String),
    /// Zero or more tokens, each one of these recognized flags — an order-free
    /// run consumed at this position (`-l`, `--all`, `-la`). Matches greedily.
    Flags(Vec<String>),
    /// One of these option flags followed by one opaque value token
    /// (`--output foo`). Two tokens; the value is not containment-checked.
    ValueOption(Vec<String>),
    /// One token naming a path that must resolve within READ containment.
    ReadPath,
    /// One token naming a path that must resolve within WRITE containment.
    WritePath,
    /// One opaque non-path token — matched but never containment-checked.
    Opaque,
    /// All remaining tokens, unverified: the rule author's explicit
    /// acknowledgement that the tail is untyped. Consumes to end.
    Rest,
}

/// The role a bound path token plays, for the engine's containment check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Read,
    Write,
}

/// A path token a matched rule bound to a path role, awaiting the containment
/// check. `token` is the literal argument as the model wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub role: Role,
    pub token: String,
}

/// A validated example invocation: `args` are the argument tokens (the program
/// excluded) and `should_match` is whether the rule's *structure* must accept
/// them. Examples exercise structural matching only — containment needs a live
/// path policy the load step does not have.
#[derive(Debug, Clone)]
pub struct Example {
    pub args: Vec<String>,
    pub should_match: bool,
}

impl Example {
    pub fn matching(args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            args: args.into_iter().map(Into::into).collect(),
            should_match: true,
        }
    }

    pub fn non_matching(args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            args: args.into_iter().map(Into::into).collect(),
            should_match: false,
        }
    }
}

/// A rule: an ordered matcher pattern, the verdict a match carries, an optional
/// justification, and self-validating examples.
#[derive(Debug, Clone)]
pub struct Rule {
    pub matchers: Vec<Matcher>,
    pub verdict: Verdict,
    pub justification: Option<String>,
    pub examples: Vec<Example>,
}

impl Rule {
    /// A rule with the given pattern and verdict, no justification, no examples.
    pub fn new(matchers: impl IntoIterator<Item = Matcher>, verdict: Verdict) -> Self {
        Self {
            matchers: matchers.into_iter().collect(),
            verdict,
            justification: None,
            examples: Vec::new(),
        }
    }

    pub fn justified(mut self, justification: impl Into<String>) -> Self {
        self.justification = Some(justification.into());
        self
    }

    pub fn with_examples(mut self, examples: impl IntoIterator<Item = Example>) -> Self {
        self.examples = examples.into_iter().collect();
        self
    }

    /// Structural match against an argument vector. `Some(bindings)` iff the
    /// pattern consumes the *entire* vector (R4); the bindings are the path
    /// tokens for the engine's containment check. This does **not** consult
    /// containment — a match here is structural, and the engine may still raise
    /// the verdict when a binding escapes.
    pub fn structural_match(&self, args: &[String]) -> Option<Vec<Binding>> {
        let mut cursor = 0usize;
        let mut bindings = Vec::new();
        for matcher in &self.matchers {
            match matcher {
                Matcher::Literal(lit) => {
                    if args.get(cursor).map(String::as_str) == Some(lit.as_str()) {
                        cursor += 1;
                    } else {
                        return None;
                    }
                }
                Matcher::Flags(flags) => {
                    while args
                        .get(cursor)
                        .is_some_and(|a| flags.iter().any(|f| f == a))
                    {
                        cursor += 1;
                    }
                }
                Matcher::ValueOption(flags) => {
                    if args
                        .get(cursor)
                        .is_some_and(|a| flags.iter().any(|f| f == a))
                    {
                        cursor += 1;
                        if args.get(cursor).is_some() {
                            cursor += 1; // the value token
                        } else {
                            return None; // option with no value
                        }
                    } else {
                        return None;
                    }
                }
                Matcher::ReadPath => match args.get(cursor) {
                    Some(tok) => {
                        bindings.push(Binding {
                            role: Role::Read,
                            token: tok.clone(),
                        });
                        cursor += 1;
                    }
                    None => return None,
                },
                Matcher::WritePath => match args.get(cursor) {
                    Some(tok) => {
                        bindings.push(Binding {
                            role: Role::Write,
                            token: tok.clone(),
                        });
                        cursor += 1;
                    }
                    None => return None,
                },
                Matcher::Opaque => {
                    if args.get(cursor).is_some() {
                        cursor += 1;
                    } else {
                        return None;
                    }
                }
                Matcher::Rest => {
                    cursor = args.len();
                }
            }
        }
        // The entire vector must be consumed — leftover tokens are a non-match.
        if cursor == args.len() {
            Some(bindings)
        } else {
            None
        }
    }
}

/// The rule set for one program.
#[derive(Debug, Clone)]
pub struct ProgramRules {
    pub program: String,
    pub rules: Vec<Rule>,
}

impl ProgramRules {
    pub fn new(program: impl Into<String>, rules: impl IntoIterator<Item = Rule>) -> Self {
        Self {
            program: program.into(),
            rules: rules.into_iter().collect(),
        }
    }
}

/// Why a policy was rejected at load (§2: "a policy that fails its own examples
/// MUST be rejected at load"). Fail-toward-prompt applies to *classification*;
/// a malformed policy is a host-authoring error surfaced eagerly instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyLoadError {
    pub program: String,
    pub rule_index: usize,
    pub example: Vec<String>,
    pub expected_match: bool,
}

impl fmt::Display for PolicyLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "approval policy rejected: program {:?} rule {} fails its own example {:?} \
             (expected {} to match)",
            self.program,
            self.rule_index,
            self.example,
            if self.expected_match { "" } else { "NOT " },
        )
    }
}

impl std::error::Error for PolicyLoadError {}

/// A host-supplied policy: a partial function from programs to rule sets.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: HashMap<String, Vec<Rule>>,
}

impl Policy {
    /// An empty policy — every command is unknown (the engine applies `U`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load a policy, validating every rule against its own examples. Rules for
    /// the same program across entries accumulate (in entry order), so a host may
    /// compose a base policy with amendments. Rejected (returns the first
    /// failure) iff any example's structural match disagrees with its
    /// `should_match`.
    pub fn load(programs: impl IntoIterator<Item = ProgramRules>) -> Result<Self, PolicyLoadError> {
        let mut rules: HashMap<String, Vec<Rule>> = HashMap::new();
        for entry in programs {
            for (rule_index, rule) in entry.rules.iter().enumerate() {
                for example in &rule.examples {
                    let matched = rule.structural_match(&example.args).is_some();
                    if matched != example.should_match {
                        return Err(PolicyLoadError {
                            program: entry.program.clone(),
                            rule_index,
                            example: example.args.clone(),
                            expected_match: example.should_match,
                        });
                    }
                }
            }
            rules.entry(entry.program).or_default().extend(entry.rules);
        }
        Ok(Self { rules })
    }

    /// The rules for `program`, in declaration order (empty if the policy is not
    /// defined there — the engine reads that as unknown).
    pub fn rules_for(&self, program: &str) -> &[Rule] {
        self.rules.get(program).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Append an allow rule for a program — the mechanics behind an approval's
    /// "don't ask again" generalization (§3). The kit supplies the append; the
    /// engine's [`crate::allow_rule_for_prefix`] supplies the guard, and the host
    /// owns persistence and scope.
    pub fn amend(&mut self, program: impl Into<String>, rule: Rule) {
        self.rules.entry(program.into()).or_default().push(rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn full_consumption_is_required() {
        let rule = Rule::new(
            [Matcher::Flags(vec!["-l".into()]), Matcher::ReadPath],
            Verdict::Safe,
        );
        // `-l path` — flags run then the read path.
        assert!(rule.structural_match(&args(&["-l", "/tmp"])).is_some());
        // A trailing unaccounted token is a non-match, not a partial match.
        assert!(
            rule.structural_match(&args(&["-l", "/tmp", "extra"]))
                .is_none()
        );
        // An unrecognized flag is a non-match.
        assert!(rule.structural_match(&args(&["-z", "/tmp"])).is_none());
    }

    #[test]
    fn path_roles_bind_their_tokens() {
        let rule = Rule::new([Matcher::ReadPath, Matcher::WritePath], Verdict::Prompt);
        let bindings = rule.structural_match(&args(&["src", "dst"])).unwrap();
        assert_eq!(
            bindings,
            vec![
                Binding {
                    role: Role::Read,
                    token: "src".into()
                },
                Binding {
                    role: Role::Write,
                    token: "dst".into()
                },
            ]
        );
    }

    #[test]
    fn flags_match_zero_or_more() {
        let rule = Rule::new(
            [Matcher::Flags(vec!["-a".into()]), Matcher::Opaque],
            Verdict::Safe,
        );
        assert!(rule.structural_match(&args(&["x"])).is_some()); // zero flags
        assert!(rule.structural_match(&args(&["-a", "x"])).is_some()); // one flag
    }

    #[test]
    fn value_options_consume_a_following_token() {
        let rule = Rule::new(
            [Matcher::ValueOption(vec!["-o".into()]), Matcher::WritePath],
            Verdict::Prompt,
        );
        let b = rule.structural_match(&args(&["-o", "fmt", "out"])).unwrap();
        assert_eq!(
            b,
            vec![Binding {
                role: Role::Write,
                token: "out".into()
            }]
        );
        // Option with no value: non-match.
        assert!(rule.structural_match(&args(&["-o"])).is_none());
    }

    #[test]
    fn rest_consumes_the_tail_unverified() {
        let rule = Rule::new(
            [Matcher::Literal("commit".into()), Matcher::Rest],
            Verdict::Safe,
        );
        assert!(
            rule.structural_match(&args(&["commit", "-m", "msg", "--amend"]))
                .is_some()
        );
        assert!(rule.structural_match(&args(&["commit"])).is_some()); // empty tail
        assert!(rule.structural_match(&args(&["push"])).is_none()); // wrong literal
    }

    #[test]
    fn a_policy_that_fails_its_own_examples_is_rejected() {
        let bad = ProgramRules::new(
            "ls",
            [Rule::new([Matcher::ReadPath], Verdict::Safe)
                .with_examples([Example::matching(["a", "b"])])], // two args, one ReadPath
        );
        let err = Policy::load([bad]).unwrap_err();
        assert_eq!(err.program, "ls");
        assert!(err.expected_match);
    }

    #[test]
    fn a_policy_that_passes_its_examples_loads() {
        let good = ProgramRules::new(
            "ls",
            [Rule::new(
                [Matcher::Flags(vec!["-l".into()]), Matcher::ReadPath],
                Verdict::Safe,
            )
            .with_examples([
                Example::matching(["-l", "/tmp"]),
                Example::matching(["/tmp"]),
                Example::non_matching(["-l", "/tmp", "extra"]),
            ])],
        );
        let policy = Policy::load([good]).unwrap();
        assert_eq!(policy.rules_for("ls").len(), 1);
        assert!(policy.rules_for("cat").is_empty());
    }

    #[test]
    fn rules_for_a_program_accumulate_across_entries() {
        let policy = Policy::load([
            ProgramRules::new(
                "git",
                [Rule::new(
                    [Matcher::Literal("status".into())],
                    Verdict::Safe,
                )],
            ),
            ProgramRules::new(
                "git",
                [Rule::new([Matcher::Literal("log".into())], Verdict::Safe)],
            ),
        ])
        .unwrap();
        assert_eq!(policy.rules_for("git").len(), 2);
    }
}

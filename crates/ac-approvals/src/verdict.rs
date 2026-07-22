//! The verdict lattice ([docs/ac-approvals.md] §2).

use std::fmt;

/// The approval lattice, totally ordered:
///
/// > `safe ⊏ prompt ⊏ forbidden`
///
/// read as: run without asking ⊏ ask the user ⊏ refuse without asking.
/// Aggregation is the **join** ([`Verdict::join`]) — a compound command is as
/// suspicious as its most suspicious segment, and overlapping rules resolve to
/// the strictest. The `Ord` derive is in ascending strictness, so the join is
/// exactly `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Run without asking.
    Safe,
    /// Ask the user.
    Prompt,
    /// Refuse without asking.
    Forbidden,
}

impl Verdict {
    /// The join `a ⊔ b` — the stricter (higher) of the two. Monotone: joining
    /// never lowers a verdict (I2).
    pub fn join(self, other: Verdict) -> Verdict {
        self.max(other)
    }

    /// The join over a sequence, or `None` if it is empty. An empty match set is
    /// the caller's signal to fall back to the unknown default, never to `Safe`.
    pub fn join_all(verdicts: impl IntoIterator<Item = Verdict>) -> Option<Verdict> {
        verdicts.into_iter().reduce(Verdict::join)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Safe => "safe",
            Verdict::Prompt => "prompt",
            Verdict::Forbidden => "forbidden",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lattice_is_ordered_by_strictness() {
        assert!(Verdict::Safe < Verdict::Prompt);
        assert!(Verdict::Prompt < Verdict::Forbidden);
    }

    #[test]
    fn join_takes_the_stricter() {
        assert_eq!(Verdict::Safe.join(Verdict::Prompt), Verdict::Prompt);
        assert_eq!(Verdict::Prompt.join(Verdict::Safe), Verdict::Prompt);
        assert_eq!(Verdict::Prompt.join(Verdict::Forbidden), Verdict::Forbidden);
        assert_eq!(Verdict::Forbidden.join(Verdict::Safe), Verdict::Forbidden);
        // Idempotent.
        assert_eq!(Verdict::Prompt.join(Verdict::Prompt), Verdict::Prompt);
    }

    #[test]
    fn join_all_folds_or_reports_empty() {
        assert_eq!(
            Verdict::join_all([Verdict::Safe, Verdict::Forbidden, Verdict::Prompt]),
            Some(Verdict::Forbidden)
        );
        assert_eq!(Verdict::join_all([Verdict::Safe]), Some(Verdict::Safe));
        assert_eq!(Verdict::join_all([]), None);
    }
}

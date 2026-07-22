//! The registry that makes `injected(t)` decidable ([docs/ac-context.md] §3).
//! A registry is the runtime's set of fragment classes; recognition against it
//! is a pure function of an item's text, which is what R1 requires.

use crate::fragment::FragmentClass;

/// The set of fragment classes a runtime knows. `injected(t)` — the union of the
/// classes' recognition predicates — is the seam every consumer of "is this
/// machine-injected?" goes through: user-input filtering, transcript
/// projection, strip-on-compaction, and injection dedupe (§3).
#[derive(Debug, Clone, Default)]
pub struct FragmentRegistry {
    classes: Vec<FragmentClass>,
}

impl FragmentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style registration.
    pub fn with(mut self, class: FragmentClass) -> Self {
        self.classes.push(class);
        self
    }

    pub fn register(&mut self, class: FragmentClass) {
        self.classes.push(class);
    }

    pub fn classes(&self) -> &[FragmentClass] {
        &self.classes
    }

    /// `injected(t) ⟺ ∃φ. marked_φ(t)` — is this item text a machine-injected
    /// fragment of *any* registered class?
    pub fn injected(&self, text: &str) -> bool {
        self.classes.iter().any(|c| c.marked(text))
    }

    /// The first registered class that recognizes `text`, if any — for a
    /// consumer that needs the class (its cadence, its name), not just the
    /// yes/no of [`injected`](Self::injected).
    pub fn recognize(&self, text: &str) -> Option<&FragmentClass> {
        self.classes.iter().find(|c| c.marked(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::Cadence;
    use ac_types::Role;

    fn registry() -> FragmentRegistry {
        FragmentRegistry::new()
            .with(FragmentClass::new(
                "catalog",
                Role::User,
                "<<SKILLS",
                "SKILLS>>",
                Some(Cadence::Window),
                4096,
            ))
            .with(FragmentClass::new(
                "handoff",
                Role::User,
                "[handoff]",
                "[/handoff]",
                None,
                usize::MAX,
            ))
    }

    #[test]
    fn injected_is_the_union_over_classes() {
        let r = registry();
        assert!(r.injected("<<SKILLS list SKILLS>>"));
        assert!(r.injected("[handoff] did the work [/handoff]"));
        assert!(!r.injected("what the user actually typed"));
    }

    #[test]
    fn recognize_returns_the_matching_class() {
        let r = registry();
        assert_eq!(
            r.recognize("[handoff] x [/handoff]").unwrap().name,
            "handoff"
        );
        assert!(r.recognize("plain text").is_none());
    }
}

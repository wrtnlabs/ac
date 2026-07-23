//! The reactive-section driver decision ([docs/ac-context.md] §4–§5,
//! [docs/ac-ultra.md] §4).
//!
//! A reactive section renders a fragment whose **prior** — what the model was
//! last told — is read from the effective history (the last item recognized as
//! the section's class), never a retained in-memory snapshot. That single choice
//! is what makes the cadence sound across compaction, resume, and fork: after a
//! compaction strips the section's fragment, `prior` is absent and the section
//! re-emits into the new window; a resumed or forked session reads the logged
//! fragment as its prior. The rendered fragment *is* the comparison value, so no
//! separate snapshot type is needed — the [`crate::decide`] primitive's job is
//! done here by string equality against the log.

use crate::fragment::FragmentClass;

/// A change-detected context section (the ℛ cadence). The driver appends its
/// rendered fragment when it differs from the last one recognized in history,
/// and nothing when it does not — so an unchanged section costs zero marginal
/// tokens and the provider's prompt cache holds.
pub trait ReactiveSection: Send + Sync {
    /// The class whose markers make this section's fragments recognizable — for
    /// prior recovery here, and for the compaction strip and user-input filter
    /// elsewhere. The class MUST be registered so those consumers see it.
    fn class(&self) -> &FragmentClass;
    /// The current fragment body, or `None` when the section has nothing to say.
    fn body(&self) -> Option<String>;
}

/// The fragment a reactive section should append at a boundary, or `None` to
/// stay silent — given the effective history's item texts in order. `prior` is
/// the last text recognized as the section's class; the result is `None` when
/// the current render equals it (no change) or when both are absent.
///
/// (A transition to `None` body while a prior fragment exists — "becoming empty"
/// — returns `None` here, i.e. appends nothing; delegation-mode, the only
/// consumer, always has a body, so this edge is unexercised.)
pub fn reactive_fragment(section: &dyn ReactiveSection, history: &[&str]) -> Option<String> {
    let now = section.body().map(|b| section.class().render(&b).text);
    let prior = history
        .iter()
        .rev()
        .find(|t| section.class().marked(t))
        .map(|t| t.to_string());
    if now == prior { None } else { now }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct ModeSection {
        class: FragmentClass,
        mode: Mutex<&'static str>,
    }
    impl ReactiveSection for ModeSection {
        fn class(&self) -> &FragmentClass {
            &self.class
        }
        fn body(&self) -> Option<String> {
            Some(format!("delegation mode is {}", *self.mode.lock().unwrap()))
        }
    }

    fn section() -> ModeSection {
        ModeSection {
            class: FragmentClass::new(
                "delegation-mode",
                ac_types::Role::User,
                "[[mode:",
                ":mode]]",
                Some(crate::fragment::Cadence::Reactive),
                4096,
            ),
            mode: Mutex::new("proactive"),
        }
    }

    #[test]
    fn emits_when_absent_then_silent_when_unchanged() {
        let s = section();
        // Empty history → emit (first injection).
        let frag = reactive_fragment(&s, &[]).expect("first emit");
        assert!(frag.contains("proactive"));
        // History now contains the fragment → unchanged → silent.
        assert_eq!(reactive_fragment(&s, &[frag.as_str()]), None);
    }

    #[test]
    fn a_flip_supersedes() {
        let s = section();
        let first = reactive_fragment(&s, &[]).unwrap();
        *s.mode.lock().unwrap() = "on-request";
        let second = reactive_fragment(&s, &[first.as_str()]).expect("flip emits");
        assert!(second.contains("on-request"));
        assert_ne!(first, second);
    }

    #[test]
    fn re_emits_when_the_prior_fragment_was_stripped() {
        // The compaction case: the fragment is gone from the (post-strip)
        // history, so prior is absent and the section re-emits — even though the
        // desired mode never changed. This is what keeps a new window from being
        // mode-blind without any retained snapshot.
        let s = section();
        let unrelated = "just a user message, not a mode fragment";
        let frag = reactive_fragment(&s, &[unrelated]).expect("re-emit after strip");
        assert!(frag.contains("proactive"));
    }
}

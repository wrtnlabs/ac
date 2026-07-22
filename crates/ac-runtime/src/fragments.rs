//! The runtime's fragment registry ([docs/ac-context.md] §3). Recognition is how
//! the runtime tells its own machine-injected text apart from what the user said,
//! from the item text alone — so compaction's `U` (and, in time, transcript
//! projection and mention scanning) never promote machinery to instructions.
//!
//! Two classes live here today, both runtime *lifecycle* fragments (no cadence
//! driver — they are written by compaction and by cancellation, not injected on
//! a cadence): the compaction handoff and the interruption marker. The cadence
//! classes (window catalog, per-turn mentions, reactive state) register here as
//! their drivers land.

use ac_context::{FragmentClass, FragmentRegistry};
use ac_types::Role;

use crate::compaction::{HANDOFF_CLOSE, HANDOFF_PREAMBLE};

/// The interruption marker's own text, split into a recognizable open/close pair
/// so the fragment predicate matches it *without* changing the wire text. Kept
/// in lockstep with `ac_types::INTERRUPTION_MARKER` by a test below.
const INTERRUPTION_OPEN: &str = "The previous turn was interrupted on purpose.";
const INTERRUPTION_CLOSE: &str = "do not assume its work completed.";

/// Build the runtime's registry of recognizable fragment classes.
pub(crate) fn runtime_registry() -> FragmentRegistry {
    FragmentRegistry::new()
        .with(FragmentClass::new(
            "compaction-handoff",
            Role::User,
            HANDOFF_PREAMBLE,
            HANDOFF_CLOSE,
            None,
            usize::MAX,
        ))
        .with(FragmentClass::new(
            "interruption-marker",
            Role::User,
            INTERRUPTION_OPEN,
            INTERRUPTION_CLOSE,
            None,
            usize::MAX,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::INTERRUPTION_MARKER;

    #[test]
    fn recognizes_the_interruption_marker_verbatim() {
        // If ac-types edits the marker such that these markers no longer bracket
        // it, this fails — the split constants must track the source string.
        assert!(
            runtime_registry().injected(INTERRUPTION_MARKER),
            "the interruption marker must be recognized as an injected fragment"
        );
    }

    #[test]
    fn plain_user_text_is_not_recognized() {
        assert!(!runtime_registry().injected("please make me a deck about otters"));
    }
}

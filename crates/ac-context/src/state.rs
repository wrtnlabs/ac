//! Change-detected state — the ℛ cadence ([docs/ac-context.md] §5). A state
//! section tracks mutable ambient state and emits *only* when what the model
//! would be told differs from what it was last told, so unchanged state costs
//! zero marginal tokens and the provider's prompt cache holds across turns whose
//! state did not change (R3).
//!
//! This module is the pure decision `emit(s)`. The snapshot function `σ_s` (the
//! comparison value) and the renderer `ρ_s` (the fragment body) are the
//! contributor's — §8 — so the runtime evaluates the decision without knowing
//! what any section means.

/// Three-valued prior knowledge `k` of a section at a turn boundary (§5):
///
/// - `Absent` — no prior emission on record.
/// - `Unknown` — a prior emission exists (its fragment is recognized in `H`) but
///   its snapshot is unrecoverable (a resume or fork replay lost it).
/// - `Known(v)` — the exact prior snapshot `v`.
///
/// Snapshots SHOULD ride the session log so `Known` is the common case; `Unknown`
/// is the conservative recovery state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prior<S> {
    Absent,
    Unknown,
    Known(S),
}

/// Whether a section emits a fresh fragment this turn boundary, or stays silent.
/// Silence is meaningful: `Skip` ⟺ the model's current view is still correct
/// (I5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip,
    Emit,
}

/// `emit(s)` of §5. `now` is the current snapshot `σ_s(now)`; `now_is_empty`
/// says whether it is the section's empty value. Both directions of R3:
///
/// - `Known(v)` with `v = now` → `Skip` — snapshot equality suppresses emission.
/// - `Absent` with an empty `now` → `Skip` — nothing on record, nothing to say.
/// - otherwise → `Emit`.
///
/// The "otherwise" is load-bearing. It forces a fragment on a genuine change
/// (`Known(v) ≠ now`), on conservative recovery (`Unknown`), on first content
/// (`Absent` + non-empty), and on *becoming empty* — a transition from
/// `Known(non-empty)` or `Unknown` to nothing is a change, said aloud, never
/// silence (§5, "becoming-empty is a change").
pub fn decide<S: PartialEq>(prior: &Prior<S>, now: &S, now_is_empty: bool) -> Decision {
    match prior {
        Prior::Known(v) if v == now => Decision::Skip,
        Prior::Absent if now_is_empty => Decision::Skip,
        _ => Decision::Emit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_equality_suppresses_emission() {
        assert_eq!(decide(&Prior::Known("v1"), &"v1", false), Decision::Skip);
    }

    #[test]
    fn a_genuine_change_forces_emission() {
        assert_eq!(decide(&Prior::Known("v1"), &"v2", false), Decision::Emit);
    }

    #[test]
    fn absent_and_empty_stays_silent_but_absent_and_content_emits() {
        assert_eq!(decide(&Prior::Absent, &"", true), Decision::Skip);
        assert_eq!(decide(&Prior::Absent, &"content", false), Decision::Emit);
    }

    #[test]
    fn becoming_empty_is_a_change_that_emits() {
        // known(non-empty) → empty: the snapshots differ, so Emit.
        assert_eq!(
            decide(&Prior::Known("had content"), &"", true),
            Decision::Emit
        );
        // unknown → empty: conservative, Emit regardless.
        assert_eq!(decide(&Prior::Unknown, &"", true), Decision::Emit);
    }

    #[test]
    fn unknown_always_re_emits_conservatively() {
        assert_eq!(decide(&Prior::Unknown, &"anything", false), Decision::Emit);
    }

    #[test]
    fn known_empty_equal_to_empty_now_stays_silent() {
        // The cell the whole rule hinges on: arm 1 keys on snapshot equality
        // alone (v == now), so a section last told "empty" and still empty is
        // silent — is_empty MUST NOT leak into the equality arm and force churn.
        assert_eq!(decide(&Prior::Known(""), &"", true), Decision::Skip);
    }
}

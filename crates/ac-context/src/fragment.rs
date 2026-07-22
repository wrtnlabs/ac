//! Fragment classes and the recognition predicate ([docs/ac-context.md] §2).
//! A fragment is a history item the runtime *writes* rather than relays; a
//! fragment class fixes the markers that make it recognizable forever after.

use ac_types::Role;

/// Cadence class `γ` — which driver emits a fragment ([docs/ac-context.md] §4).
/// A class may have *no* cadence: it is recognized and filtered like any
/// fragment, but no cadence driver emits it. That is the home of the runtime's
/// own lifecycle fragments — the compaction handoff, the interruption marker —
/// which other machinery writes and which must not be treated as user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// 𝒲 — per-window. Catalogs and standing instructions: injected once at
    /// window establishment (session start, and after each compaction inside
    /// `context′`), stripped and re-rendered on compaction, never re-emitted
    /// within a window.
    Window,
    /// 𝒯 — per-turn. Mention-selected material: injected in full with one turn's
    /// input, valid for that turn only.
    Turn,
    /// ℛ — reactive. State sections ([`crate::state`]): emitted only on change.
    Reactive,
}

/// A fragment class `φ = (role, (o, c), γ)` plus a body bound. A fragment renders
/// as `o ⧺ body ⧺ c`; the markers are in-band text, which is exactly why
/// recognition survives persistence, replay, and forking (R1).
#[derive(Debug, Clone)]
pub struct FragmentClass {
    /// Stable identifier — diagnostics and dedupe keys, never model-facing.
    pub name: String,
    /// The message role the fragment is emitted under.
    pub role: Role,
    /// Open marker `o` — non-empty, and SHOULD be improbable in organic text
    /// (§3): spoofing resolves toward "treat as context", a bounded loss.
    pub open: String,
    /// Close marker `c` — non-empty.
    pub close: String,
    /// The driver that emits this class, or `None` for a recognized-but-undriven
    /// lifecycle fragment.
    pub cadence: Option<Cadence>,
    /// Max body characters; a longer body is middle-truncated on render (§2).
    pub body_bound: usize,
}

/// A rendered fragment and how many body characters the bound dropped (I4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub truncated_chars: usize,
}

const ELISION: &str = "…[truncated]…";

impl FragmentClass {
    /// Construct a class. The markers MUST be non-empty and free of leading or
    /// trailing whitespace (§2) — both are enforced here rather than trusted:
    ///
    /// - an empty marker makes `marked` vacuously true on that side, so
    ///   `injected(t)` matches *everything* and compaction would drop the whole
    ///   of `U` — a catastrophic, silent R1 violation;
    /// - edge whitespace on a marker makes a class fail to recognize its own
    ///   rendered fragment (`render` preserves the whitespace, `marked` trims the
    ///   text before comparing), breaking I1.
    ///
    /// Both are contributor errors caught loudly at construction — markers are
    /// code-defined constants, so this fires the first time the code runs.
    pub fn new(
        name: impl Into<String>,
        role: Role,
        open: impl Into<String>,
        close: impl Into<String>,
        cadence: Option<Cadence>,
        body_bound: usize,
    ) -> Self {
        let name = name.into();
        let open = open.into();
        let close = close.into();
        assert!(
            !open.is_empty() && !close.is_empty(),
            "fragment class {name:?}: open and close markers must be non-empty (ac-context §2)"
        );
        assert!(
            open == open.trim() && close == close.trim(),
            "fragment class {name:?}: markers must have no leading or trailing whitespace, \
             or a rendered fragment fails to recognize itself (I1)"
        );
        Self {
            name,
            role,
            open,
            close,
            cadence,
            body_bound,
        }
    }

    /// Render `o ⧺ body ⧺ c`, middle-truncating an over-bound body — head and
    /// tail preserved, an elision notice between — and reporting the drop.
    pub fn render(&self, body: &str) -> Rendered {
        let (body, truncated_chars) = middle_truncate(body, self.body_bound);
        Rendered {
            text: format!("{}{}{}", self.open, body, self.close),
            truncated_chars,
        }
    }

    /// `marked_φ(t)`: the left-trimmed text opens with `o` and the trimmed text
    /// closes with `c`, ASCII-case-insensitively (§2). The recognition predicate
    /// for this one class.
    pub fn marked(&self, text: &str) -> bool {
        ci_starts_with(text.trim_start(), &self.open) && ci_ends_with(text.trim(), &self.close)
    }
}

/// Keep head and tail, drop the middle, so a bounded body preserves both ends.
/// Returns the possibly-shortened body and the number of characters removed.
fn middle_truncate(body: &str, bound: usize) -> (String, usize) {
    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= bound {
        return (body.to_string(), 0);
    }
    let elision_len = ELISION.chars().count();
    // Too small to keep both ends around the full elision, but still signal the
    // cut in-band with a single ellipsis so a truncated body never looks whole.
    if bound <= elision_len {
        if bound == 0 {
            return (String::new(), chars.len());
        }
        let keep = bound - 1; // room for the ellipsis
        let head: String = chars.iter().take(keep).collect();
        return (format!("{head}…"), chars.len() - keep);
    }
    let keep = bound - elision_len;
    let head_len = keep.div_ceil(2);
    let tail_len = keep - head_len;
    let head: String = chars.iter().take(head_len).collect();
    let tail: String = chars.iter().skip(chars.len() - tail_len).collect();
    let dropped = chars.len() - head_len - tail_len;
    (format!("{head}{ELISION}{tail}"), dropped)
}

/// ASCII-case-insensitive prefix test. Markers are ASCII; a byte-slice compare
/// is safe (a UTF-8 continuation byte never equals an ASCII byte).
fn ci_starts_with(hay: &str, prefix: &str) -> bool {
    hay.len() >= prefix.len()
        && hay.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn ci_ends_with(hay: &str, suffix: &str) -> bool {
    hay.len() >= suffix.len()
        && hay.as_bytes()[hay.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class() -> FragmentClass {
        FragmentClass::new(
            "test",
            Role::User,
            "<<CTX",
            "CTX>>",
            Some(Cadence::Window),
            50,
        )
    }

    #[test]
    fn render_wraps_and_recognizes_round_trip() {
        let c = class();
        let r = c.render("hello");
        assert_eq!(r.text, "<<CTXhelloCTX>>");
        assert_eq!(r.truncated_chars, 0);
        // I1: every rendered fragment is recognized by its class.
        assert!(c.marked(&r.text));
    }

    #[test]
    fn recognition_trims_and_ignores_ascii_case() {
        let c = class();
        assert!(
            c.marked("  \n<<ctxbodyCTX>>\n  "),
            "trimmed + case-insensitive"
        );
        assert!(!c.marked("no markers here"));
        assert!(!c.marked("<<CTX opens but no close"));
    }

    #[test]
    fn oversized_body_is_middle_truncated_and_reported() {
        let mut c = class();
        c.body_bound = 20;
        let body = "A".repeat(30) + &"Z".repeat(30);
        let r = c.render(&body);
        assert!(r.truncated_chars > 0, "the drop is reported (I4)");
        assert!(r.text.starts_with("<<CTXA"), "head preserved");
        assert!(r.text.ends_with("ZCTX>>"), "tail preserved");
        assert!(r.text.contains("truncated"), "elision notice present");
        // Still recognized after truncation.
        assert!(c.marked(&r.text));
    }

    #[test]
    fn a_tiny_bound_keeps_a_head_prefix_with_an_ellipsis() {
        let mut c = class();
        c.body_bound = 3;
        let r = c.render("abcdefgh");
        // Two head chars + an ellipsis: bound honored, and the cut is visible.
        assert_eq!(r.text, "<<CTXab…CTX>>");
        assert_eq!(r.truncated_chars, 6);
        assert!(c.marked(&r.text));
    }

    #[test]
    #[should_panic(expected = "non-empty")]
    fn empty_markers_are_rejected() {
        FragmentClass::new("bad", Role::User, "", "", None, 10);
    }

    #[test]
    #[should_panic(expected = "whitespace")]
    fn edge_whitespace_markers_are_rejected() {
        FragmentClass::new("bad", Role::User, "\n[x]", "[/x]", None, 10);
    }
}

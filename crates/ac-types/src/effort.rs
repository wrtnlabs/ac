//! The agnostic reasoning-effort tier ([docs/ac-ultra.md] §3) — the model dial.
//!
//! A reasoning model spends hidden reasoning tokens before its answer; effort
//! biases how much. This is the provider-agnostic tier; a wire crate maps it to
//! the provider's reasoning control, and a provider with no such control ignores
//! it — a **hint**, not a capability handshake. There is deliberately no "ultra"
//! tier: "ultra" is a harness composition ([docs/ac-ultra.md] §5), and its model
//! dial is [`Effort::Max`].

use serde::{Deserialize, Serialize};

/// A reasoning-effort tier, low → max. `Max` is the top *model* value; the wire
/// crate collapses it to a provider's strongest exposed level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    /// Parse a lowercase tier name; unknown strings yield `None` so a bad input
    /// is ignored (treated as "no override") rather than faulting.
    pub fn parse(s: &str) -> Option<Effort> {
        match s {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for e in [Effort::Low, Effort::Medium, Effort::High, Effort::Max] {
            assert_eq!(Effort::parse(e.as_str()), Some(e));
        }
        assert_eq!(Effort::parse("ultra"), None); // no "ultra" model tier
        assert_eq!(Effort::parse("HIGH"), None); // lowercase only
    }

    #[test]
    fn serde_is_lowercase() {
        assert_eq!(serde_json::to_string(&Effort::Max).unwrap(), "\"max\"");
        assert_eq!(
            serde_json::from_str::<Effort>("\"low\"").unwrap(),
            Effort::Low
        );
    }
}

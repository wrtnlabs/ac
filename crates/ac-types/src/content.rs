use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    /// Marks a prompt-cache breakpoint after this message (Anthropic
    /// `cache_control` via OpenRouter). The wire crate decides the encoding.
    #[serde(default, skip_serializing_if = "CacheMark::is_off")]
    pub cache: CacheMark,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text { text: text.into() }],
            cache: CacheMark::Off,
        }
    }
}

/// A prompt-cache breakpoint mark. `On` uses the provider's default TTL;
/// `WithTtl` pins an explicit one.
///
/// Wire compatibility is load-bearing: this field was historically a bare
/// bool, and persisted logs carry `true`/`false` or omit it entirely. `Off`
/// and `On` therefore still serialize as the legacy bool (and `Off` is
/// skipped), while a TTL serializes as its string form (`"5m"`, `"1h"`); all
/// of those deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMark {
    #[default]
    Off,
    On,
    WithTtl(CacheTtl),
}

impl CacheMark {
    pub fn is_on(&self) -> bool {
        !self.is_off()
    }

    pub fn is_off(&self) -> bool {
        matches!(self, CacheMark::Off)
    }

    pub fn ttl(&self) -> Option<CacheTtl> {
        match self {
            CacheMark::WithTtl(ttl) => Some(*ttl),
            _ => None,
        }
    }
}

impl From<bool> for CacheMark {
    fn from(marked: bool) -> Self {
        if marked {
            CacheMark::On
        } else {
            CacheMark::Off
        }
    }
}

impl Serialize for CacheMark {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            CacheMark::Off => serializer.serialize_bool(false),
            CacheMark::On => serializer.serialize_bool(true),
            CacheMark::WithTtl(ttl) => ttl.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CacheMark {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Flag(bool),
            Ttl(CacheTtl),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Flag(false) => CacheMark::Off,
            Wire::Flag(true) => CacheMark::On,
            Wire::Ttl(ttl) => CacheMark::WithTtl(ttl),
        })
    }
}

/// Explicit prompt-cache TTL — the durations providers accept today. The wire
/// crate encodes the string form into `cache_control.ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::OneHour => "1h",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    Image {
        media_type: String,
        /// Base64-encoded image bytes.
        data: String,
    },
    ToolUse(ToolUse),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The legacy persisted forms — a bool `cache` or no field at all — must
    // keep deserializing forever: existing stores hold them.
    #[test]
    fn legacy_bool_cache_field_still_deserializes() {
        let m: Message = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"}],"cache":true}"#,
        )
        .unwrap();
        assert_eq!(m.cache, CacheMark::On);

        let m: Message =
            serde_json::from_str(r#"{"role":"user","content":[],"cache":false}"#).unwrap();
        assert_eq!(m.cache, CacheMark::Off);

        let m: Message = serde_json::from_str(r#"{"role":"user","content":[]}"#).unwrap();
        assert_eq!(m.cache, CacheMark::Off, "a missing field means no mark");
    }

    // The no-TTL cases serialize exactly as before the widening, so existing
    // fixtures and stores stay byte-stable.
    #[test]
    fn no_ttl_marks_serialize_as_the_legacy_bool() {
        let mut m = Message::text(Role::User, "hi");
        m.cache = CacheMark::On;
        assert!(
            serde_json::to_string(&m)
                .unwrap()
                .contains(r#""cache":true"#)
        );

        m.cache = CacheMark::Off;
        assert!(
            !serde_json::to_string(&m).unwrap().contains("cache"),
            "an unmarked message omits the field, as the bool form did"
        );
    }

    #[test]
    fn every_cache_mark_form_round_trips() {
        for mark in [
            CacheMark::Off,
            CacheMark::On,
            CacheMark::WithTtl(CacheTtl::FiveMinutes),
            CacheMark::WithTtl(CacheTtl::OneHour),
        ] {
            let mut m = Message::text(Role::User, "hi");
            m.cache = mark;
            let json = serde_json::to_string(&m).unwrap();
            let back: Message = serde_json::from_str(&json).unwrap();
            assert_eq!(back.cache, mark, "round-trip of {json}");
        }
        let mut m = Message::text(Role::User, "hi");
        m.cache = CacheMark::WithTtl(CacheTtl::OneHour);
        assert!(
            serde_json::to_string(&m)
                .unwrap()
                .contains(r#""cache":"1h""#),
            "a TTL mark serializes as its duration string"
        );
    }
}

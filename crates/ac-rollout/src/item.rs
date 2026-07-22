//! The log's event vocabulary. Every session is recorded as a sequence of
//! these, append-only ([docs/ac-fork.md] §2).

use ac_types::Message;
use serde::{Deserialize, Serialize};

/// A session identity — globally unique, time-ordered (a UUIDv7 by default).
pub type SessionId = String;

/// The metadata head of a log: identity `ι` and lineage `λ`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    /// The session this was forked from, if any. Makes ancestry a queryable
    /// chain; `None` for a root session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<SessionId>,
    pub created_at_ms: i64,
}

/// One event `eᵢ` in a session log. Ordinary items accumulate into the
/// effective history; the two marker items transform it without mutating any
/// prior event (the append-only axiom A1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RolloutItem {
    /// A metadata head. The *first* in a log is canonical; later heads — which
    /// a fork copies into its prefix — are inert data on replay.
    Meta(SessionMeta),
    /// A conversation item: what the model is given.
    Message(Message),
    /// A turn began. Its number identifies the turn as a fork cut point.
    TurnStarted { turn: u64 },
    /// A turn completed. Only completed turns are canonical cut points.
    TurnEnded { turn: u64 },
    /// A compaction record `κ(H′)`: the handoff summary and the replacement
    /// history that becomes the effective view from here
    /// ([docs/ac-compaction.md]).
    Compacted {
        summary: String,
        replacement: Vec<Message>,
    },
    /// A rewind marker `ρ(k)`: the last `turns` turns are dropped from the
    /// effective view. The removed lines physically remain in the log.
    RolledBack { turns: u64 },
}

/// A timestamped log line. The timestamp is audit metadata — the projection
/// and fork logic ignore it entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutLine {
    pub at_ms: i64,
    #[serde(flatten)]
    pub item: RolloutItem,
}

impl RolloutItem {
    pub(crate) fn message(msg: Message) -> Self {
        RolloutItem::Message(msg)
    }
}

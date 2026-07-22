//! The session log substrate — [docs/ac-fork.md]. A session is an append-only
//! log of type-tagged events; every derived view, including "what the model
//! currently sees", is a pure function of that log. Mutation is replaced by
//! appending semantic markers; branching by prefix duplication under a new
//! identity.
//!
//! This crate is the substrate the fork/rewind operations and the compaction
//! record ride on. It knows nothing of the loop or a provider — a host records
//! events as a turn produces them, and reads the projection back.

mod item;

use std::path::Path;

use ac_types::{INTERRUPTION_MARKER, Message, Role};

pub use item::{RolloutItem, RolloutLine, SessionId, SessionMeta};

/// A fork boundary. Forking is permitted only at a canonical cut point
/// ([docs/ac-fork.md] §4.1): the start of a completed turn, or the end of the
/// log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cut {
    /// Fork excluding turn `n` and everything after it. `n` must name a
    /// completed turn.
    BeforeTurn(u64),
    /// Fork the whole log (branch the complete session).
    End,
}

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("no completed turn numbered {0} in the log")]
    UnknownTurn(u64),
    #[error("turn {0} is still in progress and cannot be a fork boundary")]
    TurnInProgress(u64),
}

#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("cannot rewind while a turn is in progress")]
    TurnInProgress,
    #[error("cannot rewind zero turns")]
    Zero,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log has no metadata head")]
    NoHead,
}

/// A log loaded from disk, with a count of individually-corrupt lines that were
/// skipped (never a reason to abort the whole session).
#[derive(Debug)]
pub struct Loaded {
    pub rollout: Rollout,
    pub skipped_lines: usize,
}

/// An append-only session log. `lines[0]` is always the canonical metadata
/// head.
#[derive(Debug, Clone)]
pub struct Rollout {
    lines: Vec<RolloutLine>,
}

impl Rollout {
    /// Start a fresh root log with a minted, time-ordered identity.
    pub fn create() -> Self {
        Self::create_with_id(new_id())
    }

    /// Start a fresh root log under a caller-chosen identity (a client-minted
    /// session id, an adopted external id).
    pub fn create_with_id(id: impl Into<String>) -> Self {
        let created_at_ms = now_ms();
        Self::with_head(SessionMeta {
            id: id.into(),
            forked_from: None,
            created_at_ms,
        })
    }

    fn with_head(meta: SessionMeta) -> Self {
        let at_ms = meta.created_at_ms;
        Self {
            lines: vec![RolloutLine {
                at_ms,
                item: RolloutItem::Meta(meta),
            }],
        }
    }

    /// The canonical identity — the *first* metadata head, so a fork's copied
    /// source head (a later line) is inert.
    pub fn id(&self) -> &str {
        for line in &self.lines {
            if let RolloutItem::Meta(m) = &line.item {
                return &m.id;
            }
        }
        unreachable!("a rollout always has a head")
    }

    /// The identity this log was forked from, if any (its lineage).
    pub fn forked_from(&self) -> Option<&str> {
        for line in &self.lines {
            if let RolloutItem::Meta(m) = &line.item {
                return m.forked_from.as_deref();
            }
        }
        None
    }

    /// The raw event sequence, in order (the canonical head first).
    pub fn items(&self) -> impl Iterator<Item = &RolloutItem> {
        self.lines.iter().map(|l| &l.item)
    }

    /// Append an event, stamped now. Hosts SHOULD prefer the typed recorders
    /// below; this is the general seam.
    pub fn append(&mut self, item: RolloutItem) {
        self.lines.push(RolloutLine {
            at_ms: now_ms(),
            item,
        });
    }

    /// Record a conversation item.
    pub fn record_message(&mut self, msg: Message) {
        self.append(RolloutItem::message(msg));
    }

    /// Mark the start of turn `n`.
    pub fn start_turn(&mut self, n: u64) {
        self.append(RolloutItem::TurnStarted { turn: n });
    }

    /// Mark the completion of turn `n`.
    pub fn end_turn(&mut self, n: u64) {
        self.append(RolloutItem::TurnEnded { turn: n });
    }

    /// Record a compaction: the effective view becomes `replacement` from here.
    pub fn compact(&mut self, summary: impl Into<String>, replacement: Vec<Message>) {
        self.append(RolloutItem::Compacted {
            summary: summary.into(),
            replacement,
        });
    }

    /// Rewind the effective view by `turns` turns (append a rewind marker). The
    /// removed lines stay in the log; only the projection changes. Refused
    /// while a turn is in progress — the view must not shift under a running
    /// computation ([docs/ac-fork.md] §3).
    pub fn rewind(&mut self, turns: u64) -> Result<(), RewindError> {
        if turns == 0 {
            return Err(RewindError::Zero);
        }
        if self.open_turn().is_some() {
            return Err(RewindError::TurnInProgress);
        }
        self.append(RolloutItem::RolledBack { turns });
        Ok(())
    }

    /// The effective history `E(L)`: the messages the model would be given if a
    /// turn started now. A pure fold — markers transform the accumulation
    /// ([docs/ac-fork.md] §3, I1).
    pub fn project(&self) -> Vec<Message> {
        self.fold().messages
    }

    /// The turn currently in progress (started, not yet ended), if any.
    pub fn open_turn(&self) -> Option<u64> {
        let mut open = None;
        for item in self.items() {
            match item {
                RolloutItem::TurnStarted { turn } => open = Some(*turn),
                RolloutItem::TurnEnded { turn } if open == Some(*turn) => open = None,
                _ => {}
            }
        }
        open
    }

    /// The completed turns' numbers, in log order — the fork cut points
    /// (besides [`Cut::End`]).
    pub fn cut_turns(&self) -> Vec<u64> {
        self.completed_turns().into_iter().map(|(n, _)| n).collect()
    }

    /// Fork at a canonical cut point into a new log with a fresh identity and
    /// this log as its lineage. The source is never mutated (I3); the new log's
    /// head plus the copied source prefix are one value the caller persists
    /// atomically (I4).
    pub fn fork(&self, cut: Cut, new_id: Option<String>) -> Result<Rollout, ForkError> {
        let prefix_end = match &cut {
            Cut::End => self.lines.len(),
            Cut::BeforeTurn(n) => {
                let (_, start_idx, completed) =
                    self.turn_span(*n).ok_or(ForkError::UnknownTurn(*n))?;
                if !completed {
                    return Err(ForkError::TurnInProgress(*n));
                }
                start_idx
            }
        };

        let created_at_ms = now_ms();
        let head = SessionMeta {
            id: new_id.unwrap_or_else(new_id_string),
            forked_from: Some(self.id().to_string()),
            created_at_ms,
        };
        let mut lines = Vec::with_capacity(prefix_end + 1);
        lines.push(RolloutLine {
            at_ms: created_at_ms,
            // The new head is canonical; the source head, copied inside the
            // prefix below, becomes an inert mid-file line.
            item: RolloutItem::Meta(head),
        });
        lines.extend_from_slice(&self.lines[..prefix_end]);
        let mut forked = Rollout { lines };

        // Ragged edge (I6): a prefix that ends mid-turn gets the same
        // deliberate-interruption marker a live cancellation records, and the
        // open turn is closed — the branch sees an intentional cut, and the new
        // log is well-formed (no dangling open turn).
        if let Some(open) = forked.open_turn() {
            forked.record_message(Message::text(Role::User, INTERRUPTION_MARKER));
            forked.end_turn(open);
        }
        Ok(forked)
    }

    /// Serialize to newline-delimited JSON — one line per event.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            // A RolloutLine always serializes (plain data); unwrap is sound.
            out.push_str(&serde_json::to_string(line).expect("rollout line serializes"));
            out.push('\n');
        }
        out
    }

    /// Parse newline-delimited JSON, tolerating individually-corrupt lines
    /// (skipped and counted) and taking the first metadata head as canonical.
    pub fn from_jsonl(text: &str) -> Result<Loaded, LoadError> {
        let mut lines = Vec::new();
        let mut skipped = 0usize;
        for raw in text.lines() {
            if raw.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<RolloutLine>(raw) {
                Ok(line) => lines.push(line),
                Err(_) => skipped += 1,
            }
        }
        let has_head = lines.iter().any(|l| matches!(l.item, RolloutItem::Meta(_)));
        if !has_head {
            return Err(LoadError::NoHead);
        }
        // The canonical head must be the first line so `id()` is unambiguous.
        if !matches!(lines[0].item, RolloutItem::Meta(_)) {
            return Err(LoadError::NoHead);
        }
        Ok(Loaded {
            rollout: Rollout { lines },
            skipped_lines: skipped,
        })
    }

    /// Write the log to `path` atomically (temp file in the same directory,
    /// then rename) — the atomic-birth guarantee for a fork (I4), and safe for
    /// any save. Content is append-only in effect: no prior line ever changes.
    pub fn write(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, self.to_jsonl())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read a log written by [`write`](Self::write).
    pub fn read(path: impl AsRef<Path>) -> Result<Loaded, LoadError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_jsonl(&text)
    }

    // --- internals ---

    /// One fold pass yielding the effective messages plus the turn-start
    /// positions currently in view (used for rewind).
    fn fold(&self) -> Fold {
        let mut messages: Vec<Message> = Vec::new();
        // (turn number, index into `messages`) for turns still in the view.
        let mut turn_starts: Vec<usize> = Vec::new();
        for item in self.items() {
            match item {
                RolloutItem::Meta(_) => {}
                RolloutItem::Message(m) => messages.push(m.clone()),
                RolloutItem::TurnStarted { .. } => turn_starts.push(messages.len()),
                RolloutItem::TurnEnded { .. } => {}
                RolloutItem::Compacted { replacement, .. } => {
                    // The replacement is a fresh baseline with no internal turn
                    // boundaries; a later TurnStarted adds the next.
                    messages = replacement.clone();
                    turn_starts.clear();
                }
                RolloutItem::RolledBack { turns } => {
                    let n = turn_starts.len();
                    let keep = n.saturating_sub(*turns as usize);
                    let cut = if keep < n {
                        turn_starts[keep]
                    } else {
                        messages.len()
                    };
                    messages.truncate(cut);
                    turn_starts.truncate(keep);
                }
            }
        }
        Fold { messages }
    }

    /// All completed turns as (number, start line index), in log order.
    fn completed_turns(&self) -> Vec<(u64, usize)> {
        let mut open: Option<(u64, usize)> = None;
        let mut done = Vec::new();
        for (idx, item) in self.lines.iter().enumerate() {
            match &item.item {
                RolloutItem::TurnStarted { turn } => open = Some((*turn, idx)),
                RolloutItem::TurnEnded { turn } => {
                    if let Some((n, start)) = open
                        && n == *turn
                    {
                        done.push((n, start));
                        open = None;
                    }
                }
                _ => {}
            }
        }
        done
    }

    /// (turn number, start line index, completed?) for turn `n`, or None if it
    /// never started.
    fn turn_span(&self, n: u64) -> Option<(u64, usize, bool)> {
        let mut start: Option<usize> = None;
        let mut completed = false;
        for (idx, line) in self.lines.iter().enumerate() {
            match &line.item {
                RolloutItem::TurnStarted { turn } if *turn == n => start = Some(idx),
                RolloutItem::TurnEnded { turn } if *turn == n && start.is_some() => {
                    completed = true;
                }
                _ => {}
            }
        }
        start.map(|s| (n, s, completed))
    }
}

struct Fold {
    messages: Vec<Message>,
}

fn new_id() -> String {
    new_id_string()
}

fn new_id_string() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Wall-clock ms, forced strictly monotonic per process so appends within the
/// same millisecond still order deterministically.
fn now_ms() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = wall.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

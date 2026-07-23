//! Torn-tail proofs of the log's reopen contract ([docs/ac-durability.md] §6,
//! 5.5): every byte-length prefix of a representative log loads without panic
//! into a healed, prefix-consistent view, with loud accounting (R3, R4, I6).

use ac_rollout::{Rollout, RolloutItem};
use ac_types::{ContentPart, INTERRUPTION_MARKER, Message, Role};

fn user(t: &str) -> Message {
    Message::text(Role::User, t)
}
fn assistant(t: &str) -> Message {
    Message::text(Role::Assistant, t)
}

fn is_marker(item: &RolloutItem) -> bool {
    matches!(item, RolloutItem::Message(m)
        if matches!(&m.content[0], ContentPart::Text { text } if text == INTERRUPTION_MARKER))
}

/// One of every event kind the log can hold: the meta head, two completed
/// turns around a compaction, a rewind, and a trailing OPEN turn.
fn representative_log() -> Rollout {
    let mut log = Rollout::create();
    log.start_turn(1);
    log.record_message(user("q1"));
    log.record_message(assistant("a1"));
    log.end_turn(1);
    log.compact("handoff", "manual", vec![user("q1"), assistant("SUMMARY")]);
    log.start_turn(2);
    log.record_message(user("q2"));
    log.record_message(assistant("a2"));
    log.end_turn(2);
    log.rewind(1).unwrap();
    log.start_turn(3);
    log.record_message(user("q3"));
    log
}

/// The open turn implied by a raw item prefix — the ground truth the loader's
/// healing must agree with at every cut.
fn open_turn_of(items: &[RolloutItem]) -> Option<u64> {
    let mut open = None;
    for item in items {
        match item {
            RolloutItem::TurnStarted { turn } => open = Some(*turn),
            RolloutItem::TurnEnded { turn } if open == Some(*turn) => open = None,
            _ => {}
        }
    }
    open
}

/// The full sweep of 5.5: load every byte prefix from empty to the whole file.
/// A cut short of a complete head is a typed refusal; everything past it loads
/// into exactly the fully-contained lines (plus at most the healing suffix),
/// with `skipped_lines` <= 1 and exactly 0 on a line boundary.
#[test]
fn every_byte_prefix_loads_healed_and_consistent() {
    let log = representative_log();
    let text = log.to_jsonl();
    assert!(text.is_ascii(), "the sweep slices at raw byte offsets");
    let full_items: Vec<RolloutItem> = log.items().cloned().collect();

    // Byte offset where each line's content ends (exclusive of its '\n').
    let mut content_ends = Vec::new();
    let mut start = 0usize;
    for line in text.split_inclusive('\n') {
        content_ends.push(start + line.trim_end_matches('\n').len());
        start += line.len();
    }
    let head_end = content_ends[0];

    for i in 0..=text.len() {
        let prefix = &text[..i];
        let result = Rollout::from_jsonl(prefix);
        if i < head_end {
            assert!(result.is_err(), "prefix {i}: no complete head yet");
            continue;
        }
        let loaded = result.unwrap_or_else(|e| panic!("prefix {i}: load failed: {e}"));

        // Prefix consistency: exactly the lines wholly inside the cut, in
        // order, plus (at most) the two healing lines.
        let parsed = content_ends.iter().filter(|&&end| end <= i).count();
        let items: Vec<RolloutItem> = loaded.rollout.items().cloned().collect();
        let healed_len = if loaded.healed_open_turn { 2 } else { 0 };
        assert_eq!(items.len(), parsed + healed_len, "prefix {i}: line count");
        assert_eq!(
            &items[..parsed],
            &full_items[..parsed],
            "prefix {i}: content"
        );

        // Healing fires exactly when the cut lands inside an open turn, and
        // appends the marker plus that turn's closure.
        let open_at_cut = open_turn_of(&full_items[..parsed]);
        assert_eq!(
            loaded.healed_open_turn,
            open_at_cut.is_some(),
            "prefix {i}: healing flag"
        );
        if let Some(turn) = open_at_cut {
            assert!(is_marker(&items[parsed]), "prefix {i}: marker appended");
            assert!(
                matches!(items[parsed + 1], RolloutItem::TurnEnded { turn: t } if t == turn),
                "prefix {i}: open turn {turn} closed"
            );
        }
        assert!(
            loaded.rollout.open_turn().is_none(),
            "prefix {i}: the healed view never dangles an open turn"
        );

        // Loud accounting: at most the one cut line, none on a line boundary.
        assert!(loaded.skipped_lines <= 1, "prefix {i}: skipped_lines");
        let on_boundary = i == text.len() || prefix.as_bytes()[i - 1] == b'\n';
        if on_boundary {
            assert_eq!(loaded.skipped_lines, 0, "prefix {i}: clean cut skips none");
        }

        // E(L) and the derived views stay callable on every prefix.
        let _ = loaded.rollout.project();
        let _ = loaded.rollout.cut_turns();
        assert_eq!(loaded.rollout.id(), log.id(), "prefix {i}: identity");
    }
}

/// Pins the open-turn-closure behavior on the FULL text ([docs/ac-durability.md]
/// §4 "log healing"): a log ending inside turn 3 loads with the interruption
/// marker appended, the turn closed, and the healing reported.
#[test]
fn load_closes_a_trailing_open_turn_with_the_interruption_marker() {
    let log = representative_log();
    let loaded = Rollout::from_jsonl(&log.to_jsonl()).unwrap();

    assert!(
        loaded.healed_open_turn,
        "healing must be reported, not silent"
    );
    assert_eq!(loaded.skipped_lines, 0);
    let view = loaded.rollout.project();
    let last = view.last().expect("a non-empty view");
    assert!(
        matches!(&last.content[0], ContentPart::Text { text } if text == INTERRUPTION_MARKER),
        "the view must end with the interruption marker"
    );
    assert!(loaded.rollout.open_turn().is_none());
    // The healed turn is a completed (interrupted) turn, like fork's ragged edge.
    assert_eq!(loaded.rollout.cut_turns(), vec![1, 2, 3]);
}

#[test]
fn a_clean_log_loads_without_healing() {
    let mut log = Rollout::create();
    log.start_turn(1);
    log.record_message(user("q1"));
    log.record_message(assistant("a1"));
    log.end_turn(1);
    let jsonl = log.to_jsonl();

    let loaded = Rollout::from_jsonl(&jsonl).unwrap();
    assert!(!loaded.healed_open_turn);
    assert_eq!(loaded.skipped_lines, 0);
    assert_eq!(loaded.rollout.to_jsonl(), jsonl, "clean loads round-trip");
}

/// Recovery is a fixed point (R4/I4): loading a healed log's serialization
/// heals nothing further and changes nothing.
#[test]
fn healing_is_a_fixed_point() {
    let log = representative_log();
    let once = Rollout::from_jsonl(&log.to_jsonl()).unwrap();
    assert!(once.healed_open_turn);

    let jsonl = once.rollout.to_jsonl();
    let twice = Rollout::from_jsonl(&jsonl).unwrap();
    assert!(!twice.healed_open_turn, "the healed log is already closed");
    assert_eq!(twice.skipped_lines, 0);
    assert_eq!(twice.rollout.to_jsonl(), jsonl);
}

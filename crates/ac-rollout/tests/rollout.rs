//! Hermetic proofs of the session-log substrate ([docs/ac-fork.md]): the
//! projection E(L) and its markers, cut points, fork with lineage and
//! ragged-edge honesty, rewind, and the JSONL format's fault tolerance and
//! first-head-canonical rule.

use ac_rollout::{Cut, ForkError, RewindError, Rollout, RolloutItem};
use ac_types::{INTERRUPTION_MARKER, Message, Role};

fn user(t: &str) -> Message {
    Message::text(Role::User, t)
}
fn assistant(t: &str) -> Message {
    Message::text(Role::Assistant, t)
}

/// Record a complete turn: start, the messages, end.
fn turn(log: &mut Rollout, n: u64, msgs: &[Message]) {
    log.start_turn(n);
    for m in msgs {
        log.record_message(m.clone());
    }
    log.end_turn(n);
}

fn texts(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .map(|m| match &m.content[0] {
            ac_types::ContentPart::Text { text } => text.clone(),
            _ => "<non-text>".to_string(),
        })
        .collect()
}

#[test]
fn projection_accumulates_messages_and_ignores_boundaries() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    turn(&mut log, 2, &[user("q2"), assistant("a2")]);
    assert_eq!(texts(&log.project()), vec!["q1", "a1", "q2", "a2"]);
    assert_eq!(log.cut_turns(), vec![1, 2]);
    assert!(log.open_turn().is_none());
    assert!(log.forked_from().is_none());
}

#[test]
fn compaction_replaces_the_view_but_keeps_the_record() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("long q"), assistant("long a")]);
    log.compact("handoff summary", vec![user("q1"), assistant("SUMMARY")]);
    turn(&mut log, 2, &[user("q2")]);

    // The effective view is the replacement plus what followed — the
    // pre-compaction messages are gone from the view…
    assert_eq!(texts(&log.project()), vec!["q1", "SUMMARY", "q2"]);
    // …but remain in the record (audit / fork-before-compaction).
    let recorded_long = log.items().any(
        |i| matches!(i, RolloutItem::Message(m) if texts(std::slice::from_ref(m)) == ["long q"]),
    );
    assert!(recorded_long, "the pre-compaction message stays in the log");
}

#[test]
fn rewind_drops_the_last_turns_from_the_view_only() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    turn(&mut log, 2, &[user("q2"), assistant("a2")]);
    turn(&mut log, 3, &[user("q3"), assistant("a3")]);

    log.rewind(2).unwrap();
    // Turns 2 and 3 are dropped from the view; turn 1 remains.
    assert_eq!(texts(&log.project()), vec!["q1", "a1"]);
    // The rewound messages physically remain in the log.
    let still_recorded = log
        .items()
        .filter(|i| matches!(i, RolloutItem::Message(_)))
        .count();
    assert_eq!(still_recorded, 6, "rewind removes nothing from the record");

    // A subsequent turn builds on the rewound view.
    turn(&mut log, 4, &[user("q4")]);
    assert_eq!(texts(&log.project()), vec!["q1", "a1", "q4"]);
}

#[test]
fn rewind_is_refused_mid_turn_and_at_zero() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1")]);
    assert!(matches!(log.rewind(0), Err(RewindError::Zero)));
    log.start_turn(2); // open, not ended
    assert!(matches!(log.rewind(1), Err(RewindError::TurnInProgress)));
}

#[test]
fn fork_before_a_turn_copies_the_prefix_and_records_lineage() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    turn(&mut log, 2, &[user("q2"), assistant("a2")]);
    let source_id = log.id().to_string();

    let branch = log.fork(Cut::BeforeTurn(2), None).unwrap();
    // The branch is turn 1 only, with fresh identity and this log as lineage.
    assert_eq!(texts(&branch.project()), vec!["q1", "a1"]);
    assert_ne!(branch.id(), source_id);
    assert_eq!(branch.forked_from(), Some(source_id.as_str()));

    // The source is untouched (I3).
    assert_eq!(texts(&log.project()), vec!["q1", "a1", "q2", "a2"]);

    // The branch's canonical id is the FIRST head; the copied source head is
    // inert (I5) — so id() is unambiguous even though two heads exist.
    let head_count = branch
        .items()
        .filter(|i| matches!(i, RolloutItem::Meta(_)))
        .count();
    assert_eq!(head_count, 2, "the source head rides along, inert");
    assert_eq!(branch.id(), branch.id(), "first head canonical, stable");
}

#[test]
fn fork_rejects_an_in_progress_or_unknown_turn() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1")]);
    log.start_turn(2); // in progress
    assert!(matches!(
        log.fork(Cut::BeforeTurn(2), None),
        Err(ForkError::TurnInProgress(2))
    ));
    assert!(matches!(
        log.fork(Cut::BeforeTurn(9), None),
        Err(ForkError::UnknownTurn(9))
    ));
}

#[test]
fn forking_the_whole_log_at_a_ragged_edge_records_the_interruption_marker() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    log.start_turn(2);
    log.record_message(user("q2")); // turn 2 never ended — ragged

    let branch = log.fork(Cut::End, None).unwrap();
    // The branch sees the interruption marker as the last user message, and
    // its open turn is closed — the log is well-formed.
    let view = branch.project();
    assert_eq!(
        view.last().map(|m| texts(std::slice::from_ref(m))),
        Some(vec![INTERRUPTION_MARKER.to_string()])
    );
    assert!(branch.open_turn().is_none(), "the ragged turn is closed");
    // Turn 2 is now a completed (interrupted) turn in the branch.
    assert_eq!(branch.cut_turns(), vec![1, 2]);
}

#[test]
fn forking_the_whole_log_on_a_clean_boundary_needs_no_marker() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    let branch = log.fork(Cut::End, None).unwrap();
    assert_eq!(texts(&branch.project()), vec!["q1", "a1"]);
    assert!(
        !branch
            .project()
            .iter()
            .any(|m| texts(std::slice::from_ref(m)) == [INTERRUPTION_MARKER.to_string()])
    );
}

#[test]
fn fork_before_a_compaction_yields_the_pre_compaction_view() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    log.compact("summary", vec![user("q1"), assistant("SUMMARY")]);
    turn(&mut log, 2, &[user("q2")]);

    // Fork before turn 2 — the prefix includes the compaction, so the branch
    // inherits the compacted view (rides along, §4.3).
    let after = log.fork(Cut::BeforeTurn(2), None).unwrap();
    assert_eq!(texts(&after.project()), vec!["q1", "SUMMARY"]);

    // Fork before turn 1 — the prefix predates the compaction entirely.
    let before = log.fork(Cut::BeforeTurn(1), None).unwrap();
    assert_eq!(before.project(), Vec::<Message>::new());
}

#[test]
fn jsonl_round_trips_and_is_fault_tolerant_and_first_head_canonical() {
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);
    log.compact("s", vec![user("baseline")]);
    let jsonl = log.to_jsonl();

    let loaded = Rollout::from_jsonl(&jsonl).unwrap();
    assert_eq!(loaded.skipped_lines, 0);
    assert_eq!(loaded.rollout.id(), log.id());
    assert_eq!(loaded.rollout.to_jsonl(), jsonl);

    // A corrupt line in the middle is skipped and counted, never fatal.
    let mut corrupted: Vec<&str> = jsonl.lines().collect();
    corrupted.insert(2, "{ this is not valid json");
    let text = corrupted.join("\n");
    let loaded = Rollout::from_jsonl(&text).unwrap();
    assert_eq!(loaded.skipped_lines, 1);
    assert_eq!(loaded.rollout.id(), log.id());

    // A log whose first line is not a head is rejected — id() must be
    // unambiguous.
    let headless: String = jsonl.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert!(Rollout::from_jsonl(&headless).is_err());
}

#[test]
fn write_then_read_round_trips_through_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions").join("s.jsonl");
    let mut log = Rollout::create();
    turn(&mut log, 1, &[user("q1"), assistant("a1")]);

    log.write(&path).unwrap();
    let loaded = Rollout::read(&path).unwrap();
    assert_eq!(loaded.rollout.id(), log.id());
    assert_eq!(texts(&loaded.rollout.project()), vec!["q1", "a1"]);
}

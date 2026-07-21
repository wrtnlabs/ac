//! Offline proof of the codex-mirror skills surface through the SHIPPED
//! wiring (`ac_cli::build_host` + `compose_turn_input`): the skills root is
//! readable from the start (companion scripts/references are read by the
//! model itself at the paths the catalog and body carry), never writable,
//! and a `$mention` injects the SKILL.md body as turn-input text — there is
//! no skill tool anywhere. Hermetic — MockProvider, temp dirs, no network,
//! no sandbox.

use std::path::Path;
use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentEvent, Session};
use ac_types::StopReason;
use serde_json::json;

async fn run(mut session: Session, prompt: &str) -> (StopReason, Vec<AgentEvent>) {
    let prompt = prompt.to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let stop = driver.await.expect("join").expect("turn ok");
    (stop, events)
}

fn tool_result_of(events: &[AgentEvent], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id: rid,
                output,
                is_error,
                ..
            } if rid == id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a tool result for {id}"))
}

fn seed_skill_with_companion(root: &Path) {
    let dir = root.join("packer");
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: packer\ndescription: Packing conventions.\n---\nRead references/spec.md \
         next to this file.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("references/spec.md"),
        "COMPANION-GROUND-TRUTH-3141",
    )
    .unwrap();
}

/// Companion files, codex-style: the mentioned skill's body is injected as
/// text, the model reads the companion itself by absolute path (the skills
/// root is readable from the start — skill use never has to change policy),
/// and a write into the skill directory is refused.
#[tokio::test]
async fn a_mentioned_skill_injects_and_its_companions_are_readable_never_writable() {
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill_with_companion(skills.path());
    let companion = skills.path().join("packer/references/spec.md");
    let companion_arg = companion.display().to_string();
    let intruder = skills.path().join("packer/references/planted.md");

    let provider = MockProvider::new(vec![
        // The model follows the injected body: reads the companion by
        // absolute path, and (adversarially) tries to write into the skill
        // directory.
        vec![
            tool_use("call-read", "read_file", json!({ "path": companion_arg })),
            tool_use(
                "call-intrude",
                "write_file",
                json!({ "path": intruder.display().to_string(), "content": "planted" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let handle = provider.clone();
    let host = ac_cli::build_host(
        Arc::new(provider),
        workspace.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            ..Default::default()
        },
    )
    .expect("build_host");

    let input = ac_cli::compose_turn_input(&host, "pack it up with $packer");
    assert!(
        input.contains("<skill>\n<name>packer</name>"),
        "the mention must inject the skill block: {input}"
    );
    assert!(input.contains("Read references/spec.md"));

    let (stop, events) = run(host.session, &input).await;
    assert_eq!(stop, StopReason::EndTurn);

    // The injected block reached the model in the FIRST request — as text.
    let requests = handle.requests();
    let first_user = format!("{:?}", requests[0].messages);
    assert!(first_user.contains("<skill>"));

    let (read_out, read_err) = tool_result_of(&events, "call-read");
    assert!(
        !read_err,
        "the companion must be readable (skills root is granted up front): {read_out}"
    );
    assert!(read_out.contains("COMPANION-GROUND-TRUTH-3141"));

    let (intrude_out, intrude_err) = tool_result_of(&events, "call-intrude");
    assert!(
        intrude_err,
        "a write into the skill directory must be refused: {intrude_out}"
    );
    assert!(
        !intruder.exists(),
        "nothing may be planted in the skill dir"
    );
}

/// Without a skills root, workspace containment is unchanged: the same
/// companion path is unreadable — the grant exists only because the host
/// configured skills, not as a general read widening.
#[tokio::test]
async fn without_a_skills_root_the_same_path_stays_unreadable() {
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill_with_companion(skills.path());
    let companion = skills.path().join("packer/references/spec.md");

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-read",
                "read_file",
                json!({ "path": companion.display().to_string() }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let host = ac_cli::build_host(
        Arc::new(provider),
        workspace.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");

    let (stop, events) = run(host.session, "read that file").await;
    assert_eq!(stop, StopReason::EndTurn);

    let (read_out, read_err) = tool_result_of(&events, "call-read");
    assert!(
        read_err,
        "outside-workspace reads must stay contained when no skills root is set: {read_out}"
    );
    assert!(!read_out.contains("COMPANION-GROUND-TRUTH-3141"));
}

/// The `--skill` selection and a `$mention` of the same skill inject it ONCE.
#[test]
fn an_upfront_selection_and_a_mention_of_the_same_skill_dedupe() {
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill_with_companion(skills.path());

    let host = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        workspace.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            skill: Some("packer".to_string()),
            ..Default::default()
        },
    )
    .expect("build_host");

    let input = ac_cli::compose_turn_input(&host, "pack with $packer");
    assert_eq!(
        input.matches("<skill>").count(),
        1,
        "the same skill selected twice must inject once: {input}"
    );
}

/// A skill whose directory is a symlink out of the skills root (the dotfiles
/// layout) is advertised at its CANONICAL path — and that exact path must be
/// readable, or the catalog would instruct the model to read a file the
/// policy then refuses.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_skill_is_readable_at_its_advertised_canonical_path() {
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let real_dir = elsewhere.path().join("linked");
    std::fs::create_dir_all(real_dir.join("references")).unwrap();
    std::fs::write(
        real_dir.join("SKILL.md"),
        "---\nname: linked\ndescription: Lives outside the root.\n---\nRead references/data.md.\n",
    )
    .unwrap();
    std::fs::write(real_dir.join("references/data.md"), "LINKED-TRUTH-2718").unwrap();
    std::os::unix::fs::symlink(&real_dir, skills.path().join("linked")).unwrap();

    let canonical_companion = real_dir.canonicalize().unwrap().join("references/data.md");

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-read",
                "read_file",
                json!({ "path": canonical_companion.display().to_string() }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let handle = provider.clone();
    let host = ac_cli::build_host(
        Arc::new(provider),
        workspace.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            ..Default::default()
        },
    )
    .expect("build_host");

    let (stop, events) = run(host.session, "use $linked").await;
    assert_eq!(stop, StopReason::EndTurn);

    // The catalog advertises the canonical (out-of-root) SKILL.md path…
    let system = handle.requests()[0].system.clone().unwrap();
    let canonical_md = real_dir.canonicalize().unwrap().join("SKILL.md");
    assert!(
        system.contains(&format!("(file: {})", canonical_md.display())),
        "catalog must list the canonical path: {system}"
    );

    // …and that path's directory is actually readable.
    let (read_out, read_err) = tool_result_of(&events, "call-read");
    assert!(
        !read_err,
        "the advertised canonical dir must be readable: {read_out}"
    );
    assert!(read_out.contains("LINKED-TRUTH-2718"));
}

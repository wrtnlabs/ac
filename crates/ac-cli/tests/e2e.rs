//! Offline end-to-end proof of the whole AC stack: a scripted `MockProvider`
//! drives the REAL built-in tool registry over the REAL runtime loop against a
//! REAL temp directory. No network, no API key.
//!
//! These tests are the definition of done — if they pass, the generic agent
//! loop with built-in tools genuinely works, tools mutate disk, results feed
//! back to the model, and the path policy actually contains writes.

use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentEvent, Session};
use ac_types::{ContentPart, Role, StopReason};
use serde_json::json;

/// Assemble through the SHIPPED wiring path (`ac_cli::build_host`) — the same
/// function `main.rs` uses — so this test breaks if the real host wiring
/// (built-in registration, system prompt, path policy) is gutted. Only the
/// provider is swapped for the scripted mock.
fn built_session(provider: MockProvider, dir: &std::path::Path) -> (Session, MockProvider) {
    let handle = provider.clone();
    let host = ac_cli::build_host(
        Arc::new(provider),
        dir,
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");
    (host.session, handle)
}

/// The shipped generic host must register every built-in and carry a system
/// prompt. Deleting `register_builtins` or emptying `SYSTEM_PROMPT` in the
/// library fails here — the standing-proof host cannot silently rot.
#[test]
fn shipped_host_registers_all_builtins_and_a_prompt() {
    let specs = ac_cli::generic_registry().specs();
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    for expected in [
        "read_file",
        "write_file",
        "edit_file",
        "list_files",
        "glob",
        "grep",
        "shell",
        "fetch",
    ] {
        assert!(names.contains(&expected), "missing built-in: {expected}");
    }
    assert_eq!(specs.len(), 8, "unexpected built-in tool count");
    assert!(
        !ac_cli::SYSTEM_PROMPT.trim().is_empty(),
        "the generic host must ship a system prompt"
    );
}

/// Drain the event sink into a flat vec, running the turn to completion.
async fn run(mut session: Session, prompt: &str) -> (Result<StopReason, String>, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });

    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let result = driver.await.expect("join").map_err(|e| e.to_string());
    (result, events)
}

#[tokio::test]
async fn agent_reads_seed_then_writes_new_file_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();

    // Three scripted turns: read the seed, write a NEW file, then answer.
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-read", "read_file", json!({ "path": "notes.txt" })),
            stop_tool_use(),
        ],
        vec![
            tool_use(
                "call-write",
                "write_file",
                json!({ "path": "out.txt", "content": "world" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let (session, handle) = built_session(provider, dir.path());
    let (result, events) = run(session, "read notes then write out").await;

    // --- ground truth on disk ---
    let out = dir.path().join("out.txt");
    assert!(out.exists(), "out.txt should have been created on disk");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "world");

    // --- the read tool actually returned the seed content ---
    let read_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                name, output, id, ..
            } if name == "read_file" && id == "call-read" => Some(output.clone()),
            _ => None,
        })
        .expect("expected a read_file tool result");
    assert!(
        read_result.contains("hello"),
        "read_file result should carry the seed: {read_result}"
    );

    // --- write reported success (not an error) ---
    let write_ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult { name, is_error, .. } if name == "write_file" && !is_error
        )
    });
    assert!(write_ok, "write_file should have succeeded");

    // --- final stop reason is EndTurn ---
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    // --- the provider was called 3 times ---
    assert_eq!(handle.call_count(), 3, "expected three model round-trips");

    // --- later requests carried ToolResult content back to the model ---
    let requests = handle.requests();
    let tool_results_fed_back: usize = requests
        .iter()
        .flat_map(|r| r.messages.iter())
        .flat_map(|m| m.content.iter())
        .filter(|p| matches!(p, ContentPart::ToolResult(_)))
        .count();
    assert!(
        tool_results_fed_back >= 2,
        "read+write tool results should have been fed back into later requests, saw {tool_results_fed_back}"
    );

    // The last request must contain the read_file result echoed as a User
    // ToolResult with the seed content — proving the loop closes.
    let last = requests.last().expect("a request");
    let fed_hello = last.messages.iter().any(|m| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult(tr) if tr.content.contains("hello")))
    });
    assert!(
        fed_hello,
        "the seed content should be visible to the model in the final request"
    );
}

#[tokio::test]
async fn policy_refuses_write_escaping_root_end_to_end() {
    let dir = tempfile::tempdir().unwrap();

    // Point the escape at a sibling of the sandbox root so we can assert it was
    // never created anywhere outside.
    let outside = dir.path().parent().unwrap().join("escape.txt");
    let _ = std::fs::remove_file(&outside);

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-escape",
                "write_file",
                json!({ "path": "../escape.txt", "content": "pwned" }),
            ),
            stop_tool_use(),
        ],
        vec![text("blocked"), stop_end()],
    ]);

    let (session, handle) = built_session(provider, dir.path());
    let (result, events) = run(session, "try to escape").await;

    // The tool result the model saw must be an error (policy refusal as DATA).
    let refused = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult { name, is_error, .. } if name == "write_file" && *is_error
        )
    });
    assert!(refused, "escaping write must come back as an error");

    // Ground truth: no file materialized outside the sandbox root.
    assert!(
        !outside.exists(),
        "policy must have prevented any write outside the root"
    );
    assert!(
        !dir.path().join("../escape.txt").exists(),
        "no escape.txt outside root"
    );

    // The loop still completed cleanly and the error was fed back to the model.
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);
    let saw_error_result = handle
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .flat_map(|m| m.content.iter())
        .any(|p| matches!(p, ContentPart::ToolResult(tr) if tr.is_error));
    assert!(
        saw_error_result,
        "the policy-refusal error should have been fed back to the model"
    );
}

/// Lay a valid skill directory under `root`.
fn seed_skill(root: &std::path::Path, name: &str, description: &str, body: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

/// The SHIPPED skills wiring, through `build_host` + `compose_turn_input`
/// exactly as the binary uses them: the codex-format catalog (with file
/// locators) reaches the system prompt, the `--skill` selection injects the
/// SKILL.md body into the turn input as a `<skill>` block — no tool call
/// anywhere — and a file written per the skill's instructions lands on disk.
#[tokio::test]
async fn shipped_host_injects_a_selected_skill_and_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill(
        skills.path(),
        "output-style",
        "Conventions for written output.",
        "Every file you write must contain the token GROVE-42.",
    );

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-write",
                "write_file",
                json!({ "path": "note.txt", "content": "GROVE-42" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let handle = provider.clone();
    let host = ac_cli::build_host(
        Arc::new(provider),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            skill: Some("output-style".to_string()),
            ..Default::default()
        },
    )
    .expect("build_host");

    let input = ac_cli::compose_turn_input(&host, "write a note");
    let (result, _events) = run(host.session, &input).await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle.requests();
    assert_eq!(requests.len(), 2, "expected two model round-trips");

    let system = requests[0].system.as_deref().expect("a system prompt");
    assert!(
        system.contains("## Skills"),
        "the skill catalog block must reach the system prompt"
    );
    assert!(
        system.contains("- output-style: Conventions for written output. (file: "),
        "the catalog must advertise the seeded skill with its file locator: {system}"
    );
    assert!(
        system.contains("### How to use skills"),
        "the usage instructions must ship with the catalog"
    );

    // The skill body arrived as injected text IN THE FIRST REQUEST — before
    // any tool round-trip, with no load_skill in the loop.
    let first_user_text = requests[0]
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .expect("a user message");
    assert!(first_user_text.contains("write a note"));
    assert!(
        first_user_text.contains("<skill>\n<name>output-style</name>"),
        "the selected skill must be injected as a <skill> block: {first_user_text}"
    );
    assert!(
        first_user_text.contains("GROVE-42"),
        "the injected block must carry the skill body"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("note.txt must exist"),
        "GROVE-42"
    );
}

/// A `$mention` in the prompt selects and injects the named skill — the
/// mention path of the same shipped wiring.
#[test]
fn a_dollar_mention_composes_the_skill_into_the_turn_input() {
    let dir = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill(
        skills.path(),
        "output-style",
        "Conventions for written output.",
        "Every file you write must contain the token GROVE-42.",
    );

    let host = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            ..Default::default()
        },
    )
    .expect("build_host");

    let input = ac_cli::compose_turn_input(&host, "write a note per $output-style");
    assert!(input.starts_with("write a note per $output-style"));
    assert!(input.contains("<skill>\n<name>output-style</name>"));
    assert!(input.contains("GROVE-42"));

    // No mention, no selection: the prompt passes through untouched.
    let plain = ac_cli::compose_turn_input(&host, "just write a note");
    assert_eq!(plain, "just write a note");

    // An unknown mention injects nothing rather than guessing.
    let unknown = ac_cli::compose_turn_input(&host, "use $nonexistent");
    assert_eq!(unknown, "use $nonexistent");
}

/// A selected skill that doesn't exist (or a selection without a skills root
/// at all) must fail at build time, not at the first turn.
#[test]
fn build_host_rejects_a_missing_selected_skill() {
    let dir = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    seed_skill(skills.path(), "present", "A skill that exists.", "Body.");

    let err = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skills_root: Some(skills.path().to_path_buf()),
            skill: Some("absent".to_string()),
            ..Default::default()
        },
    )
    .err()
    .expect("a missing selected skill must fail the build");
    assert!(err.to_string().contains("absent"));
    assert!(err.to_string().contains("present"), "lists what exists");

    let err = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            skill: Some("anything".to_string()),
            ..Default::default()
        },
    )
    .err()
    .expect("a skill selection without a skills root must fail the build");
    assert!(err.to_string().contains("skills root"));
}

/// The shipped host installs an OS-sandbox launcher on the tool ctx when
/// `HostOptions.sandbox` is set, and leaves it absent otherwise. This is the
/// wiring proof; the kernel-enforcement proof lives in `ac-sandbox`'s
/// platform smoke tests.
#[test]
fn shipped_host_installs_a_sandbox_when_asked() {
    let dir = tempfile::tempdir().unwrap();

    let plain = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");
    assert!(
        plain.ctx.sandbox.is_none(),
        "no sandbox unless the host asks for one"
    );

    let sandboxed = ac_cli::build_host(
        Arc::new(MockProvider::new(vec![])),
        dir.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            sandbox: true,
            sandbox_network: false,
            ..Default::default()
        },
    )
    .expect("build_host");
    let launcher = sandboxed
        .ctx
        .sandbox
        .as_ref()
        .expect("a launcher must be installed");
    // On macOS/Linux the launcher advertises strict; on other platforms it is
    // honestly off.
    let mode = launcher.mode();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    assert_eq!(mode, ac_tool::SandboxMode::Strict);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    assert_eq!(mode, ac_tool::SandboxMode::Off);
}

/// The FULL shipped path with the sandbox on: a scripted model runs a `shell`
/// command that tries to write outside the workspace. The command is denied by
/// the kernel sandbox and the tool result reports `strict` mode — proving
/// build_host → registry → shell tool → ctx.sandbox → sandbox-exec is wired
/// end-to-end, not just that a launcher is present. macOS-gated because it
/// asserts real Seatbelt enforcement.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn shipped_sandbox_contains_a_real_shell_write_escape() {
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let escape = outside.path().canonicalize().unwrap().join("pwned.txt");

    let provider = MockProvider::new(vec![
        vec![
            tool_use(
                "call-escape",
                "shell",
                json!({ "command": format!("echo pwned > {}", escape.display()) }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let host = ac_cli::build_host(
        Arc::new(provider),
        ws.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions {
            sandbox: true,
            sandbox_network: false,
            ..Default::default()
        },
    )
    .expect("build_host");

    let (_result, events) = run(host.session, "escape the sandbox").await;

    let shell_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a shell tool result");

    assert!(
        shell_result.contains("\"mode\":\"strict\""),
        "the shell result must report strict sandbox mode; got {shell_result}"
    );
    assert!(
        !escape.exists(),
        "the escaping write must have been denied by the kernel sandbox"
    );
}

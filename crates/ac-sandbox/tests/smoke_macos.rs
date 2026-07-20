//! Real kernel-enforcement smoke for the macOS (Seatbelt) backend. These spawn
//! actual `sandbox-exec` commands and assert the sandbox contains them — the
//! honest proof that this is an *actual* sandbox, not a pretend one. Gated to
//! macOS; on other targets the file compiles to nothing.
#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Stdio;
use std::thread;

use ac_sandbox::OsSandbox;
use ac_tool::{
    CommandSpec, NetworkMode, ResourceLimits, SandboxLauncher, SandboxMode, SandboxPolicy,
};

struct Run {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    mode: SandboxMode,
}

/// Prepare `cmd` under `policy` with `cwd`, spawn it for real, and collect the
/// outcome. This drives the exact path the shell tool uses.
async fn run(policy: SandboxPolicy, cwd: &Path, cmd: &str) -> Run {
    let launcher = OsSandbox::new(policy);
    let spec = CommandSpec::new("sh", ["-c", cmd], cwd.to_path_buf());
    let prepared = launcher.prepare(&spec).expect("prepare");
    let mode = prepared.mode;
    let mut command = prepared.command;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = command.output().await.expect("spawn");
    Run {
        exit: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        mode,
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn a_plain_command_runs_and_is_strict() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let r = run(SandboxPolicy::workspace(&root), &root, "echo hello-sandbox").await;
    assert_eq!(r.mode, SandboxMode::Strict);
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "hello-sandbox");
}

#[tokio::test]
async fn a_write_inside_the_workspace_succeeds() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let r = run(
        SandboxPolicy::workspace(&root),
        &root,
        "echo contained > out.txt && cat out.txt",
    )
    .await;
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "contained");
    assert_eq!(
        std::fs::read_to_string(root.join("out.txt"))
            .unwrap()
            .trim(),
        "contained"
    );
}

#[tokio::test]
async fn a_write_outside_the_workspace_is_denied() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let outside = workspace();
    let outside_root = outside.path().canonicalize().unwrap();
    let target = outside_root.join("evil.txt");

    let r = run(
        SandboxPolicy::workspace(&root),
        &root,
        &format!("echo pwned > {}", target.display()),
    )
    .await;

    assert_ne!(r.exit, Some(0), "the escaping write must fail");
    assert!(
        !target.exists(),
        "the file outside the workspace must NOT exist — kernel denied the write"
    );
}

#[tokio::test]
async fn reading_a_denied_secret_is_blocked() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    // A secret directory that lives INSIDE the readable workspace but is on the
    // mandatory deny-set — proves the deny overrides the broad read allow.
    let secret_dir = root.join("secrets");
    std::fs::create_dir(&secret_dir).unwrap();
    std::fs::write(secret_dir.join("token"), "TOP-SECRET-VALUE").unwrap();

    let mut policy = SandboxPolicy::workspace(&root);
    policy.deny_paths = vec![secret_dir.clone()];

    let r = run(policy, &root, "cat secrets/token").await;
    assert_ne!(r.exit, Some(0), "reading the secret must fail");
    assert!(
        !r.stdout.contains("TOP-SECRET-VALUE"),
        "the secret must not leak to stdout; got: {:?}",
        r.stdout
    );
}

#[tokio::test]
async fn network_off_blocks_a_loopback_connect_and_on_allows_it() {
    // A loopback HTTP responder OUTSIDE the sandbox. Under network-off the
    // sandboxed curl cannot even open the socket; under network-on it connects
    // and reads the body. Fully hermetic — no external host.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        // Serve a few connections then stop; each gets a valid tiny response.
        listener.set_nonblocking(false).ok();
        for _ in 0..8 {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    let mut buf = [0u8; 512];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                    );
                }
                Err(_) => break,
            }
        }
    });

    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let url = format!("http://127.0.0.1:{port}/");
    let cmd = format!("curl -s -m 4 {url}");

    // OFF (default): connect denied.
    let off = run(SandboxPolicy::workspace(&root), &root, &cmd).await;
    assert_ne!(off.exit, Some(0), "network-off must block the connect");
    assert!(
        !off.stdout.contains("hi"),
        "no body should come back when network is off; got {:?}",
        off.stdout
    );

    // ON: connect allowed, body returns.
    let on = run(
        SandboxPolicy::workspace(&root).with_network(NetworkMode::On),
        &root,
        &cmd,
    )
    .await;
    assert_eq!(
        on.exit,
        Some(0),
        "network-on must allow the connect; stderr {}",
        on.stderr
    );
    assert_eq!(on.stdout.trim(), "hi");

    drop(handle); // detached; process exit reaps it
}

#[tokio::test]
async fn a_file_size_rlimit_is_kernel_enforced() {
    // RLIMIT_FSIZE is per-process (no per-user-global confound), so it is the
    // clean deterministic proof that the resource caps actually bite: a write
    // past the cap is stopped by the kernel (SIGXFSZ) and the file is truncated
    // to the limit, while a small write succeeds.
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let mut policy = SandboxPolicy::workspace(&root);
    policy.limits = ResourceLimits {
        max_file_size: Some(2048),
        ..Default::default()
    };

    // A 100-byte write is fine; a 5000-byte write is capped.
    let r = run(
        policy,
        &root,
        "head -c 100 /dev/zero > small.bin; echo small=$?; \
         head -c 5000 /dev/zero > big.bin; echo big=$?",
    )
    .await;

    assert!(
        r.stdout.contains("small=0"),
        "a write within the cap must succeed; got {:?} / {:?}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("big=") && !r.stdout.contains("big=0"),
        "a write past the cap must fail; got {:?} / {:?}",
        r.stdout,
        r.stderr
    );
    let big = std::fs::metadata(root.join("big.bin")).unwrap().len();
    assert!(
        big <= 2048,
        "the file must be truncated to the RLIMIT_FSIZE cap; was {big} bytes"
    );
}

#[tokio::test]
async fn a_process_rlimit_is_applied_to_the_child() {
    // Proves RLIMIT_NPROC is plumbed into the child (visible via `ulimit -u`).
    // We set it comfortably ABOVE any real baseline so the shell still runs —
    // the fork-bomb *cap* is the same mechanism at a lower value, which a host
    // sets knowing NPROC is per-user-global (see docs/ac-sandbox.md).
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let mut policy = SandboxPolicy::workspace(&root);
    policy.limits = ResourceLimits {
        max_processes: Some(4096),
        ..Default::default()
    };
    let r = run(policy, &root, "ulimit -u").await;
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout.trim(),
        "4096",
        "the child must see the RLIMIT_NPROC we set"
    );
}

#[tokio::test]
async fn cwd_outside_write_roots_is_refused_before_spawn() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let other = workspace();
    let other_root = other.path().canonicalize().unwrap();

    let launcher = OsSandbox::new(SandboxPolicy::workspace(&root));
    // cwd is a directory the policy never granted — prepare must refuse.
    let spec = CommandSpec::new("sh", ["-c", "echo hi"], other_root);
    assert!(launcher.prepare(&spec).is_err());
}

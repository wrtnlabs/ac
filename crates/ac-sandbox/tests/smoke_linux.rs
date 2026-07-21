//! Real kernel-enforcement smoke for the Linux (landlock + seccomp + rlimit)
//! backend. Spawns actual commands and asserts containment. Gated to Linux; on
//! other targets the file compiles to nothing.
//!
//! Filesystem assertions are conditioned on the achieved [`SandboxMode`]: on a
//! kernel without the Landlock LSM the backend degrades (seccomp + rlimits
//! still apply), so those tests assert containment only when the run came back
//! `Strict`. Network-off (seccomp) and resource caps (rlimit) do not depend on
//! landlock and are asserted unconditionally.
#![cfg(target_os = "linux")]

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

/// A workspace policy that degrades (rather than refusing) where the kernel
/// can't fully enforce, so these tests run on kernels without an active
/// Landlock LSM — the FS-containment asserts then gate on `mode == Strict`,
/// while seccomp (network) and rlimit assertions hold unconditionally.
fn policy(root: &Path) -> SandboxPolicy {
    SandboxPolicy::workspace(root).allow_degraded()
}

#[tokio::test]
async fn a_plain_command_runs() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let r = run(policy(&root), &root, "echo hello-sandbox").await;
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "hello-sandbox");
    eprintln!("mode = {:?}", r.mode);
}

#[tokio::test]
async fn a_write_inside_the_workspace_succeeds() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let r = run(
        policy(&root),
        &root,
        "echo contained > out.txt && cat out.txt",
    )
    .await;
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "contained");
}

#[tokio::test]
async fn a_write_outside_the_workspace_is_denied_when_strict() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let outside = workspace();
    let outside_root = outside.path().canonicalize().unwrap();
    let target = outside_root.join("evil.txt");

    let r = run(
        policy(&root),
        &root,
        &format!("echo pwned > {}", target.display()),
    )
    .await;

    if r.mode == SandboxMode::Strict {
        assert_ne!(
            r.exit,
            Some(0),
            "the escaping write must fail under landlock"
        );
        assert!(
            !target.exists(),
            "the file outside the workspace must NOT exist — landlock denied the write"
        );
    } else {
        eprintln!(
            "SKIP fs-containment assert: landlock unavailable (mode {:?})",
            r.mode
        );
    }
}

#[tokio::test]
async fn reading_outside_the_grant_is_denied_when_strict() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let outside = workspace();
    let outside_root = outside.path().canonicalize().unwrap();
    std::fs::write(outside_root.join("secret"), "TOP-SECRET-VALUE").unwrap();

    let r = run(
        policy(&root),
        &root,
        &format!("cat {}", outside_root.join("secret").display()),
    )
    .await;

    if r.mode == SandboxMode::Strict {
        assert_ne!(r.exit, Some(0), "reading outside the grant must fail");
        assert!(
            !r.stdout.contains("TOP-SECRET-VALUE"),
            "the secret must not leak; got {:?}",
            r.stdout
        );
    } else {
        eprintln!(
            "SKIP read-containment assert: landlock unavailable (mode {:?})",
            r.mode
        );
    }
}

#[tokio::test]
async fn network_off_blocks_socket_and_on_allows_it() {
    // seccomp EPERMs socket() when network is off — no landlock needed, so this
    // asserts unconditionally.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
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

    let off = run(policy(&root), &root, &cmd).await;
    assert_ne!(off.exit, Some(0), "network-off must block the connect");
    assert!(
        !off.stdout.contains("hi"),
        "no body when off; got {:?}",
        off.stdout
    );

    let on = run(policy(&root).with_network(NetworkMode::On), &root, &cmd).await;
    assert_eq!(
        on.exit,
        Some(0),
        "network-on must connect; stderr {}",
        on.stderr
    );
    assert_eq!(on.stdout.trim(), "hi");

    drop(handle);
}

#[tokio::test]
async fn a_file_size_rlimit_is_kernel_enforced() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let mut policy = policy(&root);
    policy.limits = ResourceLimits {
        max_file_size: Some(2048),
        ..Default::default()
    };
    let r = run(
        policy,
        &root,
        "head -c 100 /dev/zero > small.bin; echo small=$?; \
         head -c 5000 /dev/zero > big.bin; echo big=$?",
    )
    .await;
    assert!(
        r.stdout.contains("small=0"),
        "small write ok; {:?}/{:?}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("big=") && !r.stdout.contains("big=0"),
        "big write must fail; {:?}/{:?}",
        r.stdout,
        r.stderr
    );
    let big = std::fs::metadata(root.join("big.bin")).unwrap().len();
    assert!(big <= 2048, "file truncated to cap; was {big}");
}

#[tokio::test]
async fn a_process_rlimit_is_applied_to_the_child() {
    let ws = workspace();
    let root = ws.path().canonicalize().unwrap();
    let mut policy = policy(&root);
    policy.limits = ResourceLimits {
        max_processes: Some(4096),
        ..Default::default()
    };
    // Read the child's real RLIMIT_NPROC from /proc (dash's `ulimit` has no
    // `-u`), proving setrlimit was applied in pre_exec.
    let r = run(policy, &root, "grep '^Max processes' /proc/self/limits").await;
    assert_eq!(r.exit, Some(0), "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("4096"),
        "the child must see the RLIMIT_NPROC we set; got {:?}",
        r.stdout
    );
}

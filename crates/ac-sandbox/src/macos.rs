//! macOS backend: Apple Seatbelt via `/usr/bin/sandbox-exec`.
//!
//! The command is wrapped as
//! `/usr/bin/sandbox-exec -p <profile> -DKEY=path … -- sh -c <cmd>`. The
//! executable path is pinned to `/usr/bin/sandbox-exec` so a poisoned `$PATH`
//! cannot substitute it. Filesystem paths ride `-D` parameters and are
//! referenced as `(param "KEY")` inside the profile, so no path is ever
//! interpolated into the SBPL string (this is how codex avoids SBPL-escaping
//! bugs; we follow it).
//!
//! Profile posture (v1, matching the Zed/codex references and this repo's
//! `docs/ac-sandbox.md` non-goals): reads are broad-with-a-mandatory-secret-
//! deny-set, writes are allow-listed to the write roots, and the network is the
//! real exfil gate — off by default (the profile's `(deny default)` denies all
//! sockets), unrestricted when the policy asks for it.
//!
//! The base profile below is adapted from OpenAI codex's
//! `seatbelt_base_policy.sbpl` (Apache-2.0) — it is what lets real programs
//! (dyld, sh, common toolchains) run under `(deny default)`.

use ac_tool::{CommandSpec, NetworkMode, Prepared, SandboxError, SandboxMode};

use crate::rlimit;

const SEATBELT: &str = "/usr/bin/sandbox-exec";

/// Base allow-set that makes ordinary programs runnable under `(deny default)`.
/// Adapted from codex-rs `seatbelt_base_policy.sbpl` (Apache-2.0); trimmed to
/// the process/sysctl/tty/prefs essentials.
const BASE_POLICY: &str = r#"(version 1)

; start closed
(deny default)

; child processes inherit the parent's policy
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; /dev/null writes (character device only)
(allow file-write-data
  (require-all
    (path "/dev/null")
    (vnode-type CHARACTER-DEVICE)))

; read-only CPU/OS sysctls programs commonly probe
(allow sysctl-read
  (sysctl-name-prefix "hw.")
  (sysctl-name-prefix "kern.")
  (sysctl-name-prefix "machdep.cpu.")
  (sysctl-name "vm.loadavg")
  (sysctl-name "sysctl.proc_cputype"))

; user/dir info lookup
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo"))

; POSIX semaphores / shared memory (python multiprocessing, libomp)
(allow ipc-posix-sem)
(allow ipc-posix-shm-read-data
  ipc-posix-shm-write-create
  ipc-posix-shm-write-unlink)

; pseudo-terminals, so shells detect a TTY and stay functional
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))

; read-only user preferences (cfprefs)
(allow ipc-posix-shm-read* (ipc-posix-name-prefix "apple.cfprefs."))
(allow mach-lookup
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent"))
(allow user-preference-read)
"#;

/// Network allow-set appended when the policy permits egress. Adapted from
/// codex-rs `seatbelt_network_policy.sbpl` (Apache-2.0) for the DNS/TLS service
/// lookups, plus a blanket `(allow network*)` since v1 network-on is
/// unrestricted (no proxy funnel — that is the v2 phase).
const NETWORK_ON_POLICY: &str = r#"
; --- network enabled (v1: unrestricted) ---
(allow network*)
(allow system-socket)
(allow mach-lookup
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.networkd")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.ocspd")
  (global-name "com.apple.trustd.agent"))
(allow sysctl-read (sysctl-name-prefix "net."))
"#;

pub fn prepare(
    policy: &ac_tool::SandboxPolicy,
    spec: &CommandSpec,
) -> Result<Prepared, SandboxError> {
    let (profile, params) = build_profile(policy);

    let mut cmd = tokio::process::Command::new(SEATBELT);
    cmd.arg("-p").arg(&profile);
    for (key, path) in &params {
        cmd.arg(format!("-D{key}={}", path.to_string_lossy()));
    }
    cmd.arg("--");
    cmd.arg(&spec.program);
    cmd.args(&spec.args);
    cmd.current_dir(&spec.cwd);

    // Resource caps inherit across sandbox-exec's own exec into the shell.
    if let Err(e) = rlimit::install(&mut cmd, &policy.limits) {
        if policy.fail_closed {
            return Err(e);
        }
        // Non-fatal: a limit we can't set here still leaves a real FS/network
        // sandbox. Report Degraded rather than pretend it's fully Strict.
        return Ok(Prepared {
            command: cmd,
            mode: SandboxMode::Degraded,
        });
    }

    Ok(Prepared {
        command: cmd,
        mode: SandboxMode::Strict,
    })
}

/// Assemble the full SBPL profile plus the `-D` param bindings (key → path).
fn build_profile(policy: &ac_tool::SandboxPolicy) -> (String, Vec<(String, std::path::PathBuf)>) {
    let mut profile = String::from(BASE_POLICY);
    let mut params: Vec<(String, std::path::PathBuf)> = Vec::new();

    // Reads: broad, then deny the mandatory secret set (last-match-wins).
    profile.push_str("\n; --- filesystem ---\n(allow file-read*)\n");
    for (i, deny) in policy.deny_paths.iter().enumerate() {
        let key = format!("DENY_{i}");
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath (param \"{key}\")))\n"
        ));
        params.push((key, deny.clone()));
    }

    // Writes: allow only the write roots.
    for (i, root) in policy.write_roots.iter().enumerate() {
        let key = format!("WRITE_{i}");
        profile.push_str(&format!(
            "(allow file-write* (subpath (param \"{key}\")))\n"
        ));
        params.push((key, root.clone()));
    }

    if policy.network == NetworkMode::On {
        profile.push_str(NETWORK_ON_POLICY);
    }

    (profile, params)
}

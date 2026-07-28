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
//! Profile posture: reads and writes are allow-listed. Read authority is the
//! policy's read/write roots plus narrowly scoped system/runtime roots; there
//! is no global `(allow file-read*)`. The network is off by default (the
//! profile's `(deny default)` denies all sockets), unrestricted when the policy
//! asks for it.
//!
//! The base profile below is adapted from OpenAI codex's
//! `seatbelt_base_policy.sbpl` (Apache-2.0) — it is what lets real programs
//! (dyld, sh, common toolchains) run under `(deny default)`.

use ac_tool::{CommandSpec, NetworkMode, Prepared, SandboxError, SandboxMode, WriteDenyRule};

use crate::rlimit;

const SEATBELT: &str = "/usr/bin/sandbox-exec";

/// Read-only roots needed by the loader, base command-line tools, developer
/// toolchains, and common package-manager runtimes. User data (`/Users`) and
/// temporary storage (`/private/tmp`, `/private/var/folders`) are deliberately
/// absent: a host must grant those through `SandboxPolicy::read_roots`.
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/Library/Apple",
    "/Library/Developer",
    "/Applications/Xcode.app/Contents/Developer",
    "/private/etc",
    "/private/var/db/dyld",
    "/private/var/db/timezone",
    "/private/var/select",
    "/opt/homebrew",
    "/nix/store",
];

/// Base allow-set that makes ordinary programs runnable under `(deny default)`.
/// Adapted from codex-rs `seatbelt_base_policy.sbpl` (Apache-2.0); trimmed to
/// the process/sysctl/tty/prefs essentials.
const BASE_POLICY: &str = r#"(version 1)

; start closed
(deny default)

; path traversal/runtime probes may inspect metadata globally, but file
; contents remain scoped to explicit read roots below
(allow file-read-metadata)
; dyld/AMFI probes the root directory itself while starting a signed binary;
; this exposes only the root directory entries, not arbitrary file contents
(allow file-read-data (literal "/"))

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

; harmless character devices and descriptor aliases ordinary CLI tools read
(allow file-read*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/random")
  (literal "/dev/urandom")
  (literal "/dev/tty")
  (subpath "/dev/fd"))

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
    let invalid_rule = policy
        .write_deny_rules
        .iter()
        .find(|rule| seatbelt_write_deny_regex(rule).is_none());
    if invalid_rule.is_some() && policy.fail_closed {
        return Err(SandboxError::Invalid(
            "write deny rules must contain non-empty path components without separators or NUL"
                .to_string(),
        ));
    }
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
        mode: if invalid_rule.is_some() {
            SandboxMode::Degraded
        } else {
            SandboxMode::Strict
        },
    })
}

pub(crate) fn mode(policy: &ac_tool::SandboxPolicy) -> SandboxMode {
    if policy
        .write_deny_rules
        .iter()
        .any(|rule| seatbelt_write_deny_regex(rule).is_none())
    {
        SandboxMode::Degraded
    } else {
        SandboxMode::Strict
    }
}

/// Assemble the full SBPL profile plus the `-D` param bindings (key → path).
fn build_profile(policy: &ac_tool::SandboxPolicy) -> (String, Vec<(String, std::path::PathBuf)>) {
    let mut profile = String::from(BASE_POLICY);
    let mut params: Vec<(String, std::path::PathBuf)> = Vec::new();

    // SBPL is LAST-MATCH-WINS, so the order below is the policy:
    //   1. read only explicit/system roots, 2. write only explicit roots,
    //   3. recursive component write denies, 4. absolute denies — final.
    //
    // The denies MUST come last. Emitting them before the write allows (as
    // this did) let any allow whose subpath contained a denied path silently
    // un-deny it: a deny of `<root>/.git/hooks` followed by an allow of
    // `<root>` is a WRITABLE hook, and a hook is code the user's own tooling
    // executes later, outside the sandbox. A deny_paths entry is a promise
    // that nothing reaches it — it cannot be conditional on ordering against
    // an allow the caller supplies separately.
    profile.push_str("\n; --- filesystem: explicit reads ---\n");
    let mut read_roots = Vec::new();
    for root in SYSTEM_READ_ROOTS {
        let path = std::path::PathBuf::from(root);
        if path.exists() {
            // These are AC-owned constants, not host authorization roots.
            // Canonicalize aliases such as `/bin -> /usr/bin` before passing
            // them to Seatbelt, whose `subpath` parameter rejects symlinks.
            push_unique(
                &mut read_roots,
                std::fs::canonicalize(&path).unwrap_or(path),
            );
        }
    }
    // A write grant also needs read authority: shells routinely inspect a
    // file before replacing it, and `cwd` itself must be traversable.
    for root in policy.read_roots.iter().chain(policy.write_roots.iter()) {
        push_unique(&mut read_roots, root.clone());
    }
    for (i, root) in read_roots.iter().enumerate() {
        let key = format!("READ_{i}");
        profile.push_str(&format!("(allow file-read* (subpath (param \"{key}\")))\n"));
        params.push((key, root.clone()));
    }

    profile.push_str("\n; --- explicit writes ---\n");
    for (i, root) in policy.write_roots.iter().enumerate() {
        let key = format!("WRITE_{i}");
        profile.push_str(&format!(
            "(allow file-write* (subpath (param \"{key}\")))\n"
        ));
        params.push((key, root.clone()));
    }

    if !policy.write_deny_rules.is_empty() {
        profile.push_str("\n; --- recursive component write denies ---\n");
    }
    for rule in &policy.write_deny_rules {
        if let Some(regex) = seatbelt_write_deny_regex(rule) {
            profile.push_str(&format!("(deny file-write* (regex #\"{regex}\"))\n"));
        }
    }

    profile.push_str("\n; --- absolute read/write denies (must remain last) ---\n");
    for (i, deny) in policy.deny_paths.iter().enumerate() {
        let key = format!("DENY_{i}");
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath (param \"{key}\")))\n"
        ));
        params.push((key, deny.clone()));
    }

    if policy.network == NetworkMode::On {
        profile.push_str(NETWORK_ON_POLICY);
    }

    (profile, params)
}

fn push_unique(paths: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Translate a semantic component rule to a Seatbelt path regex.
///
/// The leading slash makes the match component-boundary-aware. ASCII letters
/// become explicit two-case classes because a differently-cased spelling on a
/// case-insensitive filesystem must not bypass a control-path deny.
fn seatbelt_write_deny_regex(rule: &WriteDenyRule) -> Option<String> {
    match rule {
        WriteDenyRule::Basename(name) => {
            let component = case_insensitive_component_regex(name)?;
            Some(format!("/{component}$"))
        }
        WriteDenyRule::Subtree(components) if !components.is_empty() => {
            let components = components
                .iter()
                .map(|component| case_insensitive_component_regex(component))
                .collect::<Option<Vec<_>>>()?;
            Some(format!("/{}(/|$)", components.join("/")))
        }
        WriteDenyRule::Subtree(_) => None,
    }
}

fn case_insensitive_component_regex(component: &str) -> Option<String> {
    if component.is_empty() || component.contains('/') || component.contains('\0') {
        return None;
    }
    let mut out = String::new();
    for ch in component.chars() {
        if ch.is_ascii_alphabetic() {
            out.push('[');
            out.push(ch.to_ascii_lowercase());
            out.push(ch.to_ascii_uppercase());
            out.push(']');
        } else {
            if matches!(
                ch,
                '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            ) {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{SandboxPolicy, WriteDenyRule};

    #[test]
    fn profile_has_no_global_file_read_grant() {
        let root = std::path::PathBuf::from("/tmp/ac-sandbox-profile-test");
        let (profile, _) = build_profile(&SandboxPolicy::workspace(root));
        assert!(
            !profile
                .lines()
                .any(|line| line.trim() == "(allow file-read*)"),
            "read authority must come only from explicit subpath grants"
        );
        assert!(profile.contains("(allow file-read* (subpath (param \"READ_"));
    }

    #[test]
    fn component_rules_compile_case_insensitively_and_at_boundaries() {
        assert_eq!(
            seatbelt_write_deny_regex(&WriteDenyRule::basename(".zshrc")),
            Some(r"/\.[zZ][sS][hH][rR][cC]$".to_string())
        );
        assert_eq!(
            seatbelt_write_deny_regex(&WriteDenyRule::subtree([".git", "hooks"])),
            Some(r"/\.[gG][iI][tT]/[hH][oO][oO][kK][sS](/|$)".to_string())
        );
        assert_eq!(
            seatbelt_write_deny_regex(&WriteDenyRule::subtree([".idea"])),
            Some(r"/\.[iI][dD][eE][aA](/|$)".to_string())
        );
    }
}

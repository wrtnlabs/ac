# RFC: `ac-sandbox` — an OS-level sandbox for the `shell` tool

Status: **v1 implemented + verified** (2026-07-21). The seam, both backends, resource caps, and
the fail-closed envelope are built and wired into the CLI host. macOS is live-verified end-to-end
(8 `ac-sandbox` smoke tests spawn real `sandbox-exec` and prove write-escape denial, secret-read
denial, the network on/off gate, and `RLIMIT_FSIZE` enforcement; a shipped-path e2e test drives
`build_host → shell → sandbox-exec`). Linux is verified in a real container (kernel 6.12): seccomp
(network-off) and both `setrlimit` caps genuinely enforce at the kernel level; the landlock FS
layer is compile/API-verified and honestly gated by a real-enforcement probe.

That container run earned its keep: it caught a **fail-open** — on a kernel where Landlock is
compiled but not in the active LSM list, `landlock_create_ruleset` succeeds and `restrict_self`
returns `Ok` while enforcing *nothing*, so a create-success probe reported `Strict` while writes
escaped. Fixed: the backend now probes *actual* enforcement (apply a read-only ruleset on a
scratch thread, attempt a forbidden write) and reports `Degraded`/refuses rather than a false
`Strict`. This is the exact "never pretend to sandbox" rule catching itself. The v2
egress-allowlist phase remains unbuilt. This document is the design of record; `docs/ac-sandbox.md`
in a diff means the contract changed.

`ac-sandbox` closes the gap the `shell` tool documents in its own module header: today a
command spawned by `shell` is contained only by its working directory — the child process can
read and write anything the host user can, reach any network, and fork without bound. The
in-process `PathPolicy` check contains where the tool *launches* the command, not what the
command *does*. For a kit whose job is running arbitrary shell, that in-process check is real
for our own code and **pretend for the child**. This document is the plan to make it real.

## The one rule this document exists to enforce

**An actual sandbox is kernel-enforced: the sandboxed process cannot lift its own restrictions.
Anything else is pretend, and pretend is worse than nothing** because it invites the operator to
trust a boundary that isn't there. Every mechanism below is graded on this axis, and we ship
only the `actual` ones. Where we cannot be actual (native Windows), we are loudly `off` and say
so — we never emit an advisory approximation and call it a sandbox.

This is not a stylistic preference. The reference implementations we studied all draw the same
line, and the one place a major sandbox blurred it — Anthropic's `sandbox-runtime` degrading
*fail-open* when its seccomp helper is missing, and a string-matching egress allowlist that
shipped a real SOCKS5 null-byte bypass in Claude Code v2.0.24–2.1.89 — is exactly the class of
bug this rule prevents.

## What we build (v1) and what we defer

v1 delivers a **real, kernel-enforced sandbox on macOS and Linux** covering the three
containments we can make actual with high confidence, plus the resource caps that every
reference implementation omits:

| Containment | v1 | Mechanism | Grade |
|---|---|---|---|
| **Filesystem** | ✅ | macOS Seatbelt deny-default profile; Linux `landlock` (LSM) | actual |
| **Process / syscall** | ✅ | macOS Seatbelt process scoping; Linux `seccompiler` BPF (block `ptrace`, `io_uring`, and `socket()` per network mode) | actual |
| **Resource limits** | ✅ | `setrlimit` (`RLIMIT_NPROC`/`AS`/`CPU`/`FSIZE`) via `pre_exec`; cgroup-v2 slice on Linux if available | actual |
| **Network — on/off** | ✅ | OFF = Seatbelt deny-default (macOS) / seccomp blocks `socket()` (Linux). ON = unrestricted egress. | actual |
| **Network — domain allowlist** | ❌ v2 | kernel-block-then-proxy: sever all egress at the kernel, funnel through a host proxy that owns DNS | deferred |
| **Native Windows** | ❌ | no honest kernel path without a ~18k-LOC bespoke Win32 build (codex ships one, *disabled by default*) | `off` + banner |

The deliberate v1 line: **we offer network as real binary on/off, and we do not pretend to
filter by domain.** "Off" is trivially actual (a process with no reachable socket cannot
exfiltrate); a domain allowlist is only actual as the full kernel-block-then-proxy architecture,
which is half the total code and carries all of the parser-differential CVE history. Shipping
on/off now is honest; shipping a string allowlist now would be pretend. The allowlist is a
clean, self-contained v2.

Native Windows gets `off` + a loud banner, and we document "run under WSL2 for the Linux path" —
matching what `sandbox-runtime` and the current Canvas host already do. This is the only honest
bounded answer; codex's restricted-token/firewall build is real but enormous and ships off by
default.

## Why v1 is high-confidence: the Linux mechanism dodges every distro trap

The research flagged one recurring Linux fragility across codex, `sandbox-runtime`, and Zed:
they all shell out to **bubblewrap**, and bubblewrap needs unprivileged **user namespaces**,
which Ubuntu 23.10–24.04 restrict by AppArmor policy (`kernel.apparmor_restrict_unprivileged_userns=1`
strips the caps, breaking both bwrap and any nested-userns seccomp helper). That is a real,
silent, distro-dependent enforcement hole.

**v1 avoids it entirely by not using bubblewrap or namespaces.** For the chosen scope, the three
containments are all reachable by mechanisms an unprivileged process applies *to itself*, with
no `CLONE_NEWUSER`:

- **`landlock`** is a stackable LSM. Any process calls `prctl(PR_SET_NO_NEW_PRIVS)` then
  `landlock_restrict_self()` to drop its own filesystem access rights — no capability, no
  namespace, kernel 5.13+. We grant read/write to the workspace, read to the system paths a
  command needs to run, and everything else (SSH keys, cloud creds, the rest of `$HOME`) is
  denied by default. The `landlock` crate's best-effort mode returns a `RestrictionStatus`
  reporting exactly which ABI got enforced (`Fully` vs `PartiallyEnforced`) — that report feeds
  the honest degradation envelope below.
- **`seccompiler`** compiles a BPF filter applied via `PR_SET_SECCOMP` — again self-applied, no
  namespace. It blocks `ptrace`/`process_vm_*` (anti-debug-escape), `io_uring_setup/enter/register`
  (closes the `IORING_OP_SOCKET` bypass that defeats naive socket filters), and — for network
  **off** — `socket()` for every family, so there is no TCP, no UDP, and therefore no DNS, with
  no network namespace required.
- **`setrlimit`** in the child's `pre_exec` caps process count (fork-bomb defense), address
  space, CPU seconds, and file size. Optionally a cgroup-v2 slice if the host grants one. This
  is the piece codex, `sandbox-runtime`, and Zed all skip — a fork bomb takes down the host in
  every one of them — and it is cheap for us to include.

All three are installed in the child between `fork` and `exec` (Rust `CommandExt::pre_exec` /
the crates' own restrict calls), so the workload is already contained the instant `sh -c` runs.
No external binary, no setuid helper, no distro-userns dependency. The userns trap only returns
in v2, when a real network **namespace** (for the proxy bridge) re-enters the picture — and that
is precisely why the proxy is deferred to its own phase where that dependency can be surfaced
honestly.

macOS has no in-process equivalent; the only actual mechanism is `sandbox-exec` (Seatbelt),
which we invoke by wrapping the command's argv with a generated deny-default profile. It is
Apple-deprecated (compile-time warning) but **functional on current macOS (Tahoe 26)** and used
in production by Chrome, codex, and `sandbox-runtime`. No host dependency to install, so macOS is
strict-or-throw with no degraded tier.

### Confidence, stated plainly

- **macOS + Linux filesystem/process isolation: HIGH.** We are re-deriving three proven
  designs, and **codex is Apache-2.0 — we can lift its Seatbelt SBPL profile and seccomp filter
  as code**, not merely as inspiration. The Rust primitives (`landlock` 0.4.5 by the kernel
  feature's own author; `seccompiler` 0.5.0, pure-Rust, no `libseccomp` link dependency) are
  maintained and exactly fit.
- **Resource limits: HIGH** and cheap — standard `setrlimit`.
- **Network on/off: HIGH** — "off" is the strongest guarantee we make (no reachable socket).
- **Network domain allowlist (v2): MEDIUM**, and isolated by design so its risk can't leak into
  v1.
- **Native Windows: no honest kernel path.** `off` + banner is not low confidence; it is the
  correct answer.

## The seam: `SandboxLauncher`, injected like the other host seams

AC already treats the sandbox as one of its five host seams (`SandboxPolicy` injection). Today
that seam is a hole — `ToolCtx` has no sandbox field and `shell` spawns `sh -c` directly. v1
fills it with a structural trait the runtime depends on abstractly, so `ac-sandbox` stays an
optional leaf crate and the runtime never imports it:

```rust
// in ac-tool (the ctx crate), so ac-runtime/ac-tools depend only on the trait:
pub trait SandboxLauncher: Send + Sync {
    /// Prepare a command for sandboxed execution. Returns the (possibly
    /// rewritten) command to spawn plus the mode actually achieved. Applying
    /// the sandbox may mean rewriting argv (macOS: `sandbox-exec -p … -- …`)
    /// and/or installing an in-process `pre_exec` hook (Linux: landlock +
    /// seccomp + rlimits). MUST fail closed: if the requested policy cannot be
    /// enforced and the caller asked for strict, return Err — never a weaker
    /// command silently.
    fn prepare(&self, cmd: SandboxCommand, policy: &SandboxPolicy)
        -> Result<Prepared, SandboxError>;
}

pub struct Prepared {
    pub command: tokio::process::Command, // spawn this instead of the raw one
    pub mode: SandboxMode,                // Strict | Degraded | Off — rides the result envelope
}
```

`ToolCtx` gains `pub sandbox: Option<Arc<dyn SandboxLauncher>>`. `shell` changes from "build a
`Command` and spawn it" to "build a `SandboxCommand`, ask `ctx.sandbox` to `prepare` it, spawn
the result" — the process-group/timeout/drain machinery it already has is unchanged. When
`ctx.sandbox` is `None`, `shell` refuses to register or runs in an explicit `Off` mode with the
banner (host's choice), never a silent unsandboxed spawn.

`SandboxPolicy` is the platform-neutral intent (borrowed shape from Zed's design, re-derived —
their code is GPL, never copied): allowed read roots, allowed write roots, a mandatory deny-set
independent of the allow-set, network mode (`Off | On`), and resource caps. The `ac-sandbox`
crate is the only place that translates intent into Seatbelt SBPL or landlock+seccomp+rlimit
calls. `ac-sandbox` depends on `ac-tool` (for the trait + `PathPolicy` types) and nothing else
in the workspace; it never imports `ac-runtime` or `ac-agent`. Wiring parallels the shell tool:
built-in registration stays app-agnostic, the host injects the launcher.

## Fail-closed contract (the rule that separates us from the one trap we found)

1. **No silent weakening, ever.** If a requested containment cannot be enforced, the launcher
   returns `Err` in strict mode. It never returns a command that runs with less isolation than
   asked without saying so. (This is the exact discipline `sandbox-runtime` violates for its
   unix-socket seccomp sublayer, which warns-and-proceeds — we treat every layer's absence as
   strict-mode-fatal unless the operator explicitly opted into a weaker mode.)
2. **Three honest modes on the result envelope.** `SandboxMode::{Strict, Degraded, Off}` rides
   the `shell` result JSON (as `sandbox.mode`), so a host UI can banner non-strict runs. `Strict`
   = every requested layer enforced (on Linux, `landlock` reported `Fully` and the seccomp
   filter installed). `Degraded` = kernel too old for full landlock ABI, or cgroups unavailable
   — a real-but-partial sandbox, surfaced. `Off` = native Windows, or the host explicitly
   disabled it — surfaced on every call.
3. **Two layers that both must hold.** The in-process `PathPolicy` containment check stays; the
   OS sandbox is defense-in-depth beneath it. Neither replaces the other.
4. **Detect, don't assume.** Use `landlock`'s `RestrictionStatus` to know what actually got
   enforced rather than assuming the kernel honored the request; downgrade the mode from the
   real report.

## Non-goals and acknowledged residual risk (v1)

Stated up front because an honest sandbox names its holes:

- **Reads are the operator's policy, not globally denied.** We deny a mandatory secret-set (SSH
  keys, cloud creds, `.git/hooks`, `.git/config`, shell rc files, `.mcp.json`) regardless of the
  allow-set, but a command with a network grant plus broad read access can still exfiltrate —
  the same posture Zed ships. Network-off closes the exfil channel entirely.
- **No domain-level network policy in v1** (that is v2). v1 network is all-or-nothing.
- **Inherited/`SCM_RIGHTS`-passed socket fds** are not neutralized by a `socket()` filter; this
  is a known seccomp limitation the references share. Acceptable for v1's threat model
  (untrusted model output driving a shell), documented, revisited with the v2 namespace work.
- **No mount/PID namespace in v1** — so `/proc` is visible. `landlock` contains FS *access*, not
  visibility. Full namespace isolation arrives with the v2 proxy bridge, which needs a netns
  anyway.
- **Native Windows is unsandboxed** and says so on every call.

None of these are pretense: each is a bounded, disclosed limitation of a real sandbox, not an
advisory mechanism masquerading as enforcement.

## Phase plan

**v1 (this RFC) — DONE:**
1. ✅ `SandboxLauncher` trait + `SandboxPolicy`/`SandboxMode`/`SandboxError`/`CommandSpec`/
   `Prepared` in `ac-tool`; `ToolCtx` gains an optional `sandbox` launcher (via `with_sandbox`,
   so the 13 existing `ToolCtx::new` sites are untouched). Shell tool rewired to
   `prepare`-then-spawn; `sandbox.mode` rides the result envelope; a `None` launcher keeps
   today's behavior (`mode: "off"`), and fail-closed refusal never falls back to an unsandboxed
   spawn.
2. ✅ `ac-sandbox` macOS: deny-default Seatbelt profile (base allow-set adapted from codex's
   Apache SBPL), argv wrapped with the pinned `/usr/bin/sandbox-exec`, paths passed as `-D`
   params (never interpolated into SBPL), `setrlimit` via `pre_exec`. Live-verified: 8 smoke
   tests spawn real `sandbox-exec` and prove write-escape/secret-read denial, the network gate,
   and `RLIMIT_FSIZE` truncation.
3. ✅ `ac-sandbox` Linux: `landlock` FS grants (system roots + read/write roots; secrets denied
   by omission) + `seccompiler` filter (ptrace/process_vm always; socket/socketpair/io_uring when
   network-off) + `setrlimit`, all self-applied in `pre_exec` after `PR_SET_NO_NEW_PRIVS` — no
   bwrap, no userns. A parent-side landlock probe decides `Strict` vs (fail-closed) refusal vs
   `Degraded`. Verified in a Linux container.
4. ✅ Three-mode `strict|degraded|off` envelope on the shell result; native Windows returns `Off`
   (or refuses under fail-closed). Wired into the CLI host (`--no-sandbox`, `--sandbox-no-network`;
   on by default in the `ac` binary), with a shipped-wiring test asserting `build_host` installs
   the launcher.

**v2 (separate RFC when we take it):** the egress allowlist — kernel-block-then-proxy. Sever all
egress (Linux netns + AF_UNIX bridge; macOS deny-default + localhost-only), a host-side
HTTP-CONNECT + SOCKS5 proxy that owns DNS (proxy-side resolution so no packet leaves the
sandbox), host canonicalization + control-char rejection before allowlist match (the null-byte
CVE class), and per-resolved-IP pinning against DNS rebinding. This is where the userns/distro
dependency returns and must be surfaced. `zerobox-network-proxy` (Rust, 2026) or hand-rolled
`hyper` + `fast-socks5` + `hickory-resolver` are the candidate bases.

**Not planned:** native Windows kernel sandboxing (bespoke Win32; revisit only if a consumer
demands native-Windows containment and accepts the cost).

## Rough size

macOS + Linux v1: **~2–3k LOC** (`ac-sandbox` ~1.5–2k: profile generation, landlock/seccomp/rlimit
setup, mode detection; plus ~300–500 LOC of seam plumbing in `ac-tool`/`ac-tools`). For
reference: `sandbox-runtime` is ~5k (with the v2-shaped proxy); Zed's is ~11–12k (three
platforms incl. WSL packaging + the escalation UX we are not building in v1).

## License discipline

- **Lift from codex (Apache-2.0):** the Seatbelt SBPL profile content and the seccomp syscall
  set are directly liftable with attribution. This is the cleanest source for policy content.
- **Zed (GPL-3.0-or-later): designs only, never code.** The `SandboxPolicy` intent shape, the
  capture-once path-identity idea, and the three-mode degradation UX are re-derived from their
  design, not copied. Do not paste from `crates/sandbox`.
- **`sandbox-runtime` (Apache-2.0, Node):** design reference for the kernel-funnel network model
  (v2); no code to lift (it's TypeScript + C).

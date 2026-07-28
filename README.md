# AC — Agent Core

App-agnostic AI agent runtime in Rust. Providers, skills, MCP, and hard built-in tools live in the kit; everything app-shaped is injected through seams. Host applications consume AC as a library — no crate in this workspace may know who its host is.

Read [CLAUDE.md](CLAUDE.md) for the architecture doctrine before touching code.

## Try it

```sh
# Offline end-to-end proof — no network, no API key. Drives the real built-in
# tools over the real runtime loop against a temp dir via a scripted provider.
cargo test -p ac-cli

# Live generic agent — reads/writes/searches files and runs shell, contained
# to <dir> (default: current directory).
OPENROUTER_API_KEY=... cargo run -p ac-cli -- [--model <id>] [--dir <path>] \
  [--skills <dir>] [--skill <name>] [--web-search] "your prompt"
```

`--skills <dir>` points at a directory scanned (recursively, bounded depth) for `SKILL.md` skills: valid skills are advertised in the system prompt as a catalog of `name: description (file: path)` entries (invalid candidates are reported on stderr with a reason), a `$name` mention in the prompt injects that skill's `SKILL.md` into the turn input as a `<skill>` block, and the model reads companion `scripts/`/`references/` itself at the listed paths — there is no skill tool (codex-style text injection). `--skill <name>` selects a skill up front, exactly as if the prompt mentioned `$name`. The reusable resolver also offers an optional ordered direct-child policy with directory-name shadowing; both modes parse real YAML frontmatter, including nested metadata, sequences, flow collections, and block scalars.

Status: the end-to-end agent loop works. `ac` is a generic filesystem agent — the OpenRouter provider wired to the built-in tool registry (`read_file`, `write_file`, `edit_file`, `list_files`, `glob`, `grep`, `shell`, `fetch`) over the `ac-runtime` loop, all writes contained by a path policy. Hosts that expose different tool schemas or result presentation reuse the same `FileTimes`/`FileMutation`, resolved-path read/list, fuzzy-replace, and `execute_shell` primitives rather than building another file or process runtime. Proven offline in `crates/ac-cli/tests/e2e.rs`, which exercises the whole stack (real tools, real loop, real temp dir) via a scripted mock provider and asserts both the on-disk ground truth and the policy safety invariant.

`ac-provider-openrouter` also exposes a typed image-generation client. It owns the provider request/response mechanics, decoding, remote-image download, cancellation, timeout, redirect handling, and response-size bounds; a host still owns its tool schema, reference-path authorization, naming, and output sink.

## License

Apache-2.0 — see [LICENSE](LICENSE).

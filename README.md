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
  [--skills <dir>] [--require-skill <name>] [--web-search] "your prompt"
```

`--skills <dir>` points at a directory whose subdirectories each hold a `SKILL.md`: valid skills are advertised in the system prompt and loadable via the `load_skill` tool (invalid candidates are reported on stderr with a reason). `--require-skill <name>` additionally forces the model to load that skill before doing anything else — a step hook pins the tool choice until the history shows the load happened.

Status: the end-to-end agent loop works. `ac` is a generic filesystem agent — the OpenRouter provider wired to the built-in tool registry (`read_file`, `write_file`, `edit_file`, `list_files`, `glob`, `grep`, `shell`, `fetch`) over the `ac-runtime` loop, all writes contained by a path policy. Proven offline in `crates/ac-cli/tests/e2e.rs`, which exercises the whole stack (real tools, real loop, real temp dir) via a scripted mock provider and asserts both the on-disk ground truth and the policy safety invariant.

## License

Apache-2.0 — see [LICENSE](LICENSE).

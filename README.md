# AC — Agent Core

App-agnostic AI agent runtime in Rust. Providers, skills, MCP, and hard built-in tools live in the kit; everything app-shaped is injected through seams. Host applications consume AC as a library — no crate in this workspace may know who its host is.

Read [CLAUDE.md](CLAUDE.md) for the architecture doctrine before touching code.

```sh
cargo check                      # the whole workspace
OPENROUTER_API_KEY=... cargo run -p ac-cli -- "hello"   # streaming smoke
```

Status: early — the phase-1 provider slice (types, provider trait, OpenRouter wire client, smoke CLI) is live; the remaining crates are committed as placeholders that document the topology.

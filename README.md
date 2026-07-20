# AC — Agent Core

App-agnostic AI agent runtime in Rust. Providers, skills, MCP, and hard built-in tools in the kit; everything app-shaped injected through seams. Canvas is the first consumer (via git submodule), but no crate in this workspace may know that.

Read [CLAUDE.md](CLAUDE.md) for the architecture doctrine before touching code.

```sh
cargo check                      # the whole workspace
OPENROUTER_API_KEY=... cargo run -p ac-cli -- "hello"   # streaming smoke
```

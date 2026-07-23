# ac-web-react — the AI SDK demo

A **stock [`@ai-sdk/react`](https://ai-sdk.dev) `useChat` app** that renders an
AC agent. Nothing in `src/` knows about AC, ACP, or Rust — it speaks the Vercel
AI SDK **UI Message Stream Protocol** to `/api/chat`, and the AC host
(`crates/ac-ai-sdk`) emits that protocol natively from its turn stream.

That's the whole point: the web ecosystem's own React hook drives an AC agent
unchanged. AC replaces the AI SDK's *server/provider* half (it is the agent
runtime); this demo consumes its *client/UI* half.

The UI is built from [ai-elements](https://ai-sdk.dev/elements) components
(vendored under `src/components/ai-elements/`) on Tailwind v4 + shadcn, with
custom per-tool cards: a terminal view for `shell`, file cards for
`write_file`/`edit_file`, compact rows for `read_file`/`list_files`/`glob`/
`grep`/`fetch`, an image card for `image_gen`, and a generic collapsible
fallback for anything else (which is how future tools render with zero UI
work). Reasoning, web-search citations, per-message token usage, and transient
context-compaction notices all render from the same stream.

## Run it

Two processes — the Rust host and the Vite dev server.

```sh
# 1. the AC host (emits the AI SDK UI Message Stream Protocol)
OPENROUTER_API_KEY=sk-or-... \
  cargo run -p ac-ai-sdk -- --dir /path/to/a/sandbox/dir --web-search --image-gen

# 2. the React app (proxies /api → the host)
cd examples/web-react
pnpm install
pnpm dev            # http://localhost:5173
```

- `--web-search` enables the provider's web search; citations show up as a
  per-message "sources" block.
- `--image-gen` registers the `image_gen` tool; generated images are written
  into the workspace and rendered inline.

The agent's file tools act inside `--dir`. Sessions persist to
`~/.ac/ac-ai-sdk/<hash>.db` (outside the sandbox), so recents and resume work
across restarts.

### Workspace files over HTTP

The host serves workspace files at `GET /api/files/<relative path>` (path
containment enforced host-side). The Vite dev server proxies `/api/files`
alongside the rest of `/api`, which is how the `image_gen` card loads its
`<img>` — and clicking an image opens the file in a new tab.

## Prove the integration without the UI

`verify.mjs` drives the host with the AI SDK's own client code — the exact
`DefaultChatTransport` + `readUIMessageStream` path `useChat` runs internally —
and asserts it reconstructs a `UIMessage` with text and tool parts:

```sh
node verify.mjs      # with the host running on :8790
```

## How the pieces map

| AC (`AgentEvent`) | AI SDK chunk |
| --- | --- |
| `Text` | `text-start` / `text-delta` / `text-end` |
| `Thinking` | `reasoning-*` |
| `ToolCall` | `tool-input-start` / `tool-input-delta` + `tool-input-available` |
| `ToolResult` | `tool-output-available` / `tool-output-error` |
| `Citation` | `source-url` |
| `Usage` | `message-metadata` |
| `Compacted` | `data-compaction` (transient) |

The host owns history (the AI SDK "message persistence" pattern): the client
sends only the latest message plus a chat id; the host loads the rest from
`ac-store`, keyed by that id. Resumed sessions hydrate through
`GET /api/sessions/:id` and render through the exact same part mapping as live
streams — tool parts simply arrive already in their terminal state.

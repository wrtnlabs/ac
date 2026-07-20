# ac-web-react — the AI SDK demo

A **stock [`@ai-sdk/react`](https://ai-sdk.dev) `useChat` app** that renders an
AC agent. Nothing in `src/` knows about AC, ACP, or Rust — it speaks the Vercel
AI SDK v5 **UI Message Stream Protocol** to `/api/chat`, and the AC host
(`crates/ac-ai-sdk`) emits that protocol natively from its turn stream.

That's the whole point: the web ecosystem's own React hook drives an AC agent
unchanged. AC replaces the AI SDK's *server/provider* half (it is the agent
runtime); this demo consumes its *client/UI* half.

## Run it

Two processes — the Rust host and the Vite dev server.

```sh
# 1. the AC host (emits the AI SDK UI Message Stream Protocol)
OPENROUTER_API_KEY=sk-or-... \
  cargo run -p ac-ai-sdk -- --dir /path/to/a/sandbox/dir --web-search

# 2. the React app (proxies /api → the host)
cd examples/web-react
pnpm install
pnpm dev            # http://localhost:5173
```

The agent's file tools act inside `--dir`. Sessions persist to
`~/.ac/ac-ai-sdk/<hash>.db` (outside the sandbox), so recents and resume work
across restarts.

## Prove the integration without the UI

`verify.mjs` drives the host with the AI SDK's own client code — the exact
`DefaultChatTransport` + `readUIMessageStream` path `useChat` runs internally —
and asserts it reconstructs a `UIMessage` with text and tool parts:

```sh
node verify.mjs      # with the host running on :8790
```

## How the pieces map

| AC (`AgentEvent`) | AI SDK v5 chunk |
| --- | --- |
| `Text` | `text-start` / `text-delta` / `text-end` |
| `Thinking` | `reasoning-*` |
| `ToolCall` | `tool-input-start` + `tool-input-available` |
| `ToolResult` | `tool-output-available` / `tool-output-error` |
| `Citation` | `source-url` |
| `Usage` | `message-metadata` |

The host owns history (the AI SDK "message persistence" pattern): the client
sends only the latest message plus a chat id; the host loads the rest from
`ac-store`, keyed by that id.

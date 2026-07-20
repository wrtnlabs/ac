import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport, type UIMessage, type UIMessagePart } from "ai";

// This whole app is a STOCK @ai-sdk/react client. Nothing here knows about AC,
// ACP, or Rust — it speaks the AI SDK UI Message Stream Protocol to `/api/chat`
// and renders `UIMessage` parts. The AC host on the other end emits that
// protocol natively. That's the point of the demo: the ecosystem's own React
// hook drives an AC agent unchanged.

type SessionRow = { id: string; title: string | null; updatedAtMs: number };

export function App() {
  const [model, setModel] = useState("");
  const [sessions, setSessions] = useState<SessionRow[]>([]);
  const [activeId, setActiveId] = useState<string>(() => crypto.randomUUID());
  const [initialMessages, setInitialMessages] = useState<UIMessage[]>([]);

  const refreshSessions = useCallback(async () => {
    const res = await fetch("/api/sessions");
    const json = await res.json();
    setSessions(json.sessions ?? []);
  }, []);

  useEffect(() => {
    fetch("/api/config")
      .then((r) => r.json())
      .then((c) => setModel(c.model))
      .catch(() => {});
    refreshSessions();
  }, [refreshSessions]);

  // Monotonic token so a slow /api/sessions/:id fetch can't clobber a newer
  // selection (last-clicked wins, not last-resolved).
  const openToken = useRef(0);

  const newChat = useCallback(() => {
    openToken.current += 1;
    setInitialMessages([]);
    setActiveId(crypto.randomUUID());
  }, []);

  const openSession = useCallback(async (id: string) => {
    const token = ++openToken.current;
    const res = await fetch(`/api/sessions/${id}`);
    const json = await res.json();
    if (token !== openToken.current) return; // a newer open superseded this one
    setInitialMessages((json.messages ?? []) as UIMessage[]);
    setActiveId(id);
  }, []);

  return (
    <div className="app">
      <aside>
        <header>
          <h1>
            AC <span>react demo</span>
          </h1>
          <div className="sub">a stock @ai-sdk/react client</div>
        </header>
        <button className="new-chat" onClick={newChat}>
          ＋ New chat
        </button>
        <div className="sessions">
          {sessions.map((s) => (
            <button
              key={s.id}
              className={"session" + (s.id === activeId ? " active" : "")}
              onClick={() => openSession(s.id)}
            >
              <span className="title">{s.title || s.id.slice(0, 8) + "…"}</span>
              <span className="when">{new Date(s.updatedAtMs).toLocaleString()}</span>
            </button>
          ))}
        </div>
      </aside>
      {/* key=activeId remounts useChat cleanly when switching chats */}
      <Chat
        key={activeId}
        chatId={activeId}
        initialMessages={initialMessages}
        model={model}
        onTurnEnd={refreshSessions}
      />
    </div>
  );
}

function Chat({
  chatId,
  initialMessages,
  model,
  onTurnEnd,
}: {
  chatId: string;
  initialMessages: UIMessage[];
  model: string;
  onTurnEnd: () => void;
}) {
  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: "/api/chat",
        // Send only the latest message + the chat id: the AC host owns history
        // (the AI SDK "message persistence" pattern), so ac-store is the source
        // of truth and resume works.
        prepareSendMessagesRequest({ id, messages, trigger, messageId }) {
          return {
            body: { id, message: messages[messages.length - 1], trigger, messageId },
          };
        },
      }),
    [],
  );

  const { messages, sendMessage, status, stop, error } = useChat({
    id: chatId,
    messages: initialMessages,
    transport,
    onFinish: onTurnEnd,
  });

  const [input, setInput] = useState("");
  const busy = status === "submitted" || status === "streaming";
  const transcriptRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = transcriptRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, status]);

  const submit = () => {
    const text = input.trim();
    if (!text || busy) return;
    setInput("");
    sendMessage({ text });
  };

  return (
    <main>
      <div className="topbar">
        <span className={"dot " + status} />
        <span className="mono">{model}</span>
        <span className="mono id">{chatId.slice(0, 8)}…</span>
      </div>
      <div className="transcript" ref={transcriptRef}>
        <div className="lane">
          {messages.map((m) => (
            <MessageView key={m.id} message={m} />
          ))}
          {error && <div className="errblock">✗ {error.message}</div>}
        </div>
      </div>
      <div className="composer">
        <div className="lane row">
          <textarea
            value={input}
            placeholder="Ask the agent — it works inside the sandbox directory"
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
          />
          {busy ? (
            <button className="act stop" onClick={stop}>
              Stop
            </button>
          ) : (
            <button className="act" onClick={submit}>
              Send
            </button>
          )}
        </div>
      </div>
    </main>
  );
}

function MessageView({ message }: { message: UIMessage }) {
  const who = message.role === "user" ? "you" : "agent";
  return (
    <div className={"block " + message.role}>
      <div className="who">{who}</div>
      {message.role === "user" ? (
        <div className="bubble">{textOf(message)}</div>
      ) : (
        message.parts.map((part, i) => <PartView key={i} part={part} />)
      )}
    </div>
  );
}

function PartView({ part }: { part: UIMessagePart<any, any> }) {
  if (part.type === "text") {
    return <div className="text">{part.text}</div>;
  }
  if (part.type === "reasoning") {
    return (
      <details className="thought">
        <summary>thinking</summary>
        <div className="t">{part.text}</div>
      </details>
    );
  }
  if (part.type === "source-url") {
    // Provider-controlled URL — only render web schemes so a javascript: URL
    // can't execute in this origin.
    let href: string | null = null;
    try {
      const u = new URL(part.url);
      if (u.protocol === "http:" || u.protocol === "https:") href = u.href;
    } catch {
      href = null;
    }
    if (!href) return null;
    return (
      <a className="citation" href={href} target="_blank" rel="noreferrer">
        🔎 {part.title || part.url}
      </a>
    );
  }
  if (part.type === "dynamic-tool" || part.type.startsWith("tool-")) {
    return <ToolView part={part} />;
  }
  return null;
}

function ToolView({ part }: { part: any }) {
  const name: string = part.toolName ?? part.type.replace(/^tool-/, "");
  const state: string = part.state ?? "input-available";
  const output = state === "output-available" ? part.output : undefined;
  const error = state === "output-error" ? part.errorText : undefined;
  const status = error ? "failed" : output !== undefined ? "completed" : "running";
  return (
    <div className="tool">
      <div className="head">
        <span className="name">{name}</span>
        <span className={"chip " + status}>{status}</span>
      </div>
      {part.input !== undefined && (
        <pre>{JSON.stringify(part.input, null, 1)}</pre>
      )}
      {output !== undefined && <pre>{render(output)}</pre>}
      {error !== undefined && <pre className="err">{error}</pre>}
    </div>
  );
}

function render(value: unknown): string {
  const s = typeof value === "string" ? value : JSON.stringify(value, null, 1);
  return s.length > 4000 ? s.slice(0, 4000) + "\n…" : s;
}

function textOf(message: UIMessage): string {
  return message.parts
    .filter((p): p is Extract<typeof p, { type: "text" }> => p.type === "text")
    .map((p) => p.text)
    .join("");
}

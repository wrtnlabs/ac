import { Chat } from "@/components/chat";
import { Button } from "@/components/ui/button";
import { timeAgo } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { UIMessage } from "ai";
import { PlusIcon } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

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
    try {
      const res = await fetch("/api/sessions");
      const json = await res.json();
      setSessions(json.sessions ?? []);
    } catch {
      // host not up yet — keep whatever we have
    }
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
    try {
      const res = await fetch(`/api/sessions/${id}`);
      const json = await res.json();
      if (token !== openToken.current) return; // a newer open superseded this one
      setInitialMessages((json.messages ?? []) as UIMessage[]);
      setActiveId(id);
    } catch {
      // hydration failed — stay on the current chat
    }
  }, []);

  return (
    <div className="flex h-full overflow-hidden bg-background text-foreground">
      <aside className="flex w-64 shrink-0 flex-col border-r">
        <header className="border-b p-4">
          <h1 className="font-semibold text-base">
            AC <span className="font-normal text-muted-foreground">showcase</span>
          </h1>
          <p className="text-muted-foreground text-xs">
            a stock @ai-sdk/react client
          </p>
        </header>
        <div className="p-3">
          <Button className="w-full" onClick={newChat} variant="outline">
            <PlusIcon className="size-4" />
            New chat
          </Button>
        </div>
        <nav className="flex-1 space-y-1 overflow-y-auto px-3 pb-3">
          {sessions.map((s) => (
            <button
              className={cn(
                "flex w-full flex-col gap-0.5 rounded-md px-3 py-2 text-left text-sm transition-colors hover:bg-muted/50",
                s.id === activeId && "bg-accent text-accent-foreground",
              )}
              key={s.id}
              onClick={() => openSession(s.id)}
              type="button"
            >
              <span className="truncate">
                {s.title || s.id.slice(0, 8) + "…"}
              </span>
              <span className="text-muted-foreground text-xs">
                {timeAgo(s.updatedAtMs)}
              </span>
            </button>
          ))}
          {sessions.length === 0 && (
            <p className="px-3 py-2 text-muted-foreground text-xs">
              No chats yet
            </p>
          )}
        </nav>
      </aside>
      {/* key=activeId remounts useChat cleanly when switching chats */}
      <Chat
        chatId={activeId}
        initialMessages={initialMessages}
        key={activeId}
        model={model}
        onTurnEnd={refreshSessions}
      />
    </div>
  );
}

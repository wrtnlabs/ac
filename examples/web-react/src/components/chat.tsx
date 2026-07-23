import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import {
  PromptInput,
  PromptInputBody,
  PromptInputFooter,
  PromptInputSubmit,
  PromptInputTextarea,
  PromptInputTools,
} from "@/components/ai-elements/prompt-input";
import { Shimmer } from "@/components/ai-elements/shimmer";
import { MessageView } from "@/components/message-view";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { asRecord, formatTokens, num } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport, type UIMessage } from "ai";
import { ArchiveIcon, SparklesIcon, XIcon } from "lucide-react";
import { useMemo, useState } from "react";

const EXAMPLE_PROMPTS = [
  {
    label: "Research with web search",
    prompt:
      "Search the web for the latest developments in small language models and summarize the three most interesting ones with sources.",
  },
  {
    label: "Generate an image",
    prompt:
      "Generate an image of an origami crane on a wooden desk at golden hour.",
  },
  {
    label: "Explore files + shell",
    prompt:
      "List the files in the workspace, then use the shell to count how many lines each one has.",
  },
];

type Compaction = { tokensBefore?: number; tokensAfter?: number };

export function Chat({
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
        // Send only the latest message + the chat id: the host owns history
        // (the AI SDK "message persistence" pattern), so the server store is
        // the source of truth and resume works.
        prepareSendMessagesRequest({ id, messages, trigger, messageId }) {
          return {
            body: { id, message: messages[messages.length - 1], trigger, messageId },
          };
        },
      }),
    [],
  );

  const [compaction, setCompaction] = useState<Compaction | null>(null);

  const { messages, sendMessage, status, stop, error } = useChat({
    id: chatId,
    messages: initialMessages,
    transport,
    onFinish: onTurnEnd,
    onData: (part) => {
      // Transient notification — it never enters the persisted message.
      if (part.type === "data-compaction") {
        const d = asRecord(part.data);
        setCompaction({
          tokensBefore: num(d.tokensBefore),
          tokensAfter: num(d.tokensAfter),
        });
      }
    },
  });

  const busy = status === "submitted" || status === "streaming";
  const lastMessage = messages[messages.length - 1];
  const awaitingFirstChunk =
    status === "submitted" ||
    (status === "streaming" && lastMessage?.role === "user");

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <header className="flex h-12 shrink-0 items-center gap-3 border-b px-4 text-sm">
        <span
          className={cn(
            "size-2 shrink-0 rounded-full",
            busy ? "animate-pulse bg-green-500" : "bg-muted-foreground/40",
          )}
          title={status}
        />
        <span className="truncate font-mono text-muted-foreground text-xs">
          {model || "…"}
        </span>
        <span className="ml-auto shrink-0 font-mono text-muted-foreground/60 text-xs">
          {chatId.slice(0, 8)}…
        </span>
      </header>

      <Conversation>
        <ConversationContent className="mx-auto w-full max-w-3xl">
          {messages.length === 0 ? (
            <ConversationEmptyState className="min-h-[60vh]">
              <SparklesIcon className="size-8 text-muted-foreground" />
              <div className="space-y-1">
                <h2 className="font-semibold text-lg">What should we build?</h2>
                <p className="text-muted-foreground text-sm">
                  The agent can search the web, generate images, and work on
                  files inside its sandbox directory.
                </p>
              </div>
              <div className="mt-2 flex flex-col gap-2">
                {EXAMPLE_PROMPTS.map((ex) => (
                  <Button
                    className="h-auto whitespace-normal text-left"
                    key={ex.label}
                    onClick={() => sendMessage({ text: ex.prompt })}
                    variant="outline"
                  >
                    <span className="font-medium">{ex.label}</span>
                    <span className="text-muted-foreground">·</span>
                    <span className="text-muted-foreground text-xs">
                      {ex.prompt}
                    </span>
                  </Button>
                ))}
              </div>
            </ConversationEmptyState>
          ) : (
            <>
              {messages.map((m) => (
                <MessageView key={m.id} message={m} />
              ))}
              {awaitingFirstChunk && (
                <div className="flex items-center gap-2 text-muted-foreground text-sm">
                  <Spinner />
                  <Shimmer duration={1.5}>Working…</Shimmer>
                </div>
              )}
              {error && (
                <div className="flex items-start gap-2 rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-destructive text-sm">
                  <XIcon className="mt-0.5 size-4 shrink-0" />
                  <span className="min-w-0 break-words">{error.message}</span>
                </div>
              )}
            </>
          )}
        </ConversationContent>
        <ConversationScrollButton />
      </Conversation>

      <div className="mx-auto w-full max-w-3xl px-4 pb-4">
        {compaction && (
          <div className="mb-2 flex items-center gap-2 rounded-md border bg-muted/50 px-3 py-1.5 text-muted-foreground text-xs">
            <ArchiveIcon className="size-3.5 shrink-0" />
            <span>
              context compacted
              {compaction.tokensBefore !== undefined &&
              compaction.tokensAfter !== undefined
                ? ` (${formatTokens(compaction.tokensBefore)} → ${formatTokens(compaction.tokensAfter)} tokens)`
                : ""}
            </span>
            <button
              aria-label="Dismiss"
              className="ml-auto rounded p-0.5 hover:bg-muted"
              onClick={() => setCompaction(null)}
              type="button"
            >
              <XIcon className="size-3.5" />
            </button>
          </div>
        )}
        <PromptInput
          onSubmit={(message) => {
            const text = message.text.trim();
            if (!text || busy) return;
            sendMessage({ text });
          }}
        >
          <PromptInputBody>
            <PromptInputTextarea placeholder="Ask the agent — it works inside its sandbox directory" />
          </PromptInputBody>
          <PromptInputFooter>
            <PromptInputTools />
            <PromptInputSubmit onStop={stop} status={status} />
          </PromptInputFooter>
        </PromptInput>
      </div>
    </main>
  );
}

import {
  Message,
  MessageContent,
  MessageResponse,
} from "@/components/ai-elements/message";
import {
  Reasoning,
  ReasoningContent,
  ReasoningTrigger,
} from "@/components/ai-elements/reasoning";
import {
  Source,
  Sources,
  SourcesContent,
  SourcesTrigger,
} from "@/components/ai-elements/sources";
import { ToolPartView, type ToolPartLike } from "@/components/tool-views";
import { asRecord, formatTokens, num } from "@/lib/format";
import type { UIMessage } from "ai";
import { ChevronDownIcon } from "lucide-react";

type Part = UIMessage["parts"][number];

// Provider-controlled URL — only render web schemes so a `javascript:` URL
// can't execute in this origin.
function safeHttpUrl(raw: string): string | null {
  try {
    const u = new URL(raw);
    return u.protocol === "http:" || u.protocol === "https:" ? u.href : null;
  } catch {
    return null;
  }
}

function faviconFor(href: string): string | null {
  try {
    return new URL(href).origin + "/favicon.ico";
  } catch {
    return null;
  }
}

function hostOf(href: string): string {
  try {
    return new URL(href).host;
  } catch {
    return href;
  }
}

function UsageChip({ metadata }: { metadata: unknown }) {
  const usage = asRecord(asRecord(metadata).usage);
  const input = num(usage.inputTokens);
  const output = num(usage.outputTokens);
  const cacheRead = num(usage.cacheReadTokens);
  if (input === undefined && output === undefined) return null;
  return (
    <div className="flex items-center gap-2 text-muted-foreground/70 text-xs">
      {input !== undefined && <span>↑ {formatTokens(input)}</span>}
      {output !== undefined && <span>↓ {formatTokens(output)}</span>}
      {cacheRead !== undefined && cacheRead > 0 && (
        <span>cache {formatTokens(cacheRead)}</span>
      )}
    </div>
  );
}

function SourcesBlock({ parts }: { parts: Part[] }) {
  const sources: { href: string; title?: string }[] = [];
  for (const p of parts) {
    if (p.type !== "source-url") continue;
    const rec = asRecord(p);
    const href = safeHttpUrl(typeof rec.url === "string" ? rec.url : "");
    if (!href) continue;
    const title = typeof rec.title === "string" ? rec.title : undefined;
    sources.push({ href, title });
  }

  if (sources.length === 0) return null;

  return (
    <Sources>
      <SourcesTrigger count={sources.length}>
        <p className="font-medium">
          Used {sources.length} {sources.length === 1 ? "source" : "sources"}
        </p>
        <ChevronDownIcon className="h-4 w-4" />
      </SourcesTrigger>
      <SourcesContent>
        {sources.map((s, i) => (
          <Source href={s.href} key={i} title={s.title ?? hostOf(s.href)}>
            {faviconFor(s.href) && (
              <img
                alt=""
                className="size-4 rounded-sm"
                loading="lazy"
                onError={(e) => {
                  e.currentTarget.style.display = "none";
                }}
                src={faviconFor(s.href) ?? undefined}
              />
            )}
            <span className="font-medium">{s.title ?? hostOf(s.href)}</span>
            <span className="text-muted-foreground">{hostOf(s.href)}</span>
          </Source>
        ))}
      </SourcesContent>
    </Sources>
  );
}

function AssistantPart({ part }: { part: Part }) {
  if (part.type === "text") {
    return <MessageResponse>{part.text}</MessageResponse>;
  }
  if (part.type === "reasoning") {
    const streaming = asRecord(part).state === "streaming";
    return (
      <Reasoning isStreaming={streaming}>
        <ReasoningTrigger />
        <ReasoningContent>{part.text}</ReasoningContent>
      </Reasoning>
    );
  }
  if (part.type === "dynamic-tool" || part.type.startsWith("tool-")) {
    return <ToolPartView part={part as ToolPartLike} />;
  }
  return null;
}

export function MessageView({ message }: { message: UIMessage }) {
  if (message.role === "user") {
    const text = message.parts
      .filter((p): p is Extract<Part, { type: "text" }> => p.type === "text")
      .map((p) => p.text)
      .join("");
    return (
      <Message from="user">
        <MessageContent>{text}</MessageContent>
      </Message>
    );
  }

  return (
    <Message from={message.role}>
      <MessageContent className="w-full">
        <SourcesBlock parts={message.parts} />
        {message.parts.map((part, i) =>
          part.type === "source-url" ? null : (
            <AssistantPart key={i} part={part} />
          ),
        )}
      </MessageContent>
      <UsageChip metadata={message.metadata} />
    </Message>
  );
}

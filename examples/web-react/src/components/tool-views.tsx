import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Tool,
  ToolContent,
  ToolHeader,
  ToolInput,
  ToolOutput,
} from "@/components/ai-elements/tool";
import { Shimmer } from "@/components/ai-elements/shimmer";
import {
  asRecord,
  fileUrl,
  firstLine,
  formatBytes,
  lineCount,
  str,
  truncate,
  utf8Bytes,
} from "@/lib/format";
import { cn } from "@/lib/utils";
import type { DynamicToolUIPart } from "ai";
import {
  ChevronDownIcon,
  FilePenIcon,
  FilePlusIcon,
  FileTextIcon,
  FolderIcon,
  GlobeIcon,
  ImageIcon,
  SearchIcon,
  TerminalIcon,
  TextSearchIcon,
  TriangleAlertIcon,
} from "lucide-react";
import type { ReactNode } from "react";

// Tool parts arrive as `dynamic-tool` parts (the host's tools are not typed
// client-side). Everything below guards against missing fields: during
// `input-streaming` the input object is progressively parsed and any field
// may be absent. Hydrated (resumed) parts arrive directly in a terminal
// state through the exact same code path.
export type ToolPartLike = {
  type: string;
  toolName?: string;
  state?: string;
  input?: unknown;
  output?: unknown;
  errorText?: string;
};

type ToolState = DynamicToolUIPart["state"];

const KNOWN_STATES: ReadonlySet<string> = new Set([
  "input-streaming",
  "input-available",
  "output-available",
  "output-error",
  "approval-requested",
  "approval-responded",
  "output-denied",
]);

function toolState(part: ToolPartLike): ToolState {
  const s = part.state;
  return (s && KNOWN_STATES.has(s) ? s : "input-available") as ToolState;
}

function toolName(part: ToolPartLike): string {
  return part.toolName ?? part.type.replace(/^tool-/, "");
}

function outputText(part: ToolPartLike): string | undefined {
  // Tool outputs are strings on the wire; anything else is stringified.
  if (part.output === undefined || part.output === null) return undefined;
  return typeof part.output === "string"
    ? part.output
    : JSON.stringify(part.output, null, 2);
}

export function ToolPartView({ part }: { part: ToolPartLike }) {
  switch (toolName(part)) {
    case "image_gen":
      return <ImageGenCard part={part} />;
    case "shell":
      return <ShellCard part={part} />;
    case "write_file":
    case "edit_file":
      return <FileChangeCard part={part} />;
    case "read_file":
    case "list_files":
    case "glob":
    case "grep":
    case "fetch":
      return <CompactToolRow part={part} />;
    default:
      return <GenericToolView part={part} />;
  }
}

// The fallback path: any tool without a dedicated card (including future
// ones) renders through the generic ai-elements Tool collapsible.
function GenericToolView({ part }: { part: ToolPartLike }) {
  const state = toolState(part);
  return (
    <Tool defaultOpen={state === "output-error"}>
      <ToolHeader state={state} toolName={toolName(part)} type="dynamic-tool" />
      <ToolContent>
        {part.input !== undefined && <ToolInput input={part.input} />}
        <ToolOutput
          errorText={part.errorText}
          output={outputText(part)}
        />
      </ToolContent>
    </Tool>
  );
}

// ---------------------------------------------------------------- image_gen

type ImageInfo = { path: string; mimeType?: string; bytes?: number };

function parseImageOutput(part: ToolPartLike): ImageInfo | null {
  const raw = outputText(part);
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    const rec = asRecord(parsed);
    const path = str(rec.path);
    if (!path) return null;
    return {
      path,
      mimeType: str(rec.mimeType),
      bytes: typeof rec.bytes === "number" ? rec.bytes : undefined,
    };
  } catch {
    return null;
  }
}

function ImageGenCard({ part }: { part: ToolPartLike }) {
  const state = toolState(part);
  const prompt = str(asRecord(part.input).prompt);
  const image = state === "output-available" ? parseImageOutput(part) : null;
  const pending = state === "input-streaming" || state === "input-available";

  return (
    <div className="mb-4 w-full max-w-md overflow-hidden rounded-lg border bg-card">
      <div className="flex items-center gap-2 border-b px-3 py-2 text-muted-foreground text-xs">
        <ImageIcon className="size-3.5" />
        <span className="font-medium">
          {state === "output-available"
            ? "Generated image"
            : state === "output-error"
              ? "Image generation failed"
              : "Generating image"}
        </span>
        {pending && <Shimmer className="ml-auto" duration={1.5}>working…</Shimmer>}
      </div>
      {pending && (
        <div className="aspect-video w-full animate-pulse bg-muted" />
      )}
      {state === "output-error" && (
        <div className="flex items-start gap-2 p-3 text-destructive text-sm">
          <TriangleAlertIcon className="mt-0.5 size-4 shrink-0" />
          <span className="min-w-0 break-words">
            {part.errorText || "Image generation failed"}
          </span>
        </div>
      )}
      {state === "output-available" &&
        (image ? (
          <a
            href={fileUrl(image.path)}
            rel="noreferrer"
            target="_blank"
            title="Open full size"
          >
            <img
              alt={prompt || image.path}
              className="block w-full"
              loading="lazy"
              src={fileUrl(image.path)}
            />
          </a>
        ) : (
          <pre className="overflow-x-auto p-3 text-xs">{outputText(part)}</pre>
        ))}
      {(prompt || image) && (
        <div className="flex items-baseline justify-between gap-2 px-3 py-2 text-xs">
          <span className="min-w-0 break-words text-muted-foreground italic">
            {prompt}
          </span>
          {image?.bytes !== undefined && (
            <span className="shrink-0 text-muted-foreground/70">
              {formatBytes(image.bytes)}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

// -------------------------------------------------------------------- shell

// Deliberately dark in both themes — it's a terminal.
function ShellCard({ part }: { part: ToolPartLike }) {
  const state = toolState(part);
  const command = str(asRecord(part.input).command);
  const out = outputText(part);
  const running = state === "input-streaming" || state === "input-available";
  const failed = state === "output-error";

  return (
    <div
      className={cn(
        "mb-4 w-full overflow-hidden rounded-lg border bg-zinc-950 font-mono text-xs",
        failed ? "border-red-800" : "border-zinc-800",
      )}
    >
      <div className="flex items-center gap-2 border-b border-zinc-800 px-3 py-2 text-zinc-400">
        <TerminalIcon className="size-3.5" />
        <span>shell</span>
        <span
          className={cn(
            "ml-auto flex items-center gap-1.5",
            failed ? "text-red-400" : running ? "text-yellow-400" : "text-green-400",
          )}
        >
          <span
            className={cn(
              "size-1.5 rounded-full bg-current",
              running && "animate-pulse",
            )}
          />
          {failed ? "error" : running ? "running" : "done"}
        </span>
      </div>
      <div className="px-3 py-2 text-zinc-100">
        <span className="select-none text-zinc-500">$ </span>
        <span className="whitespace-pre-wrap break-all">{command ?? "…"}</span>
      </div>
      {(out || part.errorText) && (
        <pre
          className={cn(
            "max-h-64 overflow-auto border-t border-zinc-800 px-3 py-2",
            failed ? "text-red-300" : "text-zinc-300",
          )}
        >
          {truncate(part.errorText || out || "", 8000)}
        </pre>
      )}
    </div>
  );
}

// -------------------------------------------- write_file / edit_file (cards)

function FileChangeCard({ part }: { part: ToolPartLike }) {
  const state = toolState(part);
  const name = toolName(part);
  const input = asRecord(part.input);
  const path = str(input.path) ?? str(input.file_path);
  const content = str(input.content);
  const oldStr = str(input.old_string) ?? str(input.old) ?? str(input.old_str);
  const newStr = str(input.new_string) ?? str(input.new) ?? str(input.new_str);
  const failed = state === "output-error";
  const done = state === "output-available";
  const isWrite = name === "write_file";

  let summary: string;
  if (failed) summary = "failed";
  else if (!done) summary = isWrite ? "writing…" : "editing…";
  else if (isWrite && content !== undefined)
    summary = `${formatBytes(utf8Bytes(content))} written`;
  else summary = isWrite ? "written" : "edit applied";

  const Icon = isWrite ? FilePlusIcon : FilePenIcon;

  return (
    <Collapsible
      className={cn(
        "group mb-4 w-full rounded-lg border bg-card",
        failed && "border-destructive/50",
      )}
      defaultOpen={failed}
    >
      <CollapsibleTrigger className="flex w-full items-center gap-2 px-3 py-2 text-sm">
        <Icon className="size-4 shrink-0 text-muted-foreground" />
        <span className="min-w-0 truncate font-mono text-xs">
          {path ?? "…"}
        </span>
        <span
          className={cn(
            "ml-auto shrink-0 text-xs",
            failed ? "text-destructive" : "text-muted-foreground",
          )}
        >
          {summary}
        </span>
        <ChevronDownIcon className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
      </CollapsibleTrigger>
      <CollapsibleContent className="space-y-2 border-t px-3 py-2">
        {failed && (
          <div className="text-destructive text-xs">{part.errorText}</div>
        )}
        {content !== undefined && (
          <pre className="max-h-64 overflow-auto rounded-md bg-muted/50 p-2 text-xs">
            {truncate(content, 8000)}
          </pre>
        )}
        {oldStr !== undefined && (
          <pre className="max-h-40 overflow-auto rounded-md bg-red-500/10 p-2 text-xs">
            {truncate(oldStr, 4000)}
          </pre>
        )}
        {newStr !== undefined && (
          <pre className="max-h-40 overflow-auto rounded-md bg-green-500/10 p-2 text-xs">
            {truncate(newStr, 4000)}
          </pre>
        )}
        {content === undefined &&
          oldStr === undefined &&
          newStr === undefined &&
          !failed && (
            <div className="text-muted-foreground text-xs">
              waiting for input…
            </div>
          )}
      </CollapsibleContent>
    </Collapsible>
  );
}

// ---------------- read_file / list_files / glob / grep / fetch (compact rows)

const ROW_META: Record<
  string,
  { icon: ReactNode; verb: string; arg: (input: Record<string, unknown>) => string | undefined }
> = {
  read_file: {
    icon: <FileTextIcon className="size-4" />,
    verb: "read",
    arg: (i) => str(i.path) ?? str(i.file_path),
  },
  list_files: {
    icon: <FolderIcon className="size-4" />,
    verb: "list",
    arg: (i) => str(i.path) ?? str(i.dir) ?? ".",
  },
  glob: {
    icon: <SearchIcon className="size-4" />,
    verb: "glob",
    arg: (i) => str(i.pattern),
  },
  grep: {
    icon: <TextSearchIcon className="size-4" />,
    verb: "grep",
    arg: (i) => str(i.pattern),
  },
  fetch: {
    icon: <GlobeIcon className="size-4" />,
    verb: "fetch",
    arg: (i) => str(i.url),
  },
};

function rowSummary(name: string, part: ToolPartLike): string {
  const state = toolState(part);
  if (state === "output-error") return "failed";
  if (state !== "output-available") return "…";
  const out = outputText(part);
  if (out === undefined || out === "") return "done";
  switch (name) {
    case "read_file":
      return `${out.split("\n").length} lines`;
    case "list_files":
      return `${lineCount(out)} entries`;
    case "glob":
    case "grep":
      return `${lineCount(out)} matches`;
    default:
      return firstLine(out, 60);
  }
}

function CompactToolRow({ part }: { part: ToolPartLike }) {
  const name = toolName(part);
  const meta = ROW_META[name];
  const state = toolState(part);
  const failed = state === "output-error";
  const running = state === "input-streaming" || state === "input-available";
  const arg = meta?.arg(asRecord(part.input));
  const out = failed ? part.errorText : outputText(part);

  return (
    <Collapsible className="group mb-2 w-full">
      <CollapsibleTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-1 text-left text-xs hover:bg-muted/50">
        <span
          className={cn(
            "shrink-0",
            failed ? "text-destructive" : "text-muted-foreground",
            running && "animate-pulse",
          )}
        >
          {meta?.icon}
        </span>
        <span className="shrink-0 text-muted-foreground">{meta?.verb}</span>
        <span className="min-w-0 truncate font-mono">{arg ?? "…"}</span>
        <span
          className={cn(
            "ml-auto shrink-0",
            failed ? "text-destructive" : "text-muted-foreground/70",
          )}
        >
          {rowSummary(name, part)}
        </span>
        <ChevronDownIcon className="size-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
      </CollapsibleTrigger>
      <CollapsibleContent>
        {out ? (
          <pre
            className={cn(
              "mt-1 max-h-64 overflow-auto rounded-md bg-muted/50 p-2 text-xs",
              failed && "bg-destructive/10 text-destructive",
            )}
          >
            {truncate(out, 8000)}
          </pre>
        ) : (
          <div className="mt-1 px-2 text-muted-foreground text-xs">
            no output yet
          </div>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}

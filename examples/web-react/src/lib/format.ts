export function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

export function str(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

export function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

export function utf8Bytes(s: string): number {
  return new TextEncoder().encode(s).length;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export function lineCount(s: string): number {
  if (s === "") return 0;
  return s.split("\n").filter((l) => l.trim() !== "").length;
}

export function firstLine(s: string, max = 80): string {
  const line = s.split("\n", 1)[0] ?? "";
  return line.length > max ? line.slice(0, max) + "…" : line;
}

export function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max) + "\n…(truncated)" : s;
}

export function timeAgo(ms: number): string {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "just now";
  const minutes = Math.floor(delta / 60_000);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(ms).toLocaleDateString();
}

/** Encode a workspace-relative path for the `/api/files/` route, keeping `/`. */
export function fileUrl(path: string): string {
  const clean = path.replace(/^\.?\//, "");
  return "/api/files/" + clean.split("/").map(encodeURIComponent).join("/");
}

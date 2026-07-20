// Ecosystem proof: drive the AC host with the AI SDK's OWN client code — the
// exact `DefaultChatTransport` + `readUIMessageStream` path `useChat` runs
// internally — and assert it reconstructs a proper UIMessage. If stock `ai`
// turns our stream into text + tool parts, the integration is real, not
// "possible". Run against a live host: node verify.mjs
import { DefaultChatTransport, readUIMessageStream } from "ai";

const API = process.env.AC_API ?? "http://127.0.0.1:8790/api/chat";

const transport = new DefaultChatTransport({
  api: API,
  prepareSendMessagesRequest({ id, messages, trigger, messageId }) {
    return { body: { id, message: messages[messages.length - 1], trigger, messageId } };
  },
});

const chatId = "verify-" + Math.random().toString(16).slice(2);
const userMessage = {
  id: "u1",
  role: "user",
  parts: [
    {
      type: "text",
      text: "Create a file pong.txt containing exactly the word PONG, then read it back.",
    },
  ],
};

const stream = await transport.sendMessages({
  trigger: "submit-message",
  chatId,
  messageId: undefined,
  messages: [userMessage],
  abortSignal: undefined,
});

let last = null;
for await (const message of readUIMessageStream({ stream })) {
  last = message; // each yield is a fuller snapshot of the assistant UIMessage
}

if (!last) {
  console.error("FAIL: the AI SDK parser produced no message");
  process.exit(1);
}

const kinds = last.parts.map((p) => p.type);
const text = last.parts
  .filter((p) => p.type === "text")
  .map((p) => p.text)
  .join("");
const tools = last.parts.filter(
  (p) => p.type === "dynamic-tool" || p.type.startsWith("tool-"),
);

console.log("chatId:", chatId);
console.log("assistant part kinds:", kinds.join(", "));
console.log("assistant text:", JSON.stringify(text));
for (const t of tools) {
  console.log(`  tool ${t.toolName ?? t.type} -> ${t.state}`);
}

const ok = last.role === "assistant" && text.length > 0 && tools.length > 0;
console.log(ok ? "PASS: stock AI SDK client parsed an AC turn" : "FAIL");
process.exit(ok ? 0 : 1);

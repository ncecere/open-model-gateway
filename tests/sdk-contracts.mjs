import assert from "node:assert/strict";
import OpenAI, { toFile } from "openai";
import Anthropic from "@anthropic-ai/sdk";

const base = process.env.SDK_TEST_BASE_URL;
const key = process.env.SDK_TEST_API_KEY;
assert.equal(new URL(base).hostname, "127.0.0.1", "SDK contracts must use loopback fixtures");
const openai = new OpenAI({ apiKey: key, baseURL: `${base}/v1`, maxRetries: 0 });
const anthropic = new Anthropic({ apiKey: key, baseURL: base, maxRetries: 0 });
const messages = [{ role: "user", content: "hello" }];
const chat = await openai.chat.completions.create({ model: "company/smart", messages });
assert.equal(chat.choices[0].message.content, "Hello from fixture");
let text = "";
for await (const event of await openai.chat.completions.create({ model: "company/smart", messages, stream: true })) {
  text += event.choices[0]?.delta?.content ?? "";
}
assert.equal(text, "Hello from fixture");
// Legacy `max_tokens` (still sent by many OpenAI-compatible clients) is an alias of max_completion_tokens.
const legacy = await openai.chat.completions.create({ model: "company/smart", messages, max_tokens: 7 });
assert.equal(legacy.choices[0].message.content, "Hello from fixture (max 7)");
const both = await openai.chat.completions.create({ model: "company/smart", messages, max_tokens: 7, max_completion_tokens: 7 });
assert.equal(both.choices[0].message.content, "Hello from fixture (max 7)");
assert.equal((await openai.chat.completions.create({ model: "company/smart", messages, max_completion_tokens: 7 })).choices[0].message.content, "Hello from fixture (max 7)");
await assert.rejects(
  openai.chat.completions.create({ model: "company/smart", messages, max_tokens: 7, max_completion_tokens: 8 }),
  (e) => e.status === 400 && e.code === "invalid_request_error",
);
let legacyText = "";
for await (const event of await openai.chat.completions.create({ model: "company/smart", messages, max_tokens: 7, stream: true })) {
  legacyText += event.choices[0]?.delta?.content ?? "";
}
assert.equal(legacyText, "Hello from fixture (max 7)");
// Responses keeps its own max_output_tokens.
const capped = await openai.responses.create({ model: "company/smart", input: "hello", store: false, max_output_tokens: 7 });
assert.equal(capped.output_text, "Hello from fixture (max 7)");
const response = await openai.responses.create({ model: "company/smart", input: "hello", store: false });
assert.equal(response.status, "completed");
assert.equal(response.output_text, "Hello from fixture");
const responseStream = openai.responses.stream({ model: "company/smart", input: "hello", store: false });
const finalResponse = await responseStream.finalResponse();
assert.equal(finalResponse.status, "completed");
assert.equal(finalResponse.output_text, "Hello from fixture");
const message = await anthropic.messages.create({ model: "company/smart", messages, max_tokens: 32 });
assert.equal(message.content[0].text, "Hello from fixture");
assert.equal(message.stop_reason, "end_turn");
const messageStream = anthropic.messages.stream({ model: "company/smart", messages, max_tokens: 32 });
let startUsage;
messageStream.on("streamEvent", (event) => {
  // The SDK later mutates this snapshot with message_delta usage; copy it now.
  if (event.type === "message_start") startUsage = { ...event.message.usage };
});
const finalMessage = await messageStream.finalMessage();
// Vendor shape: observed input at start, cumulative output replaced by message_delta.
assert.deepEqual(startUsage, { input_tokens: 3, output_tokens: 0 });
assert.equal(finalMessage.usage.input_tokens, 3);
assert.equal(finalMessage.usage.output_tokens, 4);
assert.equal(finalMessage.content[0].text, "Hello from fixture");
assert.equal(finalMessage.stop_reason, "end_turn");
console.log("OpenAI Chat (including legacy max_tokens), Responses, and Anthropic Messages SDK contracts passed (JSON + streaming helpers)");
// Files API (gateway-owned store): the official SDK's multipart order works.
const jsonl = '{"custom_id":"a","method":"POST","url":"/v1/chat/completions","body":{}}\n';
const uploaded = await openai.files.create({ file: await toFile(Buffer.from(jsonl), "input.jsonl"), purpose: "batch" });
assert.match(uploaded.id, /^file-[0-9a-f]{32}$/);
assert.equal(uploaded.purpose, "batch");
assert.equal(uploaded.bytes, Buffer.byteLength(jsonl));
assert.equal(uploaded.filename, "input.jsonl");
const userFile = await openai.files.create({
  file: await toFile(Buffer.from("notes"), "notes.txt"),
  purpose: "user_data",
  expires_after: { anchor: "created_at", seconds: 3600 },
});
assert.equal(userFile.expires_at - userFile.created_at, 3600);
assert.equal((await openai.files.retrieve(uploaded.id)).id, uploaded.id);
const listed = [];
for await (const file of openai.files.list({ purpose: "batch" })) listed.push(file.id);
assert.deepEqual(listed, [uploaded.id]);
assert.equal(await (await openai.files.content(uploaded.id)).text(), jsonl);
assert.equal((await openai.files.delete(uploaded.id)).deleted, true);
await assert.rejects(openai.files.retrieve(uploaded.id), (e) => e.status === 404);
// Batch API (gateway files, any model): the official SDK creates, retrieves, lists and cancels.
const batchInput = await openai.files.create({
  file: await toFile(Buffer.from('{"custom_id":"one","method":"POST","url":"/v1/chat/completions","body":{"model":"company/batch","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":8}}\n'), "batch.jsonl"),
  purpose: "batch",
});
const batch = await openai.batches.create({ input_file_id: batchInput.id, endpoint: "/v1/chat/completions", completion_window: "24h", metadata: { omg_mode: "gateway" } });
assert.match(batch.id, /^batch_[0-9a-f]{32}$/);
assert.equal(batch.status, "validating");
assert.equal(batch.input_file_id, batchInput.id);
assert.deepEqual(batch.request_counts, { total: 1, completed: 0, failed: 0 });
assert.equal((await openai.batches.retrieve(batch.id)).id, batch.id);
const batches = [];
for await (const b of openai.batches.list()) batches.push(b.id);
assert.deepEqual(batches, [batch.id]);
assert.equal((await openai.batches.cancel(batch.id)).status, "cancelling");
// An invalid file is refused with a line-numbered report.
const badInput = await openai.files.create({ file: await toFile(Buffer.from('{"custom_id":"x","method":"POST","url":"/v1/chat/completions","body":{"model":"company/unknown","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":8}}\n'), "bad.jsonl"), purpose: "batch" });
await assert.rejects(
  openai.batches.create({ input_file_id: badInput.id, endpoint: "/v1/chat/completions", completion_window: "24h" }),
  (e) => e.status === 400 && e.code === "invalid_batch_input",
);
console.log("Files API and Batch API SDK contracts passed");

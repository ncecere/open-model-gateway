import assert from "node:assert/strict";
import OpenAI, { toFile } from "openai";
import Anthropic from "@anthropic-ai/sdk";

const base = process.env.SDK_TEST_BASE_URL;
const key = process.env.SDK_TEST_API_KEY;
assert.equal(new URL(base).hostname, "127.0.0.1", "SDK contracts must use loopback fixtures");
const openai = new OpenAI({ apiKey: key, baseURL: `${base}/v1`, maxRetries: 0 });
// Omission-positive compatibility cases use the official client's ordinary
// retry defaults, separately from the legacy capability/refusal fixtures.
const ordinaryOpenai = new OpenAI({ apiKey: key, baseURL: `${base}/v1` });
const anthropic = new Anthropic({ apiKey: key, baseURL: base, maxRetries: 0 });
const ordinaryAnthropic = new Anthropic({ apiKey: key, baseURL: base });
const messages = [{ role: "user", content: "hello" }];
const chat = await ordinaryOpenai.chat.completions.create({ model: "company/smart", messages });
assert.equal(chat.choices[0].message.content, "Hello from fixture");
let text = "";
for await (const event of await ordinaryOpenai.chat.completions.create({ model: "company/smart", messages, stream: true })) {
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
const response = await ordinaryOpenai.responses.create({ model: "company/smart", input: "hello" });
assert.equal(response.status, "completed");
assert.equal(response.output_text, "Hello from fixture");
const responseStream = ordinaryOpenai.responses.stream({ model: "company/smart", input: "hello" });
const finalResponse = await responseStream.finalResponse();
assert.equal(finalResponse.status, "completed");
assert.equal(finalResponse.output_text, "Hello from fixture");
let responseText = "";
let terminalTextResponse;
for await (const event of await ordinaryOpenai.responses.create({ model: "company/smart", input: "hello", stream: true })) {
  if (event.type === "response.output_text.delta") responseText += event.delta;
  if (event.type === "response.completed") terminalTextResponse = event.response;
}
assert.equal(responseText, "Hello from fixture");
assert.equal(terminalTextResponse.status, "completed");
async function terminalWireResponse(args) {
  let terminal;
  for await (const event of await ordinaryOpenai.responses.create({ ...args, stream: true })) {
    if (event.type === "response.completed" || event.type === "response.incomplete") terminal = event.response;
  }
  assert.ok(terminal, "stream must provide a terminal wire response");
  return terminal;
}
// Stateless followups reuse the gateway's actual output, including read-only
// item metadata and empty annotations. No store/max/previous_response_id workaround.
// Use the terminal event, not finalResponse()'s SDK-only parsed/parsed_arguments fields.
for (const [prior, streaming] of [[response, false], [terminalTextResponse, true]]) {
  const args = {
    model: "company/smart",
    input: [{ role: "user", content: "hello" }, ...prior.output, { role: "user", content: "continue" }],
  };
  const followup = streaming
    ? await ordinaryOpenai.responses.stream(args).finalResponse()
    : await ordinaryOpenai.responses.create(args);
  assert.equal(followup.output_text, "Hello from fixture");
}
for (const streaming of [false, true]) {
  const args = { model: "company/smart", input: "call weather" };
  const prior = streaming
    ? await terminalWireResponse(args)
    : await ordinaryOpenai.responses.create(args);
  assert.equal(prior.output[0].content[0].text, "Checking weather");
  const call = prior.output[1];
  assert.equal(call.type, "function_call");
  assert.equal(call.call_id, "semantic-call");
  assert.notEqual(call.id, call.call_id);
  assert.equal(call.arguments, '{ "city": "東京" }');
  const followupArgs = {
    model: "company/smart",
    input: [
      { role: "user", content: "call weather" }, ...prior.output,
      { type: "function_call_output", call_id: call.call_id, output: "sun", id: null, status: null },
    ],
  };
  const followup = streaming
    ? await ordinaryOpenai.responses.stream(followupArgs).finalResponse()
    : await ordinaryOpenai.responses.create(followupArgs);
  assert.equal(followup.output_text, "Tool result received");
}
// Negative controls still fail before any reservation or provider dispatch.
for (const input of [
  [{ ...response.output[0], id: null }],
  [{ ...response.output[0], status: null }],
  [{ ...response.output[0], status: "failed" }],
  [{ ...response.output[0], extra: true }],
  [{ ...response.output[0], content: [{ type: "output_text", text: "hi", parsed: null }] }],
  [{ type: "function_call", call_id: "c", name: "weather", arguments: "{}", parsed_arguments: null }],
  [{ ...response.output[0], content: null }],
  [{ ...response.output[0], content: [{ type: "output_text", text: "hi", annotations: null }] }],
  [{ ...response.output[0], content: [{ type: "output_text", text: "hi", annotations: [{ type: "url_citation", url: "https://example.invalid", title: "citation", start_index: 0, end_index: 1 }] }] }],
  [{ role: "user", content: [{ type: "output_text", text: "hi", annotations: [] }] }],
  [{ type: "function_call", id: "readonly", call_id: null, name: "weather", arguments: "{}" }],
  [{ type: "function_call", id: "readonly", call_id: "c", name: "weather", arguments: {} }],
  [{ type: "function_call_output", call_id: "c", output: [] }],
  [{ type: "item_reference", id: response.output[0].id }],
  [{ type: "reasoning", id: "readonly", summary: [] }],
]) {
  await assert.rejects(ordinaryOpenai.responses.create({ model: "company/smart", input }), (e) => e.status === 400 && e.code === "invalid_request_error");
}
for (const state of [{ previous_response_id: response.id }, { store: true }]) {
  await assert.rejects(ordinaryOpenai.responses.create({ model: "company/smart", input: "continue", ...state }), (e) => e.status === 400 && e.code === "invalid_request_error");
}
// Real gateway refusal paths: no clamping and no provider dispatch.
for (const stream of [false, true]) {
  for (const maximum of [0, 101]) {
    await assert.rejects(openai.chat.completions.create({ model: "company/smart", messages, stream, max_completion_tokens: maximum }), (e) => e.status === 400 && e.code === "invalid_request_error");
    await assert.rejects(openai.responses.create({ model: "company/smart", input: "hello", store: false, stream, max_output_tokens: maximum }), (e) => e.status === 400 && e.code === "invalid_request_error");
  }
}
await assert.rejects(anthropic.messages.create({ model: "company/smart", messages }), (e) => e.status === 400);
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
// Streamed: message_start is sent before usage is observed (input placeholder 0, no output);
// message_delta carries the observed input and cumulative output, which the SDK applies.
assert.deepEqual(startUsage, { input_tokens: 0 });
assert.equal(finalMessage.usage.input_tokens, 3);
assert.equal(finalMessage.usage.output_tokens, 4);
assert.equal(finalMessage.content[0].text, "Hello from fixture");
assert.equal(finalMessage.stop_reason, "end_turn");
// OpenAI 7.28 FunctionDefinition: omitted parameters means an empty parameter
// list, not an unconstrained object. Only intentional tool/stream selectors are
// added to the API-required fields; no maximum/store workaround.
const pingMessages = [{ role: "user", content: "call ping" }];
const pingTools = [{ type: "function", function: { name: "ping" } }];
async function pingChat(args, streaming) {
  if (!streaming) return (await ordinaryOpenai.chat.completions.create(args)).choices[0].message;
  // Assemble wire deltas rather than helper-enriched parsed fields. Public
  // nullable assistant fields such as refusal are covered separately by #221.
  const message = { role: "assistant", content: "", tool_calls: [] };
  for await (const event of await ordinaryOpenai.chat.completions.create({ ...args, stream: true })) {
    const delta = event.choices[0]?.delta;
    message.content += delta?.content ?? "";
    for (const part of delta?.tool_calls ?? []) {
      const call = message.tool_calls[part.index] ??= { id: "", type: "function", function: { name: "", arguments: "" } };
      call.id += part.id ?? "";
      call.function.name += part.function?.name ?? "";
      call.function.arguments += part.function?.arguments ?? "";
    }
  }
  return message;
}
for (const streaming of [false, true]) {
  const args = { model: "company/smart", messages: pingMessages, tools: pingTools };
  const prior = await pingChat(args, streaming);
  const call = prior.tool_calls[0];
  assert.equal(call.id, "ping-call");
  assert.equal(call.function.name, "ping");
  assert.deepEqual(JSON.parse(call.function.arguments), {});
  const followup = {
    ...args,
    messages: [...pingMessages, prior, { role: "tool", tool_call_id: call.id, content: "" }],
  };
  const next = await pingChat(followup, streaming);
  assert.equal(next.content, "Tool result received");
}
const noArguments = { type: "object", properties: {}, additionalProperties: false };
const nativePing = { model: "company/smart", messages: pingMessages, max_tokens: 32, tools: [{ name: "ping", input_schema: noArguments }] };
let nativePrior;
for (const streaming of [false, true]) {
  const prior = streaming
    ? await ordinaryAnthropic.messages.stream(nativePing).finalMessage()
    : await ordinaryAnthropic.messages.create(nativePing);
  nativePrior = prior;
  const call = prior.content.find((block) => block.type === "tool_use");
  assert.equal(call.id, "ping-call");
  assert.equal(call.name, "ping");
  assert.deepEqual(call.input, {});
  for (const defaults of [{}, { is_error: false }]) {
    const args = { ...nativePing, messages: [...pingMessages, { role: "assistant", content: prior.content }, { role: "user", content: [{ type: "tool_result", tool_use_id: call.id, ...defaults }] }] };
    const next = streaming
      ? await ordinaryAnthropic.messages.stream(args).finalMessage()
      : await ordinaryAnthropic.messages.create(args);
    assert.equal(next.content[0].text, "Tool result received");
  }
}
const explicitSchema = { type: "object", properties: { city: { type: "string" } }, required: ["city"], additionalProperties: false };
for (const strict of [false, true]) {
  await ordinaryOpenai.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: { name: "weather", parameters: explicitSchema, strict } }] });
}
const nativeFollowup = (block) => ({ ...nativePing, messages: [...pingMessages, { role: "assistant", content: nativePrior.content }, { role: "user", content: [block] }] });
await ordinaryAnthropic.messages.create(nativeFollowup({ type: "tool_result", tool_use_id: "ping-call", content: [{ type: "text", text: "ok" }], is_error: false }));
for (const parameters of [null, false, 0, "object", []]) {
  await assert.rejects(ordinaryOpenai.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: { name: "ping", parameters } }] }), (e) => e.status === 400);
}
for (const fn of [{ name: "ping", extra: true }, { name: "ping", strict: "false" }]) {
  await assert.rejects(ordinaryOpenai.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: fn }] }), (e) => e.status === 400);
}
await assert.rejects(ordinaryOpenai.chat.completions.create({ model: "company/smart", messages: [...pingMessages, { role: "tool", content: "" }] }), (e) => e.status === 400);
for (const block of [
  { type: "tool_result", tool_use_id: "ping-call", content: null },
  { type: "tool_result", tool_use_id: "ping-call", extra: true },
  { type: "tool_result" }, { type: "tool_result", tool_use_id: null },
  { type: "tool_result", tool_use_id: "" },
  ...[null, 0, "false", [], {}].map((is_error) => ({ type: "tool_result", tool_use_id: "ping-call", is_error })),
]) {
  await assert.rejects(ordinaryAnthropic.messages.create(nativeFollowup(block)), (e) => e.status === 400);
}
await assert.rejects(ordinaryAnthropic.messages.create(nativeFollowup({ type: "tool_result", tool_use_id: "ping-call", is_error: true })), (e) => e.status === 501);
console.log("OpenAI Chat (including omitted function parameters), Responses, and Anthropic Messages (including empty tool success) SDK contracts passed (JSON + streaming helpers)");
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
  file: await toFile(Buffer.from('{"custom_id":"one","method":"POST","url":"/v1/chat/completions","body":{"model":"company/batch","messages":[{"role":"user","content":"hi"}]}}\n'), "batch.jsonl"),
  purpose: "batch",
});
const batch = await ordinaryOpenai.batches.create({ input_file_id: batchInput.id, endpoint: "/v1/chat/completions", completion_window: "24h", metadata: { omg_mode: "gateway" } });
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

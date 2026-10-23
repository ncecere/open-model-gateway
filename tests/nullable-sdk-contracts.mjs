import assert from "node:assert/strict";
import OpenAI from "openai";

// Validate both supplied fixture inputs before an SDK constructor can use any
// ambient credential or default provider origin. Never run against a provider.
const base = process.env.SDK_TEST_BASE_URL;
const key = process.env.SDK_TEST_API_KEY;
assert.ok(base && key, "explicit fixture URL and key required");
const url = new URL(base);
assert.equal(url.protocol, "http:");
assert.equal(url.hostname, "127.0.0.1");
assert.ok(url.port);
const protocol = process.env.SDK_TEST_PROTOCOL;
assert.ok(["chat", "responses"].includes(protocol));
const client = new OpenAI({ apiKey: key, baseURL: `${base}/v1` });
const required = protocol === "chat"
  ? { model: "company/smart", messages: [{ role: "user", content: "hello" }] }
  : { model: "company/smart", input: "hello" };
const path = protocol === "chat" ? "/v1/chat/completions" : "/v1/responses";
let positives = 0;
let refusals = 0;
function checkJson(value) {
  if (protocol === "chat") {
    assert.equal(value.object, "chat.completion");
    assert.equal(value.choices[0].message.content, "世界");
    assert.equal(value.choices[0].finish_reason, "stop");
    assert.equal(value.usage.prompt_tokens, 3);
    assert.equal(value.usage.completion_tokens, 2);
  } else {
    assert.equal(value.object, "response");
    assert.equal(value.status, "completed");
    assert.equal(value.output[0].content[0].text, "世界");
    assert.equal(value.usage.input_tokens, 3);
    assert.equal(value.usage.output_tokens, 2);
  }
}
async function sdk(extra) {
  const args = { ...required, ...extra };
  const value = protocol === "chat"
    ? await client.chat.completions.create(args)
    : await client.responses.create(args);
  if (args.stream === true) {
    let text = "";
    let terminal = 0;
    for await (const event of value) {
      if (protocol === "chat") {
        text += event.choices[0]?.delta?.content ?? "";
        if (event.choices[0]?.finish_reason === "stop") terminal++;
      } else {
        if (event.type === "response.output_text.delta") text += event.delta;
        if (event.type === "response.completed") {
          terminal++;
          checkJson(event.response);
        }
        assert.notEqual(event.type, "error");
        assert.notEqual(event.type, "response.failed");
      }
    }
    assert.equal(text, "世界");
    assert.equal(terminal, 1);
  } else {
    checkJson(value);
    if (protocol === "responses") assert.equal(value.output_text, "世界");
  }
  positives++;
}
// API-required args only, plus the field under test; ordinary SDK defaults.
for (const extra of [{}, { stream: null }, { stream: false }, { stream: true }]) await sdk(extra);
if (protocol === "responses") {
  for (const store of [null, false]) await sdk({ store });
} else {
  // Public wire metadata in 7.28, not an SDK-only parsed extension.
  await sdk({ messages: [{ role: "assistant", content: "hello", refusal: null }] });
}

async function raw(extra, expected = 200, streaming = extra.stream === true) {
  const result = await fetch(`${base}${path}`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ ...required, ...extra }),
  });
  assert.equal(result.status, expected, JSON.stringify(extra));
  if (expected === 200) {
    if (streaming) {
      assert.match(result.headers.get("content-type"), /text\/event-stream/);
      const body = await result.text();
      assert.ok(body.includes("世界"));
      assert.ok(!body.includes('"type":"error"'));
      if (protocol === "chat") {
        assert.match(body, /data: \[DONE\]/);
        assert.match(body, /"finish_reason":"stop"/);
      } else {
        assert.equal((body.match(/event: response.completed/g) ?? []).length, 1);
        assert.ok(!body.includes("response.failed"));
      }
    } else {
      assert.match(result.headers.get("content-type"), /application\/json/);
      checkJson(await result.json());
    }
    positives++;
  } else {
    assert.equal((await result.json()).error.code, "invalid_request_error");
    refusals++;
  }
}
for (const extra of [{}, { stream: null }, { stream: false }, { stream: true }]) await raw(extra);
if (protocol === "responses") {
  for (const store of [null, false]) {
    await raw({ store });
    await raw({ store, stream: true });
  }
  for (const stream of [null, false, true]) await raw({ store: true, stream }, 400);
  // Existing nullable supported options keep omission-equivalence.
  for (const field of ["instructions", "max_output_tokens", "temperature", "metadata"]) await raw({ [field]: null });
} else {
  for (const field of ["max_tokens", "max_completion_tokens", "n", "temperature", "metadata", "stream_options"]) await raw({ [field]: null });
  await raw({ stream: true, stream_options: { include_usage: true } });
  await raw({ stream: true, stream_options: { include_usage: false } });
  await raw({ stream: true, stream_options: { include_usage: null } }, 400);
  await raw({ stream_options: {} }, 400);
  await raw({ messages: [{ role: "assistant", content: "hello", tool_calls: null }] }, 400);
  await raw({ n: 2 }, 400);
  await raw({ max_tokens: 0 }, 400);
  await raw({ max_tokens: 7, max_completion_tokens: 8 }, 400);
  // Chat store and legacy extensions are not part of the supported subset.
  await raw({ store: null }, 400);
  await raw({ messages: [{ role: "assistant", content: "hello", refusal: null }] });
  for (const refusal of ["no", "", false, 0, [], {}]) {
    await raw({ messages: [{ role: "assistant", content: "hello", refusal }] }, 400);
  }
  for (const message of [
    { role: "user", content: "hello", refusal: null },
    { role: "assistant", content: null, refusal: null },
    { role: "assistant", content: "hello", parsed: null },
    { role: "assistant", content: "hello", unknown: null },
  ]) await raw({ messages: [message] }, 400);
}
for (const field of protocol === "responses" ? ["stream", "store"] : ["stream"]) {
  for (const value of [0, 1, "false", "true", [], {}]) await raw({ [field]: value }, 400);
}
// SDK 7.28 collections are optional but NOT nullable. Do not widen them.
for (const value of [null, false, {}, "", [null]]) await raw({ tools: value }, 400);
await raw({ tools: [] });
await raw({ tools: Array.from({ length: 129 }, (_, i) => protocol === "chat"
  ? { type: "function", function: { name: `f${i}`, parameters: {} } }
  : { type: "function", name: `f${i}`, parameters: {} }) }, 400);
for (const field of ["model", protocol === "chat" ? "messages" : "input"]) await raw({ [field]: null }, 400);
for (const field of ["unknown", "response_format", "reasoning", "functions", "previous_response_id", "refusal"]) await raw({ [field]: null }, 400);
for (const metadata of [{ session_id: null }, { session_id: [] }, Array(2).fill("x"), Object.fromEntries(Array.from({ length: 17 }, (_, i) => [i, "x"]))]) await raw({ metadata }, 400);
await raw({ temperature: 3 }, 400);
await raw({ [protocol === "chat" ? "max_completion_tokens" : "max_output_tokens"]: 101 }, 400);
await raw({ [protocol === "chat" ? "max_completion_tokens" : "max_output_tokens"]: 0 }, 400);
if (protocol === "responses") {
  for (const input of [
    [{ role: "user", content: null }],
    [{ role: "assistant", content: "hello", id: null }],
    [{ role: "assistant", content: [{ type: "output_text", text: "hi", annotations: null }] }],
  ]) await raw({ input }, 400);
}
// Native Anthropic 0.131 stream/tools are not nullable. max_tokens stays required.
for (const extra of [{ stream: null }, { tools: null }, { max_tokens: null }, { model: null }, { unknown: null }]) {
  const result = await fetch(`${base}/v1/messages`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json", "anthropic-version": "2023-06-01" },
    body: JSON.stringify({ model: "company/smart", messages: [{ role: "user", content: "hello" }], max_tokens: 32, ...extra }),
  });
  assert.equal(result.status, 400);
  assert.equal((await result.json()).error.type, "invalid_request_error");
  refusals++;
}
console.log(JSON.stringify({ positives, refusals }));

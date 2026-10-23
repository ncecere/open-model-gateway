import assert from "node:assert/strict";
import OpenAI from "openai";

const base = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(base).hostname, "127.0.0.1");
// Ordinary SDK defaults, including retries. Invalid requests must be refused
// before admission; the gateway itself never retries upstream.
const client = new OpenAI({ apiKey: process.env.SDK_TEST_API_KEY, baseURL: `${base}/v1` });
const messages = [{ role: "user", content: "ping" }];
const tools = [{ type: "function", function: { name: "ping" } }];
for (const stream of [false, true]) {
  let call;
  if (stream) {
    call = { id: "", type: "function", function: { name: "", arguments: "" } };
    for await (const event of await client.chat.completions.create({ model: "company/smart", messages, tools, stream: true })) {
      for (const delta of event.choices[0]?.delta?.tool_calls ?? []) {
        assert.equal(delta.index, 0);
        call.id += delta.id ?? "";
        call.function.name += delta.function?.name ?? "";
        call.function.arguments += delta.function?.arguments ?? "";
      }
    }
  } else {
    call = (await client.chat.completions.create({ model: "company/smart", messages, tools })).choices[0].message.tool_calls[0];
  }
  assert.deepEqual(call, { id: "ping-call", type: "function", function: { name: "ping", arguments: "{}" } });
  const followup = [...messages, { role: "assistant", tool_calls: [call] }, { role: "tool", tool_call_id: call.id, content: "" }];
  let text = "";
  if (stream) {
    for await (const event of await client.chat.completions.create({ model: "company/smart", messages: followup, tools, stream: true })) {
      text += event.choices[0]?.delta?.content ?? "";
    }
  } else {
    text = (await client.chat.completions.create({ model: "company/smart", messages: followup, tools })).choices[0].message.content;
  }
  assert.equal(text, "done");
}
const explicit = { type: "object", properties: { city: { type: "string" } }, required: ["city"] };
await client.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: { name: "ping", parameters: explicit, strict: false } }] });
await assert.rejects(client.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: { name: "ping", strict: true } }] }), (e) => e.status === 501 && e.code === "unsupported_capability");
for (const fn of [{ name: "ping", parameters: null }, { name: "ping", parameters: [] }, { name: "ping", extra: true }]) {
  await assert.rejects(client.chat.completions.create({ model: "company/smart", messages, tools: [{ type: "function", function: fn }] }), (e) => e.status === 400);
}
console.log("Priced SDK -> HTTP gateway -> approved local HTTP adapter tool roundtrip passed");

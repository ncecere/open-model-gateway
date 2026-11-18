import assert from "node:assert/strict";
import Anthropic from "@anthropic-ai/sdk";

const baseURL = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(baseURL).hostname, "127.0.0.1");
const client = new Anthropic({ baseURL, apiKey: process.env.SDK_TEST_API_KEY });
const followup = (block) => ({ model: "company/smart", max_tokens: 32, messages: [
  { role: "user", content: "ping" },
  { role: "assistant", content: [
    { type: "tool_use", id: "first-call", name: "ping", input: {} },
    { type: "tool_use", id: "second-call", name: "ping", input: {} },
  ] },
  { role: "user", content: [block, { type: "tool_result", tool_use_id: "first-call", content: "unchanged" }] },
], tools: [{ name: "ping", input_schema: { type: "object" } }] });
const [expectedStatus, expectedMessage] = process.argv.slice(2);
if (expectedStatus) {
  const request = followup({ type: "tool_result", tool_use_id: "second-call", content: "failed", is_error: true });
  for (const stream of [false, true]) {
    await assert.rejects(client.messages.create({ ...request, stream }, { maxRetries: 0 }), (e) =>
      e.status === Number(expectedStatus) && e.error?.error?.type === "invalid_request_error" &&
      e.error.error.message === expectedMessage);
  }
  process.exit(0);
}
for (const flag of [undefined, false, true]) {
  for (const content of ["", "failed\n東京"]) {
    const block = { type: "tool_result", tool_use_id: "second-call", content };
    if (flag !== undefined) block.is_error = flag;
    const response = await client.messages.create(followup(block));
    assert.equal(response.type, "message");
    assert.equal(response.stop_reason, "end_turn");
  }
}
for (const flag of [null, 0, "true", [], {}]) {
  await assert.rejects(client.messages.create(followup({ type: "tool_result", tool_use_id: "second-call", is_error: flag })), (e) => e.status === 400);
}
await assert.rejects(client.messages.create(followup({ type: "tool_result", tool_use_id: "second-call", content: "failed", is_error: true, unexpected: true })), (e) => e.status === 400);

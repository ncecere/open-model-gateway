import assert from "node:assert/strict";
import OpenAI from "openai";
import Anthropic from "@anthropic-ai/sdk";

const base = process.env.SDK_TEST_BASE_URL;
const key = process.env.SDK_TEST_API_KEY;
assert.equal(new URL(base).hostname, "127.0.0.1", "retry contracts must use loopback fixtures");
const observations = [];
// Record real HTTP responses without changing SDK retry decisions or defaults.
const recordingFetch = async (...args) => {
  const response = await fetch(...args);
  observations.push({ status: response.status, retry: response.headers.get("x-should-retry") });
  return response;
};
const openai = new OpenAI({ apiKey: key, baseURL: `${base}/v1`, fetch: recordingFetch });
const anthropic = new Anthropic({ apiKey: key, baseURL: base, fetch: recordingFetch });
const messages = [{ role: "user", content: "synthetic retry test" }];
const permanent = process.argv[2] === "permanent";
const stream = permanent;
const calls = [
  () => openai.chat.completions.create({ model: "company/smart", messages, stream }),
  () => openai.responses.create({ model: "company/smart", input: "synthetic retry test", stream }),
  // Native Messages requires max_tokens, even for a capability refusal.
  () => anthropic.messages.create({ model: "company/smart", messages, max_tokens: 7, stream }),
];
const results = [];
for (const call of calls) {
  const start = observations.length;
  if (permanent) {
    await assert.rejects(call(), (error) => {
      assert.equal(error.status, 501);
      if (error instanceof OpenAI.APIError) {
        assert.equal(error.code, "unsupported_capability");
        assert.equal(error.type, "unsupported_capability");
      } else {
        assert.equal(error.error.type, "error");
        assert.equal(error.error.error.type, "invalid_request_error");
      }
      assert.match(error.message, /No registered deployment supports the requested capabilities/);
      return true;
    });
  } else {
    const response = await call();
    assert.ok(response.id);
    const text = response.choices?.[0].message.content ?? response.output_text ?? response.content?.[0].text;
    assert.equal(text, "recovered");
  }
  results.push(observations.slice(start));
}
assert.deepEqual(results, calls.map(() => permanent
  ? [{ status: 501, retry: "false" }]
  : [{ status: 502, retry: null }, { status: 200, retry: null }]));
console.log(`${permanent ? "Permanent capability refusals: one request" : "Transient upstream failures: SDK retries recover"} for Chat, Responses and native Messages`);

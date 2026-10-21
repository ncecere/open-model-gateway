import assert from "node:assert/strict";
import OpenAI from "openai";

const base = process.env.SDK_TEST_BASE_URL;
assert.equal(new URL(base).hostname, "127.0.0.1");
const key = process.env.SDK_TEST_API_KEY;
const wires = [];
const client = new OpenAI({
  apiKey: key, baseURL: `${base}/v1`,
  fetch: async (url, init) => {
    const response = await fetch(url, init);
    wires.push({ request: JSON.parse(init.body), response: await response.clone().json() });
    return response;
  },
});
const expected = [[1, -2, 0.5], [0, 4, -0.25]];
const input = ["synthetic one", "synthetic two"];
// Ordinary official SDK call: no encoding workaround. SDK requests base64 on wire
// and decodes it to numeric vectors. Check both, not just its permissive fallback.
const result = await client.embeddings.create({ model: "company/smart", input });
assert.equal(wires[0].request.encoding_format, "base64");
assert.equal(typeof wires[0].response.data[0].embedding, "string");
assert.deepEqual(result.data.map(d => d.embedding), expected);
assert.equal(result.model, "company/smart");
assert.deepEqual(result.usage, { prompt_tokens: 7, total_tokens: 7 });
const float = await client.embeddings.create({ model: "company/smart", input, encoding_format: "float" });
assert.deepEqual(float.data.map(d => d.embedding), expected);
const encoded = await client.embeddings.create({ model: "company/smart", input, encoding_format: "base64" });
for (const [i, item] of encoded.data.entries()) {
  assert.equal(typeof item.embedding, "string");
  const bytes = Buffer.alloc(12);
  expected[i].forEach((n, j) => bytes.writeFloatLE(n, j * 4));
  assert.equal(item.embedding, bytes.toString("base64"));
}
for (const encoding of [undefined, null]) {
  const response = await fetch(`${base}/v1/embeddings`, {
    method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "company/smart", input, encoding_format: encoding }),
  });
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.deepEqual(body.data.map(d => d.embedding), expected);
  assert.deepEqual(body.usage, { prompt_tokens: 7, total_tokens: 7 });
}
for (const body of [
  { input: [1, 2] }, { input: [[1, 2]] }, { input: [] },
  { input, encoding_format: "unknown" }, { input, encoding_format: "BASE64" },
  { input, encoding_format: false }, { input, dimensions: 0 },
  { input, dimensions: 16385 }, { input, stream: true },
  { input: Array(129).fill("synthetic") }, { input: "a".repeat(1024 * 1024 + 1) },
]) {
  const response = await fetch(`${base}/v1/embeddings`, {
    method: "POST", headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "company/smart", ...body }),
  });
  assert.equal(response.status, 400);
  assert.equal((await response.json()).error.code, "invalid_request_error");
}
console.log("Embedding SDK default, wire base64, explicit float, HTTP omission/null and refusals passed");

import assert from "node:assert/strict";
import OpenAI from "openai";
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
const finalMessage = await messageStream.finalMessage();
assert.equal(finalMessage.content[0].text, "Hello from fixture");
assert.equal(finalMessage.stop_reason, "end_turn");
console.log("OpenAI Chat, Responses, and Anthropic Messages SDK contracts passed (JSON + streaming helpers)");

// Official SDKs, mocked transport only: never contacts an upstream provider.
// Run with node --experimental-strip-types --test tests/inference-sdk-bases.test.mjs.
import assert from "node:assert/strict";
import { test } from "node:test";
import OpenAI from "openai";
import Anthropic from "@anthropic-ai/sdk";
import { anthropicBaseUrl, inferenceEndpoint } from "../apps/web/src/lib/inference-url.ts";

for (const base of ["https://api.bitop.dev:8443/prefix@blue/v1/inner/v1", "http://127.0.0.1:18188/v1"]) {
  test(`displayed SDK bases produce correct request URLs: ${base}`, async () => {
    const urls = [];
    const fetch = async input => {
      urls.push(input instanceof Request ? input.url : String(input));
      return Response.json({});
    };
    const openai = new OpenAI({ apiKey: "synthetic-test-key", baseURL: base, maxRetries: 0, fetch });
    const anthropic = new Anthropic({ apiKey: "synthetic-test-key", baseURL: anthropicBaseUrl(base), maxRetries: 0, fetch });
    await openai.chat.completions.create({ model: "fixture", messages: [{ role: "user", content: "hello" }] });
    await anthropic.messages.create({ model: "fixture", messages: [{ role: "user", content: "hello" }], max_tokens: 8 });
    assert.deepEqual(urls, [`${base}/chat/completions`, `${base}/messages`]);
    assert.equal(inferenceEndpoint("/v1/realtime?model=company%2Fsmart&mode=audio", base, "realtime"), `${base.replace(/^https:/, "wss:").replace(/^http:/, "ws:")}/realtime?model=company%2Fsmart&mode=audio`);
    assert.equal(inferenceEndpoint("/v1/messages", base, "messages"), `${base}/messages`);
  });
}

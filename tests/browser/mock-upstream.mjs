// TEST ONLY: deterministic loopback OpenAI-compatible upstream for the browser suite.
// It never forwards anything, ignores prompt content and records only request counts.
import http from "node:http";

export const MOCK_REPLY = "Mock upstream reply.";

export function createMockUpstream() {
  const stats = { chat: 0 };
  const server = http.createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) { body += chunk; if (body.length > 1_000_000) break; }
    const json = (value, status = 200) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(value)); };
    if (req.method !== "POST" || req.url !== "/v1/chat/completions") return json({ error: { message: "not found" } }, 404);
    let request;
    try { request = JSON.parse(body); } catch { return json({ error: { message: "invalid json" } }, 400); }
    stats.chat += 1;
    // Complete usage (including the zero cache split) so the attempt settles at a known cost.
    const usage = { prompt_tokens: 12, completion_tokens: 4, total_tokens: 16, prompt_tokens_details: { cached_tokens: 0, cache_write_tokens: 0 } };
    if (request.stream) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write(`data: ${JSON.stringify({ id: "mock", object: "chat.completion.chunk", model: request.model, choices: [{ index: 0, delta: { role: "assistant", content: MOCK_REPLY }, finish_reason: null }] })}\n\n`);
      res.write(`data: ${JSON.stringify({ id: "mock", object: "chat.completion.chunk", model: request.model, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] })}\n\n`);
      res.write(`data: ${JSON.stringify({ id: "mock", object: "chat.completion.chunk", model: request.model, choices: [], usage })}\n\n`);
      return res.end("data: [DONE]\n\n");
    }
    json({ id: "mock", object: "chat.completion", created: Math.floor(Date.now() / 1000), model: request.model, choices: [{ index: 0, message: { role: "assistant", content: MOCK_REPLY }, finish_reason: "stop" }], usage });
  });
  return { server, stats };
}

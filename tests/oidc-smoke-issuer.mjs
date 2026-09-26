// TEST ONLY: deliberately authenticates one fixture identity without a password.
// Loopback-bound, opt-in, fixed callback/client. Never register with a real deployment.
import http from "node:http";
import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";
if (process.env.OMG_ENABLE_TEST_ISSUER !== "1") throw new Error("Test issuer requires explicit opt-in");
const issuer = "http://127.0.0.1:18082";
const callback = "http://127.0.0.1:18081/api/v1/auth/callback";
const client = "gateway-browser-smoke";
const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: "jwk" }), kid: "smoke", use: "sig", alg: "RS256" };
const codes = new Map();
const encode = (v) => Buffer.from(JSON.stringify(v)).toString("base64url");
const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, issuer);
  const json = (body, status = 200) => { res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" }); res.end(JSON.stringify(body)); };
  if (url.pathname === "/.well-known/openid-configuration") return json({ issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`, jwks_uri: `${issuer}/jwks`, response_types_supported: ["code"], subject_types_supported: ["public"], id_token_signing_alg_values_supported: ["RS256"], token_endpoint_auth_methods_supported: ["none"], code_challenge_methods_supported: ["S256"] });
  if (url.pathname === "/jwks") return json({ keys: [jwk] });
  if (url.pathname === "/authorize") {
    const p = url.searchParams;
    if (p.get("client_id") !== client || p.get("redirect_uri") !== callback || p.get("response_type") !== "code" || p.get("code_challenge_method") !== "S256" || !p.get("nonce") || !p.get("state")) return json({ error: "invalid_request" }, 400);
    const code = randomBytes(32).toString("hex");
    codes.set(code, { nonce: p.get("nonce"), challenge: p.get("code_challenge"), expires: Date.now() + 60_000 });
    const redirect = new URL(callback); redirect.searchParams.set("code", code); redirect.searchParams.set("state", p.get("state"));
    res.writeHead(302, { location: redirect.href, "cache-control": "no-store" }); return res.end();
  }
  if (url.pathname === "/token" && req.method === "POST") {
    let body = ""; for await (const chunk of req) { body += chunk; if (body.length > 8192) return json({ error: "invalid_request" }, 400); }
    const p = new URLSearchParams(body); const attempt = codes.get(p.get("code")); codes.delete(p.get("code"));
    if (!attempt || attempt.expires < Date.now() || p.get("client_id") !== client || p.get("redirect_uri") !== callback || createHash("sha256").update(p.get("code_verifier") ?? "").digest("base64url") !== attempt.challenge) return json({ error: "invalid_grant" }, 400);
    const now = Math.floor(Date.now() / 1000);
    const payload = `${encode({ alg: "RS256", kid: "smoke" })}.${encode({ iss: issuer, sub: "browser-smoke", aud: client, iat: now, exp: now + 300, nonce: attempt.nonce, email: "browser-smoke@example.invalid", email_verified: true })}`;
    return json({ access_token: "test-only-unused-access-token", token_type: "Bearer", expires_in: 300, id_token: `${payload}.${sign("RSA-SHA256", Buffer.from(payload), privateKey).toString("base64url")}` });
  }
  json({ error: "not_found" }, 404);
});
server.listen(18082, "127.0.0.1", () => console.log("TEST ONLY OIDC issuer ready on loopback:18082"));
process.on("SIGTERM", () => server.close());

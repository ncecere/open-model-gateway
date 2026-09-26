import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { createHash, createPublicKey, verify } from "node:crypto";
import { createDemoIssuer, ISSUER, CALLBACK, CLIENT } from "../scripts/demo-oidc.mjs";
const server = createDemoIssuer();
let port;
before(async () => { await new Promise(resolve => server.listen(0, "127.0.0.1", resolve)); port = server.address().port; });
after(async () => { await new Promise(resolve => server.close(resolve)); });
function request(path, { method = "GET", body, headers = {} } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ hostname: "127.0.0.1", port, path, method, headers: { host: "127.0.0.1:18084", ...(body ? { "content-type": "application/x-www-form-urlencoded" } : {}), ...headers } }, res => {
      let text = ""; res.on("data", chunk => text += chunk); res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, text }));
    }); req.on("error", reject); req.end(body);
  });
}
const verifier = "v".repeat(43);
const parameters = () => new URLSearchParams({ client_id: CLIENT, redirect_uri: CALLBACK, response_type: "code", code_challenge_method: "S256", code_challenge: createHash("sha256").update(verifier).digest("base64url"), state: "browser-state", nonce: "browser-nonce" });
async function choose(account = "operator") {
  const p = parameters(); p.set("account", account);
  const response = await request("/choose", { method: "POST", body: p.toString(), headers: { origin: ISSUER } });
  assert.equal(response.status, 302);
  const location = new URL(response.headers.location);
  assert.equal(location.origin + location.pathname, CALLBACK);
  assert.equal(location.searchParams.get("state"), "browser-state");
  return location.searchParams.get("code");
}
const tokenBody = code => new URLSearchParams({ code, grant_type: "authorization_code", client_id: CLIENT, redirect_uri: CALLBACK, code_verifier: verifier });
test("picker advertises four scoped demo roles without passwords or external resources", async () => {
  const res = await request(`/authorize?${parameters()}`);
  assert.equal(res.status, 200);
  for (const [account, title] of [["operator", "Platform Admin"], ["orgadmin", "Organization Admin"], ["alex", "Team Admin · Alex"], ["blair", "Member · Blair"]]) {
    assert.ok(res.text.includes(`<button name="account" value="${account}"><strong>${title}</strong>`));
  }
  assert.equal((res.text.match(/<button /g) ?? []).length, 4);
  assert.match(res.text, /no platform-wide access/); assert.match(res.text, /no organization administration/);
  assert.match(res.text, /No password/); assert.doesNotMatch(res.text, /type="password"|<script|<link/);
  assert.equal(res.headers["cache-control"], "no-store"); assert.match(res.headers["content-security-policy"], /default-src 'none'/);
  assert.equal(res.headers["referrer-policy"], "origin");
  assert.match(res.headers["content-security-policy"], /form-action 'self' http:\/\/127\.0\.0\.1:3000;/);
});
test("rejects rebinding hosts, arbitrary redirects, and cross-origin account selection", async () => {
  assert.equal((await request("/jwks", { headers: { host: "attacker.invalid" } })).status, 400);
  const p = parameters(); p.set("redirect_uri", "https://attacker.invalid/callback");
  assert.equal((await request(`/authorize?${p}`)).status, 400);
  p.set("redirect_uri", CALLBACK); p.set("account", "operator");
  assert.equal((await request("/choose", { method: "POST", body: p.toString(), headers: { origin: "https://attacker.invalid" } })).status, 403);
  p.set("account", "arbitrary-user");
  assert.equal((await request("/choose", { method: "POST", body: p.toString(), headers: { origin: ISSUER } })).status, 400);
});
test("signed persona claims preserve nonce and authorization codes are single use", async () => {
  const jwks = JSON.parse((await request("/jwks")).text);
  for (const account of ["operator", "orgadmin", "alex", "blair"]) {
    const code = await choose(account); const body = tokenBody(code).toString();
    const res = await request("/token", { method: "POST", body }); assert.equal(res.status, 200);
    const [header, payload, signature] = JSON.parse(res.text).id_token.split(".");
    assert.ok(verify("RSA-SHA256", Buffer.from(`${header}.${payload}`), createPublicKey({ key: jwks.keys[0], format: "jwk" }), Buffer.from(signature, "base64url")));
    const claims = JSON.parse(Buffer.from(payload, "base64url"));
    assert.equal(claims.sub, account); assert.equal(claims.email_verified, true);
    assert.equal(claims.email, `${account}@demo.invalid`); assert.equal(claims.nonce, "browser-nonce"); assert.equal(claims.aud, CLIENT); assert.equal(claims.iss, ISSUER);
    assert.equal(claims.exp - claims.iat, 300);
    for (const field of ["platform_admin", "role", "roles", "memberships"]) assert.equal(claims[field], undefined);
    assert.equal((await request("/token", { method: "POST", body })).status, 400);
  }
});
test("wrong PKCE verifier cannot redeem a code", async () => {
  for (const account of ["operator", "orgadmin", "alex", "blair"]) {
    const body = tokenBody(await choose(account)); body.set("code_verifier", "x".repeat(43));
    assert.equal((await request("/token", { method: "POST", body: body.toString() })).status, 400);
  }
});

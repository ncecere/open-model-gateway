// Local exploration only. Never register this passwordless issuer with a real deployment.
import http from "node:http";
import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { pathToFileURL } from "node:url";
export const ISSUER = "http://127.0.0.1:18084";
export const CALLBACK = "http://127.0.0.1:3000/api/v1/auth/callback";
export const CLIENT = "gateway-local-demo";
const accounts = {
  operator: { email: "operator@demo.invalid", title: "Platform Admin", description: "Platform-wide user and organization administration; Gateway demo organization owner" },
  orgadmin: { email: "orgadmin@demo.invalid", title: "Organization Admin", description: "Gateway demo organization administration, teams, catalog, and policies; no platform-wide access" },
  alex: { email: "alex@demo.invalid", title: "Team Admin · Alex", description: "Manage the Product team, its keys, service accounts, and members; no organization administration" },
  blair: { email: "blair@demo.invalid", title: "Member · Blair", description: "Private personal workspace and your own Product team keys and activity; no administration" },
};
const encode = value => Buffer.from(JSON.stringify(value)).toString("base64url");
const escapeHtml = value => value.replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const validAuthorization = p => p.get("client_id") === CLIENT && p.get("redirect_uri") === CALLBACK && p.get("response_type") === "code" && p.get("code_challenge_method") === "S256" && /^[A-Za-z0-9_-]{43}$/.test(p.get("code_challenge") ?? "") && ["state", "nonce"].every(k => /^[A-Za-z0-9_-]{1,200}$/.test(p.get(k) ?? ""));
export function createDemoIssuer() {
  const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const jwk = { ...publicKey.export({ format: "jwk" }), kid: "local-demo", use: "sig", alg: "RS256" };
  const codes = new Map();
  return http.createServer(async (req, res) => {
    res.setHeader("cache-control", "no-store");
    res.setHeader("x-content-type-options", "nosniff");
    // Retain the form Origin header without leaking authorization query parameters.
    res.setHeader("referrer-policy", "origin");
    const json = (value, status = 200) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(value)); };
    if (req.headers.host !== "127.0.0.1:18084") return json({ error: "invalid_host" }, 400);
    for (const [code, item] of codes) if (item.expires < Date.now()) codes.delete(code);
    const url = new URL(req.url, ISSUER);
    if (req.method === "GET" && url.pathname === "/.well-known/openid-configuration") return json({ issuer: ISSUER, authorization_endpoint: `${ISSUER}/authorize`, token_endpoint: `${ISSUER}/token`, jwks_uri: `${ISSUER}/jwks`, response_types_supported: ["code"], subject_types_supported: ["public"], id_token_signing_alg_values_supported: ["RS256"], token_endpoint_auth_methods_supported: ["none"], code_challenge_methods_supported: ["S256"] });
    if (req.method === "GET" && url.pathname === "/jwks") return json({ keys: [jwk] });
    if (req.method === "GET" && url.pathname === "/authorize") {
      const p = url.searchParams;
      if (!validAuthorization(p)) return json({ error: "invalid_request" }, 400);
      const hidden = ["client_id", "redirect_uri", "response_type", "code_challenge_method", "code_challenge", "state", "nonce"].map(k => `<input type="hidden" name="${k}" value="${escapeHtml(p.get(k))}">`).join("");
      res.writeHead(200, { "content-type": "text/html; charset=utf-8", "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' http://127.0.0.1:3000; base-uri 'none'; frame-ancestors 'none'" });
      return res.end(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Explore Open Model Gateway</title><style>body{margin:0;background:#f8f9fb;color:#16191f;font:16px/1.5 system-ui,sans-serif}main{max-width:560px;margin:8vh auto;padding:28px}h1{font-size:32px;line-height:1.2}small{color:#515b69}.label{text-transform:uppercase;letter-spacing:.12em;font-size:12px;font-weight:700}button{display:block;width:100%;text-align:left;margin:12px 0;padding:18px 22px;background:white;border:1px solid #c6cbd3;border-radius:12px;color:inherit;cursor:pointer;font:inherit}button:hover{border-color:#5963db;background:#f0f2ff}button:focus-visible{outline:3px solid #5963db;outline-offset:3px}strong{display:block;font-size:18px}.notice{border-left:3px solid #a56900;padding:10px 16px;background:#fff5d9;font-size:14px}</style><main><div class="label">Open Model Gateway · local demo</div><h1>Choose an account to explore.</h1><p>No password or external identity provider required. Each account has its own private workspace.</p><form method="post" action="/choose">${hidden}${Object.entries(accounts).map(([key, a]) => `<button name="account" value="${key}"><strong>${a.title}</strong><small>${a.description}</small></button>`).join("")}</form><p class="notice">Local exploration only—not production authentication. Sample provider connections are disabled. Usage and costs remain empty until actual executions occur.</p><p><small>To switch accounts, sign out from the dashboard account menu and sign in again.</small></p></main></html>`);
    }
    if (req.method !== "POST" || !["/choose", "/token"].includes(url.pathname)) return json({ error: "not_found" }, 404);
    if (!req.headers["content-type"]?.startsWith("application/x-www-form-urlencoded")) return json({ error: "invalid_request" }, 400);
    if (url.pathname === "/choose" && req.headers.origin !== ISSUER) return json({ error: "invalid_origin" }, 403);
    let body = "";
    try {
      for await (const chunk of req) { body += chunk; if (Buffer.byteLength(body) > 8192) return json({ error: "invalid_request" }, 413); }
    } catch { return json({ error: "invalid_request" }, 400); }
    const p = new URLSearchParams(body);
    if (url.pathname === "/choose") {
      const account = p.get("account");
      if (!validAuthorization(p) || !Object.hasOwn(accounts, account)) return json({ error: "invalid_request" }, 400);
      if (codes.size >= 1000) return json({ error: "temporarily_unavailable" }, 503);
      const code = randomBytes(32).toString("hex");
      codes.set(code, { account, nonce: p.get("nonce"), challenge: p.get("code_challenge"), expires: Date.now() + 60_000 });
      const redirect = new URL(CALLBACK); redirect.searchParams.set("code", code); redirect.searchParams.set("state", p.get("state"));
      res.writeHead(302, { location: redirect.href }); return res.end();
    }
    const attempt = codes.get(p.get("code")); codes.delete(p.get("code"));
    if (!attempt || attempt.expires < Date.now() || p.get("grant_type") !== "authorization_code" || p.get("client_id") !== CLIENT || p.get("redirect_uri") !== CALLBACK || !/^[A-Za-z0-9._~-]{43,128}$/.test(p.get("code_verifier") ?? "") || createHash("sha256").update(p.get("code_verifier")).digest("base64url") !== attempt.challenge) return json({ error: "invalid_grant" }, 400);
    const now = Math.floor(Date.now() / 1000);
    const payload = `${encode({ alg: "RS256", kid: jwk.kid })}.${encode({ iss: ISSUER, sub: attempt.account, aud: CLIENT, iat: now, exp: now + 300, nonce: attempt.nonce, email: accounts[attempt.account].email, email_verified: true })}`;
    json({ access_token: "local-demo-unused-token", token_type: "Bearer", expires_in: 300, id_token: `${payload}.${sign("RSA-SHA256", Buffer.from(payload), privateKey).toString("base64url")}` });
  });
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.env.OMG_ENABLE_LOCAL_DEMO !== "1") throw new Error("Requires OMG_ENABLE_LOCAL_DEMO=1. This issuer authenticates fixed demo accounts without passwords.");
  const server = createDemoIssuer();
  server.listen(18084, "127.0.0.1", () => console.log("LOCAL DEMO issuer ready on 127.0.0.1:18084; never use with real data"));
  for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => server.close());
}

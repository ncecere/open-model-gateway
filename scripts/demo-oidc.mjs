// Local exploration only. Never register this passwordless issuer with a real deployment.
import http from "node:http";
import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { pathToFileURL } from "node:url";
export const ISSUER = "http://127.0.0.1:18084";
export const CALLBACK = "http://127.0.0.1:3000/api/v1/auth/callback";
export const CLIENT = "gateway-local-demo";
export const DEMO_ACCOUNTS = {
  operator: { email: "operator@demo.invalid", name: "Morgan Lee", title: "Platform Admin", description: "Enterprise administration, catalogs, users, policies and financial totals; private keys remain private", groups: ["omg/platform-admin", "omg/product-member"] },
  auditor: { email: "auditor@demo.invalid", name: "Avery Chen", title: "Platform Auditor", description: "Read-only enterprise configuration, usage/cost totals and audit; no platform mutations", groups: ["omg/platform-auditor"] },
  alex: { email: "alex@demo.invalid", name: "Alex Rivera", title: "Team Admin · Alex", description: "Your personal workspace plus administration of Product and Research; no platform Admin access", groups: ["omg/platform-user", "omg/product-admin", "omg/research-member"] },
  blair: { email: "blair@demo.invalid", name: "Blair Kim", title: "Platform User · Blair", description: "Your personal workspace and your own activity in Product and Research; no administration", groups: ["omg/platform-user", "omg/product-member", "omg/research-member"] },
  unentitled: { email: "unentitled@demo.invalid", name: "Casey Brooks", title: "Unentitled SSO user", description: "SSO authentication succeeds, but platform access must be denied; no personal workspace is created", groups: [] },
};
const accounts = DEMO_ACCOUNTS;
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
      // Look follows Grounded's development-accounts sign-in (dark card, initials, role badge, arrow).
      const initials = (email) => email.slice(0, 2).toUpperCase();
      const roles = { operator: ["Platform admin", "info"], auditor: ["Platform auditor", "neutral"], alex: ["Team admin", "neutral"], blair: ["Platform user", "neutral"], unentitled: ["No entitlement", "warn"] };
      const css = `:root{color-scheme:dark}*{box-sizing:border-box}body{margin:0;min-height:100dvh;display:grid;place-items:center;padding:48px 16px;background:radial-gradient(60rem 30rem at 50% -10%,#1d1f4a,transparent 70%),#0d0f14;color:#e7e9ee;font:14px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,sans-serif}main{width:100%;max-width:26rem;display:flex;flex-direction:column;gap:24px}.brand{display:flex;flex-direction:column;align-items:center;gap:8px;text-align:center}.mark{position:relative;width:40px;height:40px;margin-bottom:8px;border-radius:10px;background:#5b5bd6;box-shadow:inset 0 1px 0 rgba(255,255,255,.2)}.mark i{position:absolute;right:6px;bottom:6px;width:12px;height:12px;border-radius:3px;background:#3dd68c}h1{margin:0;font-size:24px;letter-spacing:-.01em}.sub{margin:0;color:#9ba1ad;font-size:15px}.card{background:#14161c;border:1px solid #262a33;border-radius:12px;padding:24px;box-shadow:0 12px 32px rgba(0,0,0,.35)}h2{margin:0;font-size:17px}.lead{margin:4px 0 20px;color:#9ba1ad;font-size:13px}h3{margin:0;font-size:15px}.hint{margin:0 0 12px;color:#9ba1ad;font-size:13px}form{display:flex;flex-direction:column;gap:8px;margin:0}button{display:flex;align-items:center;gap:12px;width:100%;padding:8px 12px;border:1px solid #2a2e38;border-radius:8px;background:#1a1d24;color:inherit;font:inherit;text-align:left;cursor:pointer}button:hover{background:#21252e;border-color:#3a3f4b}button:focus-visible{outline:2px solid #7c7cf0;outline-offset:2px}.av{flex:none;display:grid;place-items:center;width:24px;height:24px;border-radius:50%;background:#262a52;color:#b9bdf6;font-size:10px;font-weight:600}.txt{flex:1;min-width:0;display:flex;flex-direction:column;line-height:1.3}strong{font-weight:500;font-size:14px}.meta{color:#9ba1ad;font-size:12px;overflow:hidden;white-space:nowrap;text-overflow:ellipsis}.desc{color:#7d8390;font-size:11.5px;white-space:normal}.badge{flex:none;padding:1px 7px;border-radius:999px;font-size:11px;border:1px solid #3a3f4b;color:#c3c7d0}.badge.info{border-color:#3e4fb0;background:#1c2350;color:#b9c3ff}.badge.warn{border-color:#6b4a12;background:#2a1f0c;color:#f0c27a}.arrow{flex:none;color:#6e7480}.notice{margin:0;color:#7d8390;font-size:12px;text-align:center}`;
      return res.end(`<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Sign in · Open Model Gateway (local demo)</title><style>${css}</style><main><div class="brand"><span class="mark" aria-hidden="true"><i></i></span><h1>Open Model Gateway</h1><p class="sub">Local demo sign-in</p></div><section class="card"><h2>Choose an account</h2><p class="lead">No password or external identity provider required. Entitled accounts receive their own private workspace. The unentitled account must be denied.</p><h3>Development accounts</h3><p class="hint">Local exploration only. These accounts have no password.</p><form method="post" action="/choose">${hidden}${Object.entries(accounts).map(([key, a]) => `<button name="account" value="${key}" title="${a.description}"><span class="av" aria-hidden="true">${initials(a.email)}</span><span class="txt"><strong>${a.title}</strong><span class="meta">${a.email}</span></span><span class="badge ${roles[key][1]}">${roles[key][0]}</span><span class="arrow" aria-hidden="true">→</span></button>`).join("")}</form></section><p class="notice">Local exploration only, not production authentication. To switch accounts, sign out from the dashboard account menu and sign in again.</p></main></html>`);
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
    const payload = `${encode({ alg: "RS256", kid: jwk.kid })}.${encode({ iss: ISSUER, sub: attempt.account, aud: CLIENT, iat: now, exp: now + 300, nonce: attempt.nonce, email: accounts[attempt.account].email, email_verified: true, name: accounts[attempt.account].name, groups: accounts[attempt.account].groups })}`;
    json({ access_token: "local-demo-unused-token", token_type: "Bearer", expires_in: 300, id_token: `${payload}.${sign("RSA-SHA256", Buffer.from(payload), privateKey).toString("base64url")}` });
  });
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.env.OMG_ENABLE_LOCAL_DEMO !== "1") throw new Error("Requires OMG_ENABLE_LOCAL_DEMO=1. This issuer authenticates fixed demo accounts without passwords.");
  const server = createDemoIssuer();
  server.listen(18084, "127.0.0.1", () => console.log("LOCAL DEMO issuer ready on 127.0.0.1:18084; never use with real data"));
  for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => server.close());
}

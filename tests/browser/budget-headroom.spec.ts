import { expect, test } from "@playwright/test";
import { BASE_URL } from "./stack.mjs";
import { api, chat, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, readFixture, signIn } from "./helpers";

test.afterEach(expectNoCspViolations);
test.use({ trace: "off", screenshot: "off", video: "off" });
const policy = (amount: string) => ({ requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [{ period: "lifetime", amount_microusd: amount }] });

test("headroom uses real accounting and preserves member/personal refusals", async ({ browser }) => {
  const operator = await signIn(browser, "operator"), member = await signIn(browser, "blair"), admin = await signIn(browser, "alex"), auditor = await signIn(browser, "auditor");
  const ws = readFixture().product!;
  const platform = `/platform/workspaces/${ws}/policy`;
  try {
    expect((await api(operator.page, "PUT", platform, policy("9007199254740993"))).status()).toBe(200);
    const created = await api(member.page, "POST", `/workspaces/${ws}/keys`, { name: "headroom-acceptance", expires_in_days: 1, model_ids: null, budgets: policy("1000000").budgets });
    expect(created.status()).toBe(200);
    const key = await created.json();
    expect((await chat(key.token)).status).toBe(200);
    const path = `/workspaces/${ws}/keys/${key.id}`;
    const known = await (await api(member.page, "GET", `${path}/policy`)).json();
    expect(known.budgets[0]).toMatchObject({ headroom_status: "restricted", remaining_microusd: null, settled_microusd: null, unknown_cost_requests: null });
    expect(known.budgets.at(-1)).toMatchObject({ headroom_status: "known", settled_microusd: "70", pending_held_microusd: "0", remaining_microusd: "999930" });
    await member.page.goto(`${path}?tab=limits`);
    await expect(member.page.getByText("Remaining: $0.99993", { exact: true })).toBeVisible();
    await expect(member.page.getByText("Lifetime · Remaining: Restricted", { exact: true })).toBeVisible();
    expect((await api(member.page, "PUT", `/workspaces/${ws}/policy`, policy("1"))).status()).toBe(403);
    await admin.page.goto(`${path}?tab=limits`);
    await expect(admin.page.getByText("Lifetime · Remaining: $0.99993", { exact: true })).toBeVisible();
    await admin.page.goto(`/workspaces/${ws}/settings?tab=limits`);
    await expect(admin.page.getByText("Effective headroom", { exact: true })).toBeVisible();
    const before = await (await api(operator.page, "GET", platform)).json();
    expect((await api(operator.page, "PUT", platform, policy("9007199254740995"))).status()).toBe(200);
    const after = await (await api(operator.page, "GET", platform)).json();
    expect(after.budgets[0].settled_microusd).toBe(before.budgets[0].settled_microusd);
    expect(BigInt(after.budgets[0].remaining_microusd) - BigInt(before.budgets[0].remaining_microusd)).toBe(2n);
    // Test-only upstream omits usage; real settlement must keep the floor and
    // mark headroom unknown, despite legacy unresolved_usage remaining false.
    expect((await fetch(`${BASE_URL}/v1/chat/completions`, { method: "POST", headers: { authorization: `Bearer ${key.token}`, "content-type": "application/json" }, body: JSON.stringify({ model: "mock/chat", messages: [{ role: "user", content: "budget-headroom-unknown" }], max_completion_tokens: 16 }) })).status).toBe(200);
    const unknown = await (await api(member.page, "GET", `${path}/policy`)).json();
    expect(unknown.budgets.at(-1)).toMatchObject({ headroom_status: "unknown", remaining_microusd: null, unknown_cost_requests: "1", unresolved_usage: false });
    expect(BigInt(unknown.budgets.at(-1).unknown_held_microusd)).toBeGreaterThan(0n);
    const rotated = await api(member.page, "POST", `${path}/rotate`, { expires_in_days: 1 });
    expect(rotated.status()).toBe(200);
    const replacement = await rotated.json();
    const stats = await (await api(member.page, "GET", `/workspaces/${ws}/keys/${replacement.id}/stats`)).json();
    expect(stats.budgets).toEqual(unknown.budgets);
    await member.page.goto(`/workspaces/${ws}/keys/${replacement.id}?tab=limits`);
    await expect(member.page.getByText("Remaining: Unknown", { exact: true })).toBeVisible();
    await expect(member.page.getByText(/Retained unknown-cost floors:/)).toBeVisible();
    await expectAccessible(member.page, "key budget headroom");
    await member.page.setViewportSize({ width: 390, height: 844 });
    await expectNoHorizontalScroll(member.page);
    // An auditor can read personal aggregates, not the owner's key policy.
    const me = await (await api(admin.page, "GET", "/me")).json();
    const personal = me.workspaces.find((w: { kind: string }) => w.kind === "personal").id;
    expect((await api(operator.page, "PUT", `/platform/workspaces/${personal}/policy`, policy("1000000"))).status()).toBe(200);
    const personalKey = await (await api(admin.page, "POST", `/workspaces/${personal}/keys`, { name: "private-headroom", expires_in_days: 1, model_ids: null })).json();
    expect((await api(auditor.page, "GET", `/platform/workspaces/${personal}/policy`)).status()).toBe(200);
    expect((await api(auditor.page, "GET", `/workspaces/${personal}/policy`)).status()).toBe(403);
    for (const suffix of [`keys/${personalKey.id}`, `keys/${personalKey.id}/policy`, `keys/${personalKey.id}/stats`, "requests"]) {
      expect((await api(auditor.page, "GET", `/workspaces/${personal}/${suffix}`)).status()).toBe(403);
      expect((await api(operator.page, "GET", `/workspaces/${personal}/${suffix}`)).status()).toBe(403);
    }
  } finally {
    expect((await api(operator.page, "DELETE", platform)).status()).toBe(200);
    for (const session of [operator, member, admin, auditor]) await session.context.close();
  }
});

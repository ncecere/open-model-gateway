// Example monitoring files (deploy/monitoring): the Prometheus rules and the
// Grafana dashboard parse, name only metric families and labels the gateway
// exports (apps/gateway/src/metrics_export.rs), and cover the accounting
// failure counter (#528). promtool is not run here or in CI (#584); check the
// rules with `promtool check rules` before deploying them.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const root = fileURLToPath(new URL("..", import.meta.url));
const read = path => readFileSync(join(root, path), "utf8");
const rules = parse(read("deploy/monitoring/prometheus-rules.yml"));
const dashboard = JSON.parse(read("deploy/monitoring/grafana-dashboard.json"));
const squash = text => text.replace(/\s+/g, " ").trim();

// The exported families: name => { kind, labels }.
function families() {
  const source = read("apps/gateway/src/metrics_export.rs");
  const block = source.slice(source.indexOf("classes! {"));
  const entry = /"(gateway_[a-z0-9_]+)" => \(\w+, (Counter|Gauge|Histogram), "[^"]*", \[([^\]]*)\]\)/g;
  const found = new Map();
  for (const [, name, kind, labels] of block.slice(0, block.indexOf("\n}")).matchAll(entry)) {
    found.set(name, { kind, labels: [...labels.matchAll(/"([a-z_]+)"/g)].map(m => m[1]) });
  }
  assert.ok(found.size > 40, "metrics_export.rs classes! table was not found");
  return found;
}

// OpenMetrics sample name => family.
function family(sample, known) {
  const candidates = [
    [sample, "Gauge"],
    [sample.replace(/_total$/, ""), "Counter"],
    [sample.replace(/_(bucket|sum|count)$/, ""), "Histogram"],
  ];
  for (const [name, kind] of candidates) {
    const f = known.get(name);
    if (f && f.kind === kind) return f;
  }
  return undefined;
}

const allRules = rules.groups.flatMap(group => group.rules.map(rule => ({ ...rule, group: group.name })));
const panels = dashboard.panels;
const dashboardExprs = panels.flatMap(panel => (panel.targets ?? []).map(target => target.expr));

function stages() {
  const source = read("apps/gateway/src/db/accounting_failure.rs");
  const body = source.slice(source.indexOf("impl Stage"), source.indexOf("enum Cause"));
  return [...body.matchAll(/=> "([a-z_]+)"/g)].map(m => m[1]);
}

test("every gateway metric and label in the rules and dashboard is exported", () => {
  const known = families();
  const exprs = [...allRules.map(rule => rule.expr), ...dashboardExprs];
  const generic = new Set(["instance", "job", "le"]);
  for (const expr of exprs) {
    for (const [, sample, matchers] of expr.matchAll(/\b(gateway_[a-z0-9_]+)(\{[^}]*\})?/g)) {
      const f = family(sample, known);
      assert.ok(f, `${sample} is not an exported family (in: ${squash(expr)})`);
      for (const [, label] of (matchers ?? "").matchAll(/([a-z_]+)\s*(?:=~|!~|!=|=)/g)) {
        assert.ok(f.labels.includes(label) || generic.has(label), `${sample} has no label ${label}`);
      }
    }
  }
});

test("rule names are unique and every rule has a severity and a summary", () => {
  const names = allRules.map(rule => rule.alert);
  assert.equal(new Set(names).size, names.length);
  for (const rule of allRules) {
    assert.ok(["info", "warning", "critical"].includes(rule.labels?.severity), `${rule.alert} severity`);
    assert.ok(rule.annotations?.summary, `${rule.alert} summary`);
  }
});

test("accounting failures alert, split by settlement stage (#528)", () => {
  const all = stages();
  // A new stage must be placed in one rule on purpose (#497): the critical one if
  // a failure there leaves money held until reconciliation.
  assert.deepEqual(all.toSorted(), [
    "cancellation_settlement", "explicit_settlement", "governance_storage", "job_settlement", "job_storage",
    "key_authentication", "maintenance", "pool_acquire_and_begin", "realtime_admission", "realtime_settlement",
    "route_lookup",
  ]);
  const settlement = "explicit_settlement|cancellation_settlement|realtime_settlement|job_settlement";
  // increase() plus the series that did not exist 10 minutes ago: a series
  // exists only after a failure, so a new one is the first failure, which
  // rate() and increase() cannot see.
  const failures = matcher =>
    `sum by (stage, cause) ( increase(gateway_accounting_failures_total{${matcher}}[10m]) or ` +
    `(gateway_accounting_failures_total{${matcher}} unless gateway_accounting_failures_total{${matcher}} offset 10m) ) > 0`;
  const expected = {
    GatewayAccountingFailures: { expr: failures(`stage!~"${settlement}"`), for: "5m", severity: "warning" },
    GatewaySettlementAccountingFailures: { expr: failures(`stage=~"${settlement}"`), for: undefined, severity: "critical" },
  };
  for (const [name, { expr, for: pending, severity }] of Object.entries(expected)) {
    const rule = allRules.find(r => r.alert === name);
    assert.ok(rule, `${name} is missing`);
    assert.equal(rule.group, "open-model-gateway.accounting");
    // No `absent` or `or vector(0)`: missing data yields nothing.
    assert.equal(squash(rule.expr), expr);
    assert.equal(rule.for, pending, `${name} for`);
    assert.equal(rule.labels.severity, severity);
    assert.equal(rule.annotations.runbook_url, "docs/operations.md#accounting-failures");
    for (const label of ["{{ $labels.stage }}", "{{ $labels.cause }}"]) {
      assert.ok(rule.annotations.summary.includes(label), `${name} summary names ${label}`);
    }
  }
  // The two rules split the stages: every stage is covered exactly once.
  const re = new RegExp(`^(?:${settlement})$`);
  assert.deepEqual(all.filter(stage => re.test(stage)).toSorted(), ["cancellation_settlement", "explicit_settlement", "job_settlement", "realtime_settlement"]);
  // Runbook links resolve to a heading in docs/operations.md.
  assert.match(read("docs/operations.md"), /^### Accounting failures$/m);
});

test("the docs do not claim CI validates the rules with promtool (#528)", () => {
  const docs = read("docs/operations.md");
  assert.doesNotMatch(docs, /validated with `promtool/);
  assert.match(docs, /`promtool check rules` before deploying them; it is not run in CI \(#584\)/);
});

test("dashboard panel ids are unique and panels do not overlap", () => {
  const ids = panels.map(panel => panel.id);
  assert.equal(new Set(ids).size, ids.length, `duplicate panel id in ${ids}`);
  const cells = new Map();
  for (const { id, gridPos: { x, y, w, h } } of panels) {
    assert.ok(x >= 0 && x + w <= 24, `panel ${id} width`);
    for (let row = y; row < y + h; row++) {
      for (let col = x; col < x + w; col++) {
        const key = `${row},${col}`;
        assert.ok(!cells.has(key), `panels ${cells.get(key)} and ${id} overlap at ${key}`);
        cells.set(key, id);
      }
    }
  }
});

test("dashboard shows accounting failures by stage and cause in the accounting row (#528)", () => {
  const expr = 'sum by (stage, cause) (rate(gateway_accounting_failures_total{instance=~"$instance"}[$__rate_interval]))';
  const panel = panels.find(p => (p.targets ?? []).some(t => t.expr === expr));
  assert.ok(panel, "no panel plots gateway_accounting_failures_total by stage and cause");
  assert.equal(panel.type, "timeseries");
  assert.deepEqual(panel.datasource, { type: "prometheus", uid: "${datasource}" });
  assert.equal(panel.targets.length, 1);
  assert.equal(panel.targets[0].legendFormat, "{{stage}} / {{cause}}");
  assert.deepEqual(panel.targets[0].datasource, { type: "prometheus", uid: "${datasource}" });
  const rows = panels.filter(p => p.type === "row").toSorted((a, b) => a.gridPos.y - b.gridPos.y);
  const row = rows.findLast(r => r.gridPos.y < panel.gridPos.y);
  assert.equal(row.title, "Accounting and admission");
});

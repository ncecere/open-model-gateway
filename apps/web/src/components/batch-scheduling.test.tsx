// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { batchSchedulingPath, daysText, pauseLabel, queueText, readingText, schedulingBody, schedulingFields, settingsRows, windowText, type BatchSchedulingDocument } from "../lib/batch-scheduling";
import { batchStatus, batchStatusLabel } from "../lib/batches";
import { validateFields, type Values } from "../lib/forms";
import { markup } from "../lib/test-fixtures";
import { RouteBatchScheduling, useBatchScheduling } from "./batch-scheduling";

const doc: BatchSchedulingDocument = {
  settings: {
    max_concurrency: 2, yield_live_threshold: 1, priority: 10,
    metrics: { url: "http://gpu.example:8000/metrics", max_waiting: 0, max_running: null, max_kv_cache_percent: 90 },
    window: { timezone: "America/New_York", days: ["mon", "tue", "wed", "thu", "fri"], start: "19:00", end: "07:00" },
  },
  defaults: { max_concurrency: 2, yield_live_threshold: null, metrics: null, priority: null, window: null },
  priority_supported: true,
  status: {
    queued_lines: 1200, waiting_batches: 3, running_lines: 0, paused_reason: "outside_window", checked_at: "2026-10-09T12:00:00Z", live_in_flight: 0,
    metrics: { checked_at: "2026-10-09T12:00:00Z", ok: true, waiting: 3, running: 1, kv_cache_permille: 913, error: null },
  },
};
const values = (fields: ReturnType<typeof schedulingFields>, patch: Values = {}): Values => ({ ...Object.fromEntries(fields.map(f => [f.name, f.value ?? ""])), ...patch });

describe("batch scheduling (route)", () => {
  it("summarizes settings in plain words", () => {
    expect(daysText(["mon", "tue", "wed", "thu", "fri"])).toBe("Mon–Fri");
    expect(daysText(["mon", "wed", "sat", "sun"])).toBe("Mon, Wed, Sat, Sun");
    expect(daysText(["mon", "tue", "wed", "thu", "fri", "sat", "sun"])).toBe("Every day");
    expect(windowText(doc.settings.window!)).toBe("Mon–Fri 19:00–07:00 (America/New_York)");
    const rows = Object.fromEntries(settingsRows(doc.settings, true).map(r => [r.label, r.value]));
    expect(rows["Yield to live traffic"]).toBe("Pause when 1+ live requests run");
    expect(rows["Server load signal"]).toBe("Pause if waiting > 0 or KV cache > 90%");
    expect(rows["Priority hint"]).toBe("priority 10 on batch lines");
    // Cloud routes don't offer a priority hint.
    expect(settingsRows(doc.defaults, false).map(r => r.label)).not.toContain("Priority hint");
    expect(Object.fromEntries(settingsRows(doc.defaults, false).map(r => [r.label, r.value]))["Time window"]).toBe("Any time");
  });

  it("explains pauses and the last server reading; unreadable is busy", () => {
    expect(pauseLabel("live_traffic")).toBe("Yielding to live traffic");
    expect(pauseLabel(null)).toBeNull();
    expect(readingText(doc.status.metrics)).toBe("3 waiting · 1 running · KV cache 91.3%");
    expect(readingText({ ...doc.status.metrics!, ok: false, error: "http_status" })).toBe("Unavailable (http status) · treated as busy");
    expect(queueText(doc.status)).toBe("0 lines running · 1,200 lines queued (3 batches) · Paused: outside its time window");
    expect(queueText({ ...doc.status, queued_lines: 0 })).toBe("No batch lines waiting");
    expect(batchSchedulingPath("d 1")).toBe("/api/v1/platform/deployments/d%201/batch-scheduling");
  });

  it("round-trips the edit dialog into the API body", () => {
    const fields = schedulingFields(doc.settings, true);
    expect(fields.map(f => f.name)).toContain("priority");
    expect(schedulingFields(doc.defaults, false).map(f => f.name)).not.toContain("priority");
    const v = values(fields);
    expect(validateFields(fields, v)).toEqual({});
    expect(schedulingBody(v)).toEqual(doc.settings);
    // Defaults: everything optional is off.
    const plain = schedulingFields(doc.defaults, false);
    expect(schedulingBody(values(plain))).toEqual({ max_concurrency: 2, yield_live_threshold: null, metrics: null, priority: null, window: null });
  });

  it("validates where mistakes are likely", () => {
    const fields = schedulingFields(doc.settings, true);
    const errors = (patch: Values) => validateFields(fields, values(fields, patch));
    expect(errors({ metrics_max_waiting: "", metrics_max_kv_cache_percent: "" }).metrics_url).toBe("Set at least one limit below.");
    expect(errors({ metrics_url: "http://gpu.example:8000/stats" }).metrics_url).toBeDefined();
    expect(errors({ window_start: "7pm" }).window_start).toBe("Use HH:MM (24-hour).");
    expect(errors({ priority: "-1" }).priority).toBeDefined();
    expect(errors({ max_concurrency: "0" }).max_concurrency).toBeDefined();
    // No days: no window, and its times are not required.
    expect(errors({ window_days: "[]", window_start: "" })).toEqual({});
    expect(schedulingBody(values(fields, { window_days: "[]" })).window).toBeNull();
  });

  it("renders the route's queue, pause reason, reading and settings", () => {
    function Section() { return <RouteBatchScheduling query={useBatchScheduling("route-1")} />; }
    const html = markup(<Section />, [[batchSchedulingPath("route-1"), doc]]);
    for (const text of ["Queued lines", "1,200", "0 / 2", "Outside its time window", "3 waiting · 1 running · KV cache 91.3%", "Mon–Fri 19:00–07:00 (America/New_York)", "priority 10 on batch lines"]) expect(html).toContain(text);
  });
});

describe("batch status while waiting for capacity", () => {
  const base = { state: "in_progress" as const, upstream_status: "in_progress", cancel_requested_at: null };
  it("says Queued — waiting for capacity only when nothing runs", () => {
    expect(batchStatus({ ...base, waiting_reason: "outside_window", running_lines: 0 })).toBe("waiting_capacity");
    expect(batchStatusLabel("waiting_capacity")).toBe("Queued — waiting for capacity");
    expect(batchStatus({ ...base, waiting_reason: "fair_share", running_lines: 2 })).toBe("in_progress");
    expect(batchStatus({ ...base, waiting_reason: null, running_lines: 0 })).toBe("in_progress");
    expect(batchStatus({ ...base, waiting_reason: "live_traffic", cancel_requested_at: "2026-10-09T00:00:00Z" })).toBe("cancelling");
  });
});

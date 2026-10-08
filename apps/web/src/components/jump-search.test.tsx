import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClientProvider } from "@tanstack/react-query";
import { admin as session, session as alex, team, testClient } from "../lib/test-fixtures";
import type { CommandPaletteProps } from "./ui/command-palette/command-palette";
import { JumpSearch, requestIdQuery, useEntityResults } from "./jump-search";

const captured = vi.hoisted(() => ({ palette: undefined as CommandPaletteProps | undefined }));
vi.mock("./ui/command-palette/command-palette", () => ({
  CommandPalette: (props: CommandPaletteProps) => { captured.palette = props; return null; },
  CommandPaletteTrigger: () => <button>Search</button>,
  useCommandPaletteShortcut: () => {},
}));
afterEach(() => { captured.palette = undefined; vi.unstubAllGlobals(); });
function setup() {
  const navigate = vi.fn(), refresh = vi.fn();
  renderToStaticMarkup(<QueryClientProvider client={testClient()}><JumpSearch session={session} navigate={navigate} refresh={refresh} /></QueryClientProvider>);
  const palette = captured.palette!;
  const finalFocus = palette.finalFocus as (interaction: "keyboard") => unknown;
  return { palette, finalFocus, navigate, refresh };
}
describe("jump search focus after command selection", () => {
  it("uses default focus restoration on dismissal and non-navigation actions", () => {
    const { palette, finalFocus, refresh, navigate } = setup();
    expect(finalFocus("keyboard")).toBe(true);
    palette.groups.flatMap(group => group.items).find(item => item.id === "refresh")!.onSelect();
    expect(refresh).toHaveBeenCalledOnce(); expect(navigate).not.toHaveBeenCalled();
    expect(finalFocus("keyboard")).toBe(true);
  });
  it("uses the destination heading after navigation without adding a sequential tab stop", () => {
    const heading = { tabIndex: 0 };
    const querySelector = vi.fn().mockReturnValue(heading);
    vi.stubGlobal("document", { querySelector });
    const { palette, finalFocus, navigate } = setup();
    palette.groups.flatMap(group => group.items).find(item => item.id === "page:models")!.onSelect();
    expect(navigate).toHaveBeenCalledWith({ page: "models", ws: undefined });
    expect(finalFocus("keyboard")).toBe(heading);
    expect(querySelector).toHaveBeenCalledWith("#main h1"); expect(heading.tabIndex).toBe(-1);
  });
  it("falls back safely if navigation has not produced a heading", () => {
    vi.stubGlobal("document", { querySelector: () => null });
    const { palette, finalFocus } = setup();
    palette.groups.flatMap(group => group.items).find(item => item.id === "page:models")!.onSelect();
    expect(finalFocus("keyboard")).toBe(true);
  });
});

describe("jump search entities (finding #4)", () => {
  function results(who: typeof session, query: string, seed: [string, unknown][]) {
    const client = testClient(); for (const [path, data] of seed) client.setQueryData(["api", undefined, path, "jump", path.includes("/catalog") ? "models" : path.includes("/me/keys") ? "keys" : path.includes("/requests") ? "requests" : path.includes("/platform/models") ? "admin-models" : path.includes("/platform/users") ? "users" : path.includes("/platform/workspaces") ? "workspaces" : "connections"], data);
    let out: ReturnType<typeof useEntityResults> | undefined;
    function Probe() { out = useEntityResults(who, team, query, true); return null; }
    renderToStaticMarkup(<QueryClientProvider client={client}><Probe /></QueryClientProvider>);
    return out!;
  }
  it("finds models, your own keys and request ids in the selected workspace, opening their pages", () => {
    const day = (n: number) => new Date(Date.now() + n * 86_400_000).toISOString().slice(0, 10);
    const r = results(alex, "gpt", [
      ["/api/v1/workspaces/team/catalog?q=gpt&limit=8", { data: [{ model_id: "m1", public_name: "openai/gpt-5", display_name: "GPT-5" }] }],
      ["/api/v1/me/keys?status=all&limit=200", { data: [{ id: "k1", name: "gpt notebook", status: "active", workspace: { id: "team", name: "Product", kind: "team" } }, { id: "k2", name: "Other", status: "active", workspace: { id: "team", name: "Product", kind: "team" } }] }],
    ]);
    expect(r.groups.map(g => g.label)).toEqual(["Models in Product", "Your API keys"]);
    expect(r.groups[0]!.hits[0]).toMatchObject({ label: "GPT-5", search: { page: "workspace-model", ws: "team", record: "m1" } });
    expect(r.groups[1]!.hits.map(h => h.label)).toEqual(["gpt notebook"]);
    const ids = results(alex, "3cbfb500", [[`/api/v1/workspaces/team/requests?q=3cbfb500&limit=5&start_date=${day(-92)}&end_date=${day(1)}`, { data: [{ root_request_id: "3cbfb500-0000-0000-0000-000000000000", model: "gpt-5", key: { id: "k1", name: "CI" } }], next_cursor: null }]]);
    expect(ids.groups[0]).toMatchObject({ label: "Requests in Product", hits: [{ label: "Request 3cbfb500", search: { page: "request-detail", record: "3cbfb500-0000-0000-0000-000000000000" } }] });
    expect(requestIdQuery("3cbfb500")).toBe(true); expect(requestIdQuery("gpt")).toBe(false);
  });
  it("adds users, teams/projects and connections for platform readers only", () => {
    const seed: [string, unknown][] = [["/api/v1/platform/users?q=al&limit=5", { data: [{ id: "u1", email: "alex@demo.invalid", disabled_at: null }] }], ["/api/v1/platform/workspaces?q=al&limit=5", { data: [{ id: "w1", name: "Analytics", kind: "project" }] }], ["/api/v1/platform/providers?limit=200", { data: [{ id: "c1", name: "Alibaba", provider: "openai_compatible", enabled: true }] }]];
    expect(results(alex, "al", seed).groups.map(g => g.label)).toEqual([]);
    const admin = results({ ...session, workspaces: [team] }, "al", seed);
    expect(admin.groups.map(g => g.label)).toEqual(["Users", "Teams and projects", "Connections"]);
    expect(admin.groups[1]!.hits[0]!.search).toEqual({ page: "workspace-detail", record: "w1", kind: "project" });
  });
});

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { ApiError } from "../lib/api";
import { ErrorNotice } from "./ui";
import { Overview, Profile } from "../pages/overview";
import { Grants } from "../pages/workspace";
import { Providers } from "../pages/catalog";
import { session, team, member, report, markup, testClient, grant } from "../lib/test-fixtures";
import { reportQuery } from "../lib/reports";
describe("enterprise dashboard rendering", () => {
  it("Overview has Create key as its one primary action plus Settings, and no navigation cards (finding #7)", () => {
    const client = testClient(); client.setQueryData(["api", undefined, "/api/v1/workspaces/team/models", "choices"], [grant]); client.setQueryData(["api", undefined, "/api/v1/workspaces/team/service-accounts", "choices"], []);
    const html = markup(<Overview session={session} workspace={team} />, [], client);
    expect(html.match(/data-variant="primary"/g)).toHaveLength(1); expect(html).toMatch(/data-variant="primary"[^>]*>.*Create key/);
    expect(html).toContain('href="/workspaces/team/settings"'); expect(html).not.toContain("team pages"); expect(html).not.toContain("Product pages");
  });
  it("does not fabricate metrics while loading", () => { const html = markup(<Overview session={session} workspace={team} />); expect(html).toContain("Loading this month"); expect(html).not.toContain("$0"); });
  it("renders actual exact counts and unknown execution measurements", () => { const html = markup(<Overview session={session} workspace={team} />, [[`/api/v1/workspaces/team/cost-report?${reportQuery({ ws: "team" }, false)}`, report], ["/api/v1/workspaces/team/executions?limit=5&offset=0", { data: [{ id: "ex", public_model: "company/smart", provider: "openai", state: "cancelled", input_tokens: null, output_tokens: null, elapsed_ms: null, started_at: "2026-01-01T00:00:00Z" }] }]]); expect(html).toContain("$9,007,199,254.740993"); expect(html).toContain("Unknown · "); expect(html).toContain("Cancelled"); expect(html).toContain("Recent activity"); expect(html).not.toContain("0 ms"); });
  it("never gives shared admins platform provider controls", () => { const html = markup(<Providers session={session} />); expect(html).toContain("Access not available"); expect(html).not.toContain("Create connection</button>"); });
  it("offers members useful read-only model guidance", () => { const client = testClient(); client.setQueryData(["api", undefined, "/api/v1/workspaces/team/catalog", "choices"], []); const html = markup(<Grants session={session} workspace={member} />, [], client); expect(html).toContain("Platform Admin"); expect(html).not.toContain("Add model</button>"); });
  it("labels denied and missing resources while escaping error text", () => { expect(renderToStaticMarkup(<ErrorNotice error={new ApiError(403, "forbidden", "No access")} />)).toContain("Access denied"); const html = renderToStaticMarkup(<ErrorNotice error={new ApiError(404, "missing", "<script>bad</script>")} />); expect(html).toContain("Not found"); expect(html).not.toContain("<script>"); });
  it("renders authenticated profile identity rather than example account data", () => { const html = markup(<Profile session={session} />); expect(html).toContain("me@example.invalid"); expect(html).toContain("Example gateway"); expect(html).toContain("membership"); expect(html).not.toContain("Organizations"); });
});

import { wsPath, type Usage, type Execution, type Session } from "../lib/api";
import { permissions } from "../lib/permissions";
import { Badge, CollectionTable, DateTime, ErrorNotice, Heading, Id, Panel, StatCard, useApi } from "../components/ui";
import type { Scope } from "./workspace";

const count = (n: number | null | undefined) => n == null ? "Unknown" : n.toLocaleString();
export function Overview({ session, workspace, organization }: Scope) {
  const path = wsPath(workspace.id);
  const usage = useApi<Usage>(`${path}/usage`);
  const p = permissions(session, organization, workspace);
  return <><Heading title="Workspace overview" description={`${workspace.kind === "personal" ? "Your private workspace" : workspace.kind === "project" ? "Project workspace" : "Team workspace"} · ${p.workspaceAdmin ? "Showing workspace-wide activity" : "Showing activity from your own keys"}`} actions={<button className="button secondary" disabled={usage.isFetching} onClick={() => void usage.refetch()}>Refresh usage</button>} /><section aria-labelledby="usage-title"><div className="section-heading"><h2 id="usage-title">Usage · last 30 days</h2><span className="muted">Accounting, not billing</span></div>{usage.isPending ? <p className="loading" role="status">Loading usage…</p> : usage.isError ? <ErrorNotice error={usage.error} retry={() => void usage.refetch()} /> : <><div className="stats"><StatCard label="Upstream attempts" value={count(usage.data.requests)} /><StatCard label="Known input tokens" value={count(usage.data.input_tokens)} /><StatCard label="Known output tokens" value={count(usage.data.output_tokens)} /><StatCard label="Attempts with unknown usage" value={count(usage.data.unknown_usage_requests)} /></div><p className="help">Attempt counts include fallbacks. Token totals include only reported counts. Missing usage is not zero; these totals do not represent cost or a bill.</p></>}</section><CollectionTable<Execution> path={`${path}/executions`} label="Executions" empty="No inference executions are visible yet. Grant a model and issue an API key to get started." pageSize={50} rowKey={(e) => e.id} columns={[
    { title: "Started", render: (e) => <DateTime value={e.started_at} /> },
    { title: "Model / provider", render: (e) => <><code>{e.public_model}</code><div className="muted">{e.provider}</div></> },
    { title: "State", render: (e) => <Badge tone={e.state === "succeeded" ? "good" : e.state === "failed" ? "bad" : "neutral"}>{e.state}</Badge> },
    { title: "Mode", render: (e) => e.streamed ? "Streamed" : "Standard" },
    { title: "Input / output tokens", render: (e) => `${count(e.input_tokens)} / ${count(e.output_tokens)}` },
    { title: "Duration", render: (e) => e.elapsed_ms == null ? "Unknown" : `${count(e.elapsed_ms)} ms` },
    { title: "Error", render: (e) => e.error_code ? <code>{e.error_code}</code> : <span className="muted">—</span> },
  ]} /><p className="help">Execution history never includes prompts or generated content. A “started” record may be in progress or awaiting reconciliation.</p></>;
}
export function Profile({ session }: { session: Session }) {
  return <><Heading title="Your profile" description="Identity and current access from your signed-in session. Roles are enforced by the gateway." /><Panel title="Identity"><dl className="details"><dt>Email</dt><dd>{session.user.email}</dd><dt>User ID</dt><dd><Id value={session.user.id} /></dd><dt>Platform access</dt><dd><Badge>{session.user.platform_admin ? "Platform operator" : "Standard user"}</Badge></dd><dt>Authentication</dt><dd>Organization identity provider (OIDC)</dd></dl><p className="help">Profile edits and password changes are managed by your identity provider.</p></Panel><Panel title="Organization access">{session.organizations.length ? <ul className="membership-list">{session.organizations.map((org) => <li key={org.id}><div><strong>{org.name}</strong><span className="muted">{org.slug}</span></div><Badge>{org.role}</Badge></li>)}</ul> : <p>No organization memberships. Accept an invitation to get started.</p>}</Panel><Panel title="Workspace access">{session.workspaces.length ? <ul className="membership-list">{session.workspaces.map((ws) => <li key={ws.id}><div><strong>{ws.name}</strong><span className="muted">{session.organizations.find((org) => org.id === ws.organization_id)?.name} · {ws.kind === "personal" ? "Private personal workspace" : ws.kind === "project" ? "Project workspace" : "Team workspace"}</span></div><Badge>{ws.role}</Badge></li>)}</ul> : <p>No workspace memberships.</p>}</Panel></>;
}

import { API, platformPath, type Session } from "../lib/api";
import { ResourceLink } from "../components/navigation-link";
import { Badge, ErrorNotice, Heading, Panel, useCollection } from "../components/ui";

function SetupStep({ path, title, description, search }: {
  path: string; title: string; description: string; search: Parameters<typeof ResourceLink>[0]["search"];
}) {
  const query = useCollection<{ id: string }>(`${path}?limit=1&offset=0`);
  return <li><div className="setup-step-heading"><ResourceLink search={search}>{title}</ResourceLink>
    {query.isPending ? <span role="status">Checking configuration…</span> : query.isError ? <Badge>Unknown</Badge> : <Badge tone={query.data.data.length ? "good" : "neutral"}>{query.data.data.length ? "Configured" : "Not configured"}</Badge>}
  </div><p>{description}</p>{query.isError && <ErrorNotice error={query.error} retry={() => void query.refetch()} />}</li>;
}

export function PlatformOverview({ session }: { session: Session }) {
  if (!session.user.platform_admin) return <Heading title="Access not available" />;
  return <>
    <Heading title="Platform overview" description="Configure model infrastructure, then grant consuming organizations access. Workspace operations stay in the Workspace portal." />
    <Panel title="Set up this gateway"><ol className="setup-steps">
      <SetupStep path={`${platformPath}/providers`} title="Configure provider connections" description="Use allowlisted credential references. Secret values are never entered into these forms." search={{ page: "providers" }} />
      <SetupStep path={`${platformPath}/models`} title="Create model aliases" description="Give clients stable API model names independent of upstream deployments." search={{ page: "models" }} />
      <SetupStep path={`${platformPath}/deployments`} title="Connect deployments and publish pricing" description="Open a deployment to review routing, exact USD rates, and hard token ceilings." search={{ page: "deployments" }} />
      <SetupStep path={`${API}/orgs`} title="Assign models to organizations" description="Open an organization to manage entitlements and ceilings, then delegate models to shared workspaces." search={{ page: "organizations" }} />
    </ol><p className="help">Configured means a record exists, not that credentials, pricing, entitlements, or upstream inference have been verified. This page never sends provider probes or paid inference.</p></Panel>
    <Panel title="Review operations"><div className="actions">
      <ResourceLink className="button secondary" search={{ page: "platform-audit" }}>Review platform audit</ResourceLink>
      <ResourceLink className="button secondary" search={{ page: "platform-teams" }}>Browse teams</ResourceLink>
      <ResourceLink className="button secondary" search={{ page: "platform-projects" }}>Browse projects</ResourceLink>
    </div><p className="help">Shared directories exclude personal workspaces. Usage and cost records remain in their authorized workspace, with unknown charges shown explicitly.</p></Panel>
  </>;
}

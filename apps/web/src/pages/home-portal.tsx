/*
 * Home (Workspace portal, user-level): a welcome, the models you can use
 * across your workspaces, your own usage this month, your own API keys and
 * your teams & projects with quick links.
 *
 * Privacy: Home shows only the signed-in person's own data, whatever their
 * role (own human keys and their usage via /me/summary and /me/keys; the
 * model lists of their own workspaces). Workspace-wide totals belong on each
 * workspace's Overview.
 *
 * Provenance: layout follows Grounded web/src/pages/user.tsx (HomePage,
 * YourTeams) and pages/chat/directory.tsx tiles (read-only reference).
 */
import { useQueries } from "@tanstack/react-query";
import { ArrowRight, Boxes, KeyRound, Plus, Shield, UsersRound } from "lucide-react";
import type { ReactElement, ReactNode } from "react";
import { api, wsPath, type Collection, type Session } from "../lib/api";
import { formatMicroUsd } from "../lib/governance";
import { countLabel, formatCount } from "../lib/reports";
import { ME_KEYS, ME_SUMMARY, addedModelCount, keyWorkspaces, modelsAcrossWorkspaces, monthName, notAvailable, sharedMemberships, topKeys, welcomeTitle, type HomeCatalogRow, type MeKey, type MeSummary } from "../lib/home";
import { personalWorkspace, portalWorkspaces, rememberedWorkspace } from "../lib/navigation";
import { kindLabels } from "../lib/people";
import { ErrorNotice, Stack, useApi, useApiScope } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { RoleBadge } from "../components/people";
import { IconCell, LabIcon } from "../components/provider-icon";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { UsageBar } from "../components/templates/usage-bar";
import { Avatar } from "../components/ui/avatar/avatar";
import { Badge } from "../components/ui/badge/badge";
import { Button } from "../components/ui/button/button";
import { Card } from "../components/ui/card/card";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { Menu, MenuItem, MenuLinkItem } from "../components/ui/menu/menu";
import { PageHeader } from "../components/ui/page-header/page-header";
import { Skeleton } from "../components/ui/skeleton/skeleton";
import { Time } from "../components/ui/time/time";
import s from "./shared.module.css";
import h from "./home-portal.module.css";
import { utcDay } from "../lib/format";

export function HomePortal({ session }: { session: Session }) {
  const shared = sharedMemberships(session), catalogs = useWorkspaceCatalogs(session);
  return <Stack gap={6} className={s.page}>
    <PageHeader title={welcomeTitle(session)} description={shared.length ? "Use approved models from your personal workspace and your teams and projects." : "Use approved models from your personal workspace. Teams and projects you join appear here too."} />
    {session.capabilities.platform_read && <p className={h.adminLine}><Shield aria-hidden className={h.adminIcon} /> You also have {session.capabilities.platform_write ? "Admin" : "read-only Admin"} access. Other teams and projects are managed there. <ResourceLink search={{ page: "platform-overview" }}>Open Admin</ResourceLink></p>}
    <ModelsYouCanUse session={session} catalogs={catalogs} />
    <YourUsage session={session} />
    <YourKeys session={session} catalogs={catalogs} />
    <YourTeams session={session} />
  </Stack>;
}

function SeeAll({ children, render }: { children: ReactNode; render: ReactElement }) { return <Button variant="ghost" size="sm" render={render}>{children} <ArrowRight aria-hidden /></Button>; }
const Loading = ({ label }: { label: string }) => <div role="status" aria-label={`Loading ${label}…`}><Skeleton height="5.5rem" /></div>;
const NotYet = ({ what }: { what: string }) => <EmptyState size="compact" title="Not available yet." description={`This gateway doesn't report ${what} yet.`} />;

/** Every workspace's catalog (eligibility and enabled routes), shared with the Models page's cache. */
function useWorkspaceCatalogs(session: Session) {
  const scope = useApiScope(), workspaces = portalWorkspaces(session);
  const results = useQueries({ queries: workspaces.map(w => { const path = `${wsPath(w.id)}/catalog`; return { queryKey: ["api", scope, path, "choices"], retry: false, queryFn: async ({ signal }: { signal: AbortSignal }) => { const rows: HomeCatalogRow[] = []; for (let offset = 0; offset < 20000; offset += 200) { const page = await api<Collection<HomeCatalogRow>>(`${path}?limit=200&offset=${offset}`, { signal }); rows.push(...page.data); if (page.has_more === false || page.data.length < 200) return rows; } return rows; } }; }) });
  return { workspaces, results };
}
type Catalogs = ReturnType<typeof useWorkspaceCatalogs>;

function selectedOnHome(session: Session) { try { return rememberedWorkspace(sessionStorage, session); } catch { return undefined; } }
/** Model cards across your workspaces; each links to that workspace's Models page. Models that can't serve are flagged, not listed as usable. */
function ModelsYouCanUse({ session, catalogs }: { session: Session; catalogs: Catalogs }) {
  const { workspaces, results } = catalogs, personal = personalWorkspace(session);
  const { usable: models, notServing } = modelsAcrossWorkspaces(workspaces.map((workspace, i) => ({ workspace, rows: results[i]?.data })));
  const pending = results.some(r => r.isPending), failed = results.filter(r => r.isError);
  // "All models" opens the workspace selected in the sidebar (review #42), not always Personal.
  const selected = selectedOnHome(session) ?? personal ?? workspaces[0];
  return <Card title="Models you can use" description={workspaces.length > 1 ? "Models added in your workspaces that can serve requests. Keys call them from your code." : "Models added in your personal workspace that can serve requests. Keys call them from your code."} actions={selected && <SeeAll render={<ResourceLink search={{ page: "grants", ws: selected.id }} aria-label={`All models in ${selected.name}`} />}>All models</SeeAll>}>
    {pending && !models.length && !notServing.length ? <Loading label="models" /> : <Stack gap={3}>
      {failed.length > 0 && <ErrorNotice error={failed[0]!.error} retry={() => failed.forEach(r => void r.refetch())} />}
      {models.length === 0 ? <EmptyState size="compact" icon={<Boxes />} title={notServing.length ? "No model can serve requests yet." : "No models yet."} description={notServing.length ? "The models added in your workspaces have no enabled route to a provider. Ask a Platform Admin." : personal?.capabilities.delegate_models ? "Add models to your personal workspace to start creating keys." : "No models are enabled in your workspaces yet. Ask a Platform Admin."} action={!notServing.length && personal?.capabilities.delegate_models && <Button variant="secondary" render={<ResourceLink search={{ page: "grants", ws: personal.id }} />}>Add models</Button>} /> :
        <ul className={h.grid} aria-label="Models you can use">{models.slice(0, 6).map(m => <li key={m.model.model_id}>
          <ResourceLink className={h.tile} search={{ page: "workspace-model", ws: m.workspaces[0]!.id, record: m.model.model_id }}>
            <IconCell icon={<LabIcon model={[m.model.public_name, m.model.display_name]} />}><span className={h.tileName}>{m.model.display_name || m.model.public_name}</span><span className={h.tileMeta}>{m.model.public_name}</span></IconCell>
            <span className={h.tileBadges}>{m.workspaces.slice(0, 3).map(w => <Badge key={w.id} size="sm" variant="outline">{w.name}</Badge>)}{m.workspaces.length > 3 && <Badge size="sm" variant="outline">+{m.workspaces.length - 3}</Badge>}</span>
            <ArrowRight aria-hidden className={h.arrow} />
          </ResourceLink>
        </li>)}</ul>}
      {models.length > 6 && <p className={s.note}>{models.length - 6} more in your workspaces' Models pages.</p>}
      {notServing.length > 0 && <p className={h.notServing}><Badge size="sm" tone="warning" dot>Not serving</Badge> {notServing.map((m, i) => <span key={m.model.model_id}>{i > 0 && ", "}<ResourceLink search={{ page: "workspace-model", ws: m.workspaces[0]!.id, record: m.model.model_id }}>{m.model.display_name || m.model.public_name}</ResourceLink></span>)}: added, but no enabled route to a provider, so requests fail.</p>}
    </Stack>}
  </Card>;
}

/** Own spend, requests and tokens this month (your human keys only), with the change against last month. */
function YourUsage({ session }: { session: Session }) {
  const q = useApi<MeSummary>(ME_SUMMARY), personal = personalWorkspace(session);
  // Several workspaces: the "Usage by workspace" row links each one; with one, the header links its Usage & costs (review #42).
  const several = (q.data?.workspaces?.length ?? 0) > 1, only = personal ?? portalWorkspaces(session)[0];
  const action = !several && only && <SeeAll render={<ResourceLink search={{ page: "costs", ws: only.id }} />}>Usage &amp; costs</SeeAll>;
  const title = "Your usage this month";
  if (q.isPending) return <Card title={title} actions={action}><Loading label="your usage" /></Card>;
  if (q.isError) return <Card title={title} actions={action}>{notAvailable(q.error) ? <NotYet what="your own usage" /> : <ErrorNotice error={q.error} retry={() => void q.refetch()} />}</Card>;
  if (!q.data?.totals?.current || !q.data.period?.current) return <Card title={title} actions={action}><NotYet what="your own usage" /></Card>;
  const d = q.data, now = d.totals.current, prev = d.totals.previous, month = monthName(d.period.previous.start_date);
  const vs = month ? `vs all of ${month}` : "vs last month";
  const held = now.held_microusd && now.held_microusd !== "0", unknown = now.unresolved_attempts && now.unresolved_attempts !== "0", partialTokens = now.unknown_token_attempts && now.unknown_token_attempts !== "0";
  return <Card title={title} description={`Your own API keys across all your workspaces, ${utcDay(d.period.current.start_date)} to today (UTC). Estimates from configured prices, not invoices.`} actions={action}>
    <Stack gap={4}>
      <StatTileGrid columns={3} label="Your usage this month">
        <StatTile label="Spent" value={now.known_cost_microusd == null ? null : formatMicroUsd(now.known_cost_microusd)} hint={held || unknown ? `${held ? `${formatMicroUsd(now.held_microusd)} on hold` : ""}${held && unknown ? " · " : ""}${unknown ? `${formatCount(now.unresolved_attempts)} cost unknown` : ""}` : "Final costs"} delta={{ current: now.known_cost_microusd, previous: prev?.known_cost_microusd, increaseIs: "neutral", label: vs }} />
        <StatTile label="Requests" value={now.requests == null ? null : formatCount(now.requests)} delta={{ current: now.requests, previous: prev?.requests, increaseIs: "neutral", label: vs }} />
        <StatTile label="Tokens" value={now.tokens == null ? null : `${partialTokens ? "At least " : ""}${formatCount(now.tokens)}`} hint={partialTokens ? `${countLabel(now.unknown_token_attempts, "request")} didn't report tokens` : "Input and output"} delta={{ current: partialTokens ? null : now.tokens, previous: prev?.tokens, increaseIs: "neutral", label: vs }} />
      </StatTileGrid>
      {several && <p className={h.byWorkspace}><span className={s.muted}>Usage by workspace: </span>{d.workspaces.map((w, i) => <span key={w.workspace_id}>{i > 0 && <span aria-hidden className={s.muted}> · </span>}<ResourceLink search={{ page: "costs", ws: w.workspace_id }}>{w.name}</ResourceLink> {formatMicroUsd(w.current.known_cost_microusd)}</span>)}</p>}
    </Stack>
  </Card>;
}

/** Your own active human keys (top 5 by spend in their budget window). Service-account keys live on each workspace's keys page. */
function YourKeys({ session, catalogs }: { session: Session; catalogs: Catalogs }) {
  const q = useApi<Collection<MeKey>>(ME_KEYS), targets = keyWorkspaces(session);
  // As on API keys: a workspace without models can't get a useful key, so it's offered disabled with the reason.
  const models = (id: string) => addedModelCount(catalogs.results[catalogs.workspaces.findIndex(w => w.id === id)]?.data);
  const create = targets.length > 0 && <Menu align="end" trigger={<Button variant="ghost" size="sm"><Plus aria-hidden /> Create key</Button>}>{targets.map(w => models(w.id) === 0
    ? <MenuItem key={w.id} disabled icon={<Avatar name={w.name} shape="square" size="xs" decorative />}>{w.name} · Add a model first</MenuItem>
    : <MenuLinkItem key={w.id} render={<ResourceLink search={{ page: "keys", ws: w.id }} />} icon={<Avatar name={w.name} shape="square" size="xs" decorative />}>{w.name}</MenuLinkItem>)}</Menu>;
  const title = "Your API keys";
  if (q.isPending) return <Card title={title} actions={create}><Loading label="your API keys" /></Card>;
  if (q.isError) return <Card title={title} actions={create}>{notAvailable(q.error) ? <NotYet what="your keys across workspaces" /> : <ErrorNotice error={q.error} retry={() => void q.refetch()} />}</Card>;
  if (!Array.isArray(q.data?.data)) return <Card title={title} actions={create}><NotYet what="your keys across workspaces" /></Card>;
  const active = q.data.data.filter(k => k.status === "active"), rows = topKeys(active);
  return <Card title={title} description={active.length ? `${active.length.toLocaleString()} active ${active.length === 1 ? "key" : "keys"}${q.data.has_more ? " or more" : ""} you created, across your workspaces. Keys in Personal are visible only to you; a team's or project's admins can see keys in it.` : undefined} actions={create} flush={rows.length > 0}>
    {rows.length === 0 ? <EmptyState size="compact" icon={<KeyRound />} title="You have no active keys." description="Create a key to call the API from your code. Keys belong to one workspace." /> :
      <ul className={h.rows} aria-label="Your API keys">{rows.map(k => <li key={k.id} className={h.keyRow}>
        <span className={h.keyName}><ResourceLink search={{ page: "keys", ws: k.workspace.id }}>{k.name}</ResourceLink><span className={h.tileMeta}>{k.workspace.name} · {k.last_used_at ? <>last used <Time value={k.last_used_at} format="relative" /></> : "never used"} · expires <Time value={k.expires_at} format="relative" /></span></span>
        <span className={h.keyUsage}>{k.usage ? <UsageBar label={`${k.name} budget`} size="sm" used={k.usage.used_microusd} limit={k.usage.limit_microusd} period={k.usage.period} /> : <span className={s.muted}>Usage unknown</span>}</span>
      </li>)}</ul>}
  </Card>;
}

/** Shared memberships as compact rows with quick links (Grounded "Your teams"). */
function YourTeams({ session }: { session: Session }) {
  const shared = sharedMemberships(session), title = "Your teams & projects";
  if (!shared.length) return <Card title={title}><EmptyState size="compact" icon={<UsersRound />} title="You're not in a team or project yet." description="Teams and projects are shared workspaces with their own models, keys and budget. Platform Admins create them and add people; a workspace admin can also send you an invitation." action={<Button variant="secondary" render={<ResourceLink search={{ page: "accept-invitation" }} />}>Accept an invitation</Button>} /></Card>;
  const teams = shared.filter(w => w.kind === "team").length, projects = shared.length - teams;
  return <Card title={title} description={[teams && `${teams} ${teams === 1 ? "team" : "teams"}`, projects && `${projects} ${projects === 1 ? "project" : "projects"}`].filter(Boolean).join(" · ")}>
    <ul className={h.teams} aria-label={title}>{shared.map(w => <li key={w.id} className={h.teamRow}>
      <Avatar name={w.name} shape="square" size="md" decorative />
      <span className={h.teamText}><ResourceLink className={h.teamLink} search={{ page: "overview", ws: w.id }}>{w.name}</ResourceLink><span className={h.teamBadges}><RoleBadge role={w.role} /><Badge size="sm" variant="outline">{kindLabels[w.kind]}</Badge></span></span>
      <span className={h.quickLinks}>
        <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "grants", ws: w.id }} aria-label={`${w.name}: models`} />}>Models</Button>
        <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "keys", ws: w.id }} aria-label={`${w.name}: API keys`} />}>API keys</Button>
        <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "costs", ws: w.id }} aria-label={`${w.name}: usage`} />}>Usage</Button>
      </span>
    </li>)}</ul>
  </Card>;
}

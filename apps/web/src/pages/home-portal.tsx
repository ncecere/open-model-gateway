/*
 * Home (Workspace portal, user-level): a greeting, your workspaces with quick
 * links, the models you can use, your own usage this month as three tiles
 * and your own API keys. Few words: badges and tooltips instead of sentences.
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
import { ArrowRight, Boxes, KeyRound, Plus } from "lucide-react";
import type { ReactElement, ReactNode } from "react";
import { api, wsPath, type Collection, type Session, type Workspace } from "../lib/api";
import { formatMicroUsd } from "../lib/governance";
import { formatCount } from "../lib/reports";
import { ME_KEYS, ME_SUMMARY, addedModelCount, keyWorkspaces, modelsAcrossWorkspaces, notAvailable, sharedMemberships, topKeys, welcomeTitle, type HomeCatalogRow, type MeKey, type MeSummary } from "../lib/home";
import { personalWorkspace, portalWorkspaces, rememberedWorkspace } from "../lib/navigation";
import { kindLabels } from "../lib/people";
import { ErrorNotice, Stack, useApi, useApiScope } from "../components/ui";
import { ResourceLink } from "../components/navigation-link";
import { RoleBadge } from "../components/people";
import { IconCell, LabIcon } from "../components/provider-icon";
import { HintBadge } from "../components/templates/hint-badge";
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

export function HomePortal({ session }: { session: Session }) {
  const catalogs = useWorkspaceCatalogs(session);
  // Admin is reached from the sidebar's Workspace | Admin switch (as in Grounded), not a second header button.
  return <Stack gap={6} className={s.page}>
    <PageHeader title={welcomeTitle(session)} description="Your workspaces, models, keys and usage." />
    <YourWorkspaces session={session} />
    <ModelsYouCanUse session={session} catalogs={catalogs} />
    <YourUsage session={session} />
    <YourKeys session={session} catalogs={catalogs} />
  </Stack>;
}

function SeeAll({ children, render }: { children: ReactNode; render: ReactElement }) { return <Button variant="ghost" size="sm" render={render}>{children} <ArrowRight aria-hidden /></Button>; }
const Loading = ({ label }: { label: string }) => <div role="status" aria-label={`Loading ${label}…`}><Skeleton height="4.5rem" /></div>;
const NotYet = ({ what }: { what: string }) => <EmptyState size="compact" title="Not available yet." description={`This gateway doesn't report ${what} yet.`} />;

/** Every workspace's catalog (eligibility and enabled routes), shared with the Models page's cache. */
function useWorkspaceCatalogs(session: Session) {
  const scope = useApiScope(), workspaces = portalWorkspaces(session);
  const results = useQueries({ queries: workspaces.map(w => { const path = `${wsPath(w.id)}/catalog`; return { queryKey: ["api", scope, path, "choices"], retry: false, queryFn: async ({ signal }: { signal: AbortSignal }) => { const rows: HomeCatalogRow[] = []; for (let offset = 0; offset < 20000; offset += 200) { const page = await api<Collection<HomeCatalogRow>>(`${path}?limit=200&offset=${offset}`, { signal }); rows.push(...page.data); if (page.has_more === false || page.data.length < 200) return rows; } return rows; } }; }) });
  return { workspaces, results };
}
type Catalogs = ReturnType<typeof useWorkspaceCatalogs>;

/** Personal first, then teams and projects: one compact row each with role and quick links (Grounded "Your teams"). */
function YourWorkspaces({ session }: { session: Session }) {
  const personal = personalWorkspace(session), list: Workspace[] = [...(personal && portalWorkspaces(session).some(w => w.id === personal.id) ? [personal] : []), ...sharedMemberships(session)];
  const invite = !sharedMemberships(session).length && <Button variant="ghost" size="sm" render={<ResourceLink search={{ page: "accept-invitation" }} />}>Accept an invitation</Button>;
  const title = "Your workspaces";
  return <Card title={title} actions={invite || undefined} flush>
    {list.length === 0 ? <EmptyState size="compact" title="No workspaces yet." /> :
      <ul className={h.rows} aria-label={title}>{list.map(w => <li key={w.id} className={h.wsRow}>
        <Avatar name={w.name} shape="square" size="sm" decorative />
        <span className={h.wsName}><ResourceLink className={h.teamLink} search={{ page: "overview", ws: w.id }}>{w.name}</ResourceLink>{w.kind === "personal" ? <Badge size="sm" variant="outline">Private</Badge> : <><RoleBadge role={w.role} /><span className={s.muted}>{kindLabels[w.kind]}</span></>}</span>
        <span className={h.quickLinks}>
          <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "grants", ws: w.id }} aria-label={`${w.name}: models`} />}>Models</Button>
          <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "keys", ws: w.id }} aria-label={`${w.name}: API keys`} />}>API keys</Button>
          <Button size="sm" variant="ghost" render={<ResourceLink search={{ page: "costs", ws: w.id }} aria-label={`${w.name}: usage`} />}>Usage</Button>
        </span>
      </li>)}</ul>}
  </Card>;
}

function selectedOnHome(session: Session) { try { return rememberedWorkspace(sessionStorage, session); } catch { return undefined; } }
/** Up to six model tiles across your workspaces; models that can't serve are a badge (names in its tooltip), never listed as usable. */
function ModelsYouCanUse({ session, catalogs }: { session: Session; catalogs: Catalogs }) {
  const { workspaces, results } = catalogs, personal = personalWorkspace(session);
  const { usable: models, notServing } = modelsAcrossWorkspaces(workspaces.map((workspace, i) => ({ workspace, rows: results[i]?.data })));
  const pending = results.some(r => r.isPending), failed = results.filter(r => r.isError);
  // "All models" opens the workspace selected in the sidebar (review #42), not always Personal.
  const selected = selectedOnHome(session) ?? personal ?? workspaces[0];
  const all = selected && { page: "grants", ws: selected.id } as const;
  return <Card title="Models you can use" actions={all && <SeeAll render={<ResourceLink search={all} aria-label={`All models in ${selected.name}`} />}>All models</SeeAll>}>
    {pending && !models.length && !notServing.length ? <Loading label="models" /> : <Stack gap={3}>
      {failed.length > 0 && <ErrorNotice error={failed[0]!.error} retry={() => failed.forEach(r => void r.refetch())} />}
      {models.length === 0 ? <EmptyState size="compact" icon={<Boxes />} title={notServing.length ? "No model can serve requests yet." : "No models yet."} description={notServing.length ? "Ask a Platform Admin to enable a route." : undefined} action={!notServing.length && personal?.capabilities.delegate_models && <Button variant="secondary" render={<ResourceLink search={{ page: "grants", ws: personal.id }} />}>Add models</Button>} /> :
        <ul className={h.grid} aria-label="Models you can use">{models.slice(0, 6).map(m => <li key={m.model.model_id}>
          <ResourceLink className={h.tile} search={{ page: "workspace-model", ws: m.workspaces[0]!.id, record: m.model.model_id }}>
            <IconCell icon={<LabIcon model={[m.model.public_name, m.model.display_name]} />}><span className={h.tileName}>{m.model.display_name || m.model.public_name}</span><span className={h.tileMeta}>{m.model.public_name}</span></IconCell>
            <span className={h.tileBadges}>{m.workspaces.slice(0, 2).map(w => <Badge key={w.id} size="sm" variant="outline">{w.name}</Badge>)}{m.workspaces.length > 2 && <Badge size="sm" variant="outline">+{m.workspaces.length - 2}</Badge>}</span>
          </ResourceLink>
        </li>)}</ul>}
      {(models.length > 6 || notServing.length > 0) && <p className={h.footLine}>
        {models.length > 6 && all && <ResourceLink search={all}>+{models.length - 6} more</ResourceLink>}
        {notServing.length > 0 && <HintBadge tone="warning" hint={`${notServing.map(m => m.model.display_name || m.model.public_name).join(", ")}: added, but no enabled route to a provider, so requests fail.`}>{notServing.length} not serving</HintBadge>}
      </p>}
    </Stack>}
  </Card>;
}

/** Own spend, requests and tokens this month (your human keys only): three small tiles, caveats as short hints. */
function YourUsage({ session }: { session: Session }) {
  const q = useApi<MeSummary>(ME_SUMMARY), only = personalWorkspace(session) ?? portalWorkspaces(session)[0];
  const head = <div className={h.sectionHead}><h2 id="home-usage" className={s.settingTitle}>Your usage this month</h2>{only && <SeeAll render={<ResourceLink search={{ page: "costs", ws: only.id }} />}>Usage &amp; costs</SeeAll>}</div>;
  const body = q.isPending ? <Loading label="your usage" />
    : q.isError ? notAvailable(q.error) ? <NotYet what="your own usage" /> : <ErrorNotice error={q.error} retry={() => void q.refetch()} />
    : !q.data?.totals?.current ? <NotYet what="your own usage" />
    : (() => {
      const now = q.data.totals.current;
      const held = now.held_microusd && now.held_microusd !== "0", unknown = now.unresolved_attempts && now.unresolved_attempts !== "0", partialTokens = now.unknown_token_attempts && now.unknown_token_attempts !== "0";
      return <StatTileGrid columns={3} label="Your usage this month">
        <StatTile label="Spent" value={now.known_cost_microusd == null ? null : formatMicroUsd(now.known_cost_microusd)} hint={held ? `+${formatMicroUsd(now.held_microusd)} on hold` : unknown ? `${formatCount(now.unresolved_attempts)} cost unknown` : "Estimated"} />
        <StatTile label="Requests" value={now.requests == null ? null : formatCount(now.requests)} hint="Your keys" />
        <StatTile label="Tokens" value={now.tokens == null ? null : `${partialTokens ? "≥ " : ""}${formatCount(now.tokens)}`} hint={partialTokens ? `${formatCount(now.unknown_token_attempts)} not reported` : "Input and output"} />
      </StatTileGrid>;
    })();
  return <section aria-labelledby="home-usage"><Stack gap={3}>{head}{body}</Stack></section>;
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
  const rows = topKeys(q.data.data.filter(k => k.status === "active"));
  return <Card title={title} actions={create} flush={rows.length > 0}>
    {rows.length === 0 ? <EmptyState size="compact" icon={<KeyRound />} title="No active keys." /> :
      <ul className={h.rows} aria-label="Your API keys">{rows.map(k => <li key={k.id} className={h.keyRow}>
        <span className={h.keyName}><ResourceLink search={{ page: "key-detail", ws: k.workspace.id, record: k.id }}>{k.name}</ResourceLink><span className={h.tileMeta}>{k.workspace.name} · {k.last_used_at ? <>used <Time value={k.last_used_at} format="relative" /></> : "never used"}</span></span>
        <span className={h.keyUsage}>{k.usage ? <UsageBar label={`${k.name} budget`} size="sm" used={k.usage.used_microusd} limit={k.usage.limit_microusd} period={k.usage.period} /> : <span className={s.muted}>Usage unknown</span>}</span>
      </li>)}</ul>}
  </Card>;
}

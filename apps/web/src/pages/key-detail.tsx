/*
 * One API key on its own page: "Back to API keys" and previous/next within
 * the list's filters, then pill tabs (ui-principles 7, 8; `?tab=`):
 * - Overview: settings (name, owner, expiry, model access, all read-only
 *   because the server fixes them), today / this week / this month totals,
 *   spending over the last 30 days, and the danger zone (Disable or Enable,
 *   Rotate, Revoke).
 * - Limits: the key's limits edited inline (stacked budgets, tighten-only) and
 *   a budget ring per applicable budget (every layer, from the key's stats).
 * - Access: effective access by layer with "why can't I use this model?".
 * Revoked and expired keys are final: no danger zone, read-only copy, "View
 * requests" stays in the header. Key lineage: rotation keeps limits and usage.
 *
 * The key itself comes from GET …/keys/{id} (same visibility as the list: a
 * member asking for someone else's key gets 404). The list is read only for
 * previous/next and never blocks the page.
 */
import { DetailTime } from "../components/templates/when";
import type { ReactNode } from "react";
import { FileQuestion, Gauge, LayoutDashboard, ShieldCheck } from "lucide-react";
import { EmptyState } from "../components/ui/empty-state/empty-state";
import { ApiError, wsPath, type Grant, type Member, type ServiceAccount } from "../lib/api";
import { formatMicroUsd } from "../lib/governance";
import { canDisable, canEditKeyLimits, canEnable, canRevoke, canRotate, filterKeys, issuedTo, keyListStatus, keyPill, keyStatus, type KeyRow, type KeyStats, type KeyTotals } from "../lib/keys";
import { keyModelOptions, keyModelSummary } from "../lib/key-models";
import { periodName } from "../lib/limits";
import { countText } from "../lib/requests";
import { longDate, shortDate } from "../lib/usage";
import { permissions, type DashboardSearch } from "../lib/permissions";
import { Button, ErrorNotice, Stack, useAction, useApi, useChoices } from "../components/ui";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { ScopeLimits } from "../components/scope-limits";
import { EffectiveAccess } from "../components/effective-access";
import { BackLink } from "../components/templates/form-page";
import { BudgetRing } from "../components/templates/budget-ring";
import { CopyId } from "../components/templates/copy-id";
import { DangerAction, DangerZone } from "../components/templates/notices";
import { PrevNext } from "../components/templates/prev-next";
import { StatTile, StatTileGrid } from "../components/templates/stat-tile";
import { StatusPill } from "../components/templates/status-pill";
import { TypeTabs } from "../components/templates/type-tabs";
import { BarChart } from "../components/ui/bar-chart/bar-chart";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { PageHeader } from "../components/ui/page-header/page-header";
import { Time } from "../components/ui/time/time";
import { keyActions } from "./keys";
import type { Scope } from "./workspace";
import s from "./shared.module.css";
import k from "./keys.module.css";

const layerNames = { platform: "Platform", local: "Workspace", key: "Key" } as const;
function totalsHint(t: KeyTotals) {
  const held = t.held_microusd !== "0" ? `${formatMicroUsd(t.held_microusd)} on hold · ` : "";
  return `${held}${countText(t.requests)} request${t.requests === "1" ? "" : "s"}${t.unresolved_attempts !== "0" ? ` · ${countText(t.unresolved_attempts)} cost unknown` : ""}`;
}
/** Micro-USD as a Number only to size bars; labels format the exact integer string. */
const microNumber = (value: string) => /^\d{1,15}$/.test(value) ? Number(value) : Number.NaN;

export function KeyUsage({ workspace, keyId, part = "spending" }: { workspace: Scope["workspace"]; keyId: string; /** Spending totals and chart (Overview) or budget rings (Limits). */ part?: "spending" | "budgets" }) {
  const q = useApi<KeyStats>(`${wsPath(workspace.id)}/keys/${encodeURIComponent(keyId)}/stats`);
  if (q.isPending) return <p role="status">Loading usage…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  const stats = q.data, total = stats.daily.reduce((sum, d) => sum + BigInt(/^\d+$/.test(d.spend_microusd) ? d.spend_microusd : "0"), 0n);
  if (part === "budgets") return <KeyBudgets stats={stats} />;
  return <Stack gap={6}>
    <StatTileGrid columns={3} label="Spending totals (UTC)">
      <StatTile label="Today" value={formatMicroUsd(stats.totals.today.spend_microusd)} hint={totalsHint(stats.totals.today)} />
      <StatTile label="This week" value={formatMicroUsd(stats.totals.week.spend_microusd)} hint={totalsHint(stats.totals.week)} />
      <StatTile label="This month" value={formatMicroUsd(stats.totals.month.spend_microusd)} hint={totalsHint(stats.totals.month)} />
    </StatTileGrid>
    <Card title="Spending, last 30 days" titleAs="h2" description="Per UTC day, including earlier versions of this key.">
      <BarChart layout="stack" size="sm" data={stats.daily.map(d => ({ label: shortDate(d.date), values: { spend: microNumber(d.spend_microusd), held: microNumber(d.held_microusd) } }))} series={[{ key: "spend", label: "Spent" }, { key: "held", label: "On hold", tone: "warning" }]}
        summary={`Spent ${formatMicroUsd(total.toString())} over the last 30 days${stats.daily.length ? `, ${longDate(stats.daily[0]!.date)} to ${longDate(stats.daily.at(-1)!.date)} (UTC days)` : ""}.`} formatValue={v => Number.isFinite(v) ? formatMicroUsd(String(Math.round(v))) : "Unknown"} dataTable={{ caption: "Spending per day", labelHeader: "UTC day" }} />
    </Card>
  </Stack>;
}
/** Every budget that applies to this key (key, workspace, platform), each in its current window; spent plus on hold. */
function KeyBudgets({ stats }: { stats: KeyStats }) {
  return <Card title="Budgets" titleAs="h2" description="Every budget that applies to this key. Spent plus on hold.">
      {stats.budgets.length === 0 ? <p className={s.note}>No budget applies to this key.</p> : <div className={k.rings}>{stats.budgets.map(w => { const period = w.period ?? w.budget_period, amount = w.amount_microusd ?? w.monthly_budget_microusd, label = `${layerNames[w.layer]} ${periodName[period].toLowerCase()}`; return w.usage_visible
        ? <div key={`${w.layer}-${period}`}><BudgetRing label={label} used={w.used_microusd} limit={amount} period={period} />{w.unresolved_usage && <span className={s.secondary}>Some costs aren't known yet: at least this much.</span>}</div>
        : <p key={`${w.layer}-${period}`} className={s.note}>{label}: {formatMicroUsd(amount)}. Only workspace admins see how much of it is used.</p>; })}</div>}
    </Card>;
}
const keyTabs = [{ value: "overview", label: "Overview", icon: <LayoutDashboard aria-hidden /> }, { value: "limits", label: "Limits", icon: <Gauge aria-hidden /> }, { value: "access", label: "Access", icon: <ShieldCheck aria-hidden /> }];

export function KeyDetail({ session, workspace, id }: Scope & { id: string }) {
  const ask = useAction(), nav = useDashboardNavigation(), search: DashboardSearch = nav?.search ?? { page: "key-detail", ws: workspace.id, record: id }, p = permissions(session, workspace);
  const one = useApi<KeyRow>(`${wsPath(workspace.id)}/keys/${encodeURIComponent(id)}`), key = one.data?.id === id ? one.data : undefined;
  const keys = useChoices<KeyRow>(`${wsPath(workspace.id)}/keys`), grants = useChoices<Grant>(`${wsPath(workspace.id)}/models`), accounts = useChoices<ServiceAccount>(`${wsPath(workspace.id)}/service-accounts`, p.manageServiceAccounts);
  useResourceName(key?.name ?? "API key");
  const listSearch = { status: search.status, q: search.q };
  const back = { label: "API keys", search: { page: "keys", ws: workspace.id, ...listSearch } as DashboardSearch };
  const members = useChoices<Member>(`${wsPath(workspace.id)}/members`, workspace.kind !== "personal" && workspace.capabilities.view_all_activity);
  // Previous/next within the list's current filters and order (every key when this one isn't in the filtered list).
  const filteredList = filterKeys(keys.data ?? [], keyListStatus(search.status), search.q), list = filteredList.some(x => x.id === id) ? filteredList : filterKeys(keys.data ?? [], undefined, search.q), index = list.findIndex(x => x.id === id);
  const target = (other?: KeyRow) => other ? { label: other.name, render: <ResourceLink search={{ page: "key-detail", ws: workspace.id, record: other.id, ...listSearch }} /> } : null;
  const header = (title: string, meta?: ReactNode, actions?: ReactNode) => <PageHeader title={title} meta={meta} breadcrumbs={<BackLink {...back} />} actions={<>{actions}<PrevNext noun="key" shortcuts position={index >= 0 ? { index, total: list.length } : undefined} prev={target(list[index - 1])} next={target(index >= 0 ? list[index + 1] : undefined)} /></>} />;
  const missing = one.error instanceof ApiError && one.error.status === 404 || one.isSuccess && !key;
  if (one.isPending) return <Stack gap={6} className={s.page}>{header("API key")}<p role="status">Loading key…</p></Stack>;
  if (one.isError && !missing) return <Stack gap={6} className={s.page}>{header("API key")}<ErrorNotice error={one.error} retry={() => void one.refetch()} /></Stack>;
  // Same not-found state as the request and model pages (rule 8): plain words, Back to API keys, no previous/next.
  if (!key) return <Stack gap={6} className={s.page}><PageHeader title="Key not found" breadcrumbs={<BackLink {...back} />} /><EmptyState icon={<FileQuestion />} titleAs="h2" title="This key doesn't exist or you can't see it" description="Members see only their own keys." action={<Button variant="secondary" render={<ResourceLink search={back.search} />}>Back to API keys</Button>} /></Stack>;
  const status = keyStatus(key), models = keyModelSummary(key, keyModelOptions(grants.data ?? []));
  // Revoked and expired keys are final: no danger zone, read-only copy throughout.
  const final = status === "revoked" || status === "expired", limitsWritable = !final && canEditKeyLimits(session, workspace, key);
  // Only a final key needs a sentence; otherwise the facts speak for themselves (expiry carries its own short note).
  const settingsCopy = final ? `This key is ${status} and can't be used or changed. Create a new key instead.` : undefined;
  const tab = keyTabs.some(x => x.value === search.tab) ? search.tab! : "overview";
  const setTab = (next: string) => nav?.navigate({ ...search, tab: next === "overview" ? undefined : next });
  const overview = <Stack gap={6}>
    <Card title="Settings" titleAs="h2" description={settingsCopy}>
      <DescriptionList dividers items={[
        { label: "Name", value: key.name },
        { label: "Key ID", value: <CopyId value={key.id} label="key ID" /> },
        { label: "Issued to", value: issuedTo(key, session, accounts.data, members.data) },
        { label: "Created", value: <DetailTime value={key.created_at} fallback="—" /> },
        { label: "Expires", value: <><DetailTime value={key.expires_at} fallback="—" />{!final && <span className={s.secondary}>Can't be changed. Rotate the key to set a new expiry.</span>}</> },
        { label: "Last used", value: key.last_used_at ? <Time value={key.last_used_at} format="relative" /> : key.last_used_at === null ? "Never" : "Unknown" },
        { label: "Models", value: <><span title={models.models.join(", ") || undefined}>{models.label}</span>{models.models.length > 0 && <span className={s.secondary}>{models.models.join(", ")}</span>}</> },
        ...(key.disabled_at && status === "disabled" ? [{ label: "Disabled", value: <DetailTime value={key.disabled_at} /> }] : []),
        ...(key.revoked_at ? [{ label: "Revoked", value: <DetailTime value={key.revoked_at} /> }] : []),
      ]} />
    </Card>
    <KeyUsage workspace={workspace} keyId={key.id} />
    {!final && (canDisable(session, workspace, key) || canEnable(session, workspace, key) || canRotate(session, workspace, key) || canRevoke(session, workspace, key)) && <DangerZone>
      {status === "disabled" ? <DangerAction title="Enable key" description="Requests using this key work again straight away." action={<Button variant="secondary" disabled={!canEnable(session, workspace, key)} onClick={() => ask(keyActions.enable(workspace.id, key))}>Enable key</Button>} disabledReason={canEnable(session, workspace, key) ? undefined : "Only the key's holder or a workspace admin can enable it."} />
        : <DangerAction title="Disable key" description="Requests using this key fail until it's enabled again. Limits and usage are kept." action={<Button variant="secondary" disabled={!canDisable(session, workspace, key)} onClick={() => ask(keyActions.disable(workspace.id, key))}>Disable key</Button>} />}
      {status === "active" && <DangerAction title="Rotate key" description="A new secret replaces this one, and the old secret stops working immediately. Models, limits and usage carry over." action={<Button variant="secondary" disabled={!canRotate(session, workspace, key)} onClick={() => ask(keyActions.rotate(workspace.id, key))}>Rotate key</Button>} disabledReason={canRotate(session, workspace, key) ? undefined : "Only the key's holder (or a workspace admin, for service-account keys) can rotate it."} />}
      <DangerAction title="Revoke key" description="Requests using this key fail immediately. This can't be undone." action={<Button variant="danger" disabled={!canRevoke(session, workspace, key)} onClick={() => ask(keyActions.revoke(workspace.id, key))}>Revoke key</Button>} />
    </DangerZone>}
  </Stack>;
  return <Stack gap={6} className={s.page}>
    {header(key.name, <StatusPill {...keyPill(status)} size="md" explain />, <Button variant="secondary" render={<ResourceLink search={{ page: "requests", ws: workspace.id, key_id: key.id }} />}>View requests</Button>)}
    <TypeTabs label="Key sections" items={keyTabs} value={tab} onChange={setTab}>
      {tab === "limits" ? <Stack gap={6}>
        <ScopeLimits mode="key" path={`${wsPath(workspace.id)}/keys/${encodeURIComponent(key.id)}/policy`} writable={limitsWritable} kind={workspace.kind} scopeLabel="This key" readOnlyReason={final ? `This key is ${status}, so its limits no longer change.` : "Only the key's holder or a workspace admin can change these limits."} />
        <KeyUsage workspace={workspace} keyId={key.id} part="budgets" />
      </Stack> : tab === "access" ? <EffectiveAccess workspace={workspace} keyId={key.id} keyName={key.name} keyStatus={status} canManageModels={p.manageGrants} /> : overview}
    </TypeTabs>
  </Stack>;
}

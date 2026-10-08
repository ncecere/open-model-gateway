import { useState } from "react";
import { Activity, Coins, Cpu, FolderKanban, UserRound, UsersRound } from "lucide-react";
import { platformPath, type PlatformOverviewData, type Session } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { formatMicroUsd } from "../lib/governance";
import { formatCount } from "../lib/reports";
import { setupSteps, type SetupStepId } from "../lib/model-setup";
import { ResourceLink } from "../components/navigation-link";
import { Checklist } from "../components/templates/checklist";
import { Button, ErrorNotice, Heading, Stack, StatCard, useApi } from "../components/ui";
import { InstallationBudgets } from "./usage/overview";
import { periodLabel, usagePeriod, usageQuery, type UsageOverview } from "../lib/usage";
import s from "./shared.module.css";

// Admin overview (Grounded pages/admin/overview: setup checklist, then "Platform at a glance").
// Every number comes from GET /platform/overview; nothing is estimated or defaulted to zero.
const DISMISS_KEY = "omg.enterprise.setupDismissed";
const stepActions: Record<SetupStepId, { label: string; search: DashboardSearch }> = {
  connection: { label: "Add connection", search: { page: "providers" } },
  model: { label: "Add model", search: { page: "model-new" } },
  pricing: { label: "Price routes", search: { page: "pricing", status: "unknown" } },
  offer: { label: "Catalogs", search: { page: "catalogs" } },
  defaults: { label: "Catalog defaults", search: { page: "catalogs", tab: "team" } },
  access: { label: "SSO groups", search: { page: "oidc" } },
  budget: { label: "Set a budget", search: { page: "policies" } },
};
function readDismissed(user: string) { try { return localStorage.getItem(`${DISMISS_KEY}:${user}`) === "1"; } catch { return false; } }

export function PlatformOverview({ session }: { session: Session }) {
  const overview = useApi<PlatformOverviewData>(`${platformPath}/overview`, session.capabilities.platform_read);
  const [dismissed, setDismissed] = useState(() => readDismissed(session.user.id));
  if (!session.capabilities.platform_read) return <Heading title="Access not available" />;
  const writable = session.capabilities.platform_write;
  const steps = overview.data ? setupSteps(overview.data.setup, overview.data.installation_budgets ?? undefined) : [];
  const required = steps.filter(step => !step.optional), allDone = required.length > 0 && required.every(step => step.done);
  const next = steps.find(step => !step.done && !step.optional) ?? (allDone ? undefined : steps.find(step => !step.done));
  const remember = (value: boolean) => { try { if (value) localStorage.setItem(`${DISMISS_KEY}:${session.user.id}`, "1"); else localStorage.removeItem(`${DISMISS_KEY}:${session.user.id}`); } catch { /* Storage is optional. */ } setDismissed(value); };
  // The next setup step is the page's one primary action; checklist rows keep secondary buttons.
  const primary = writable && next ? <Button render={<ResourceLink search={stepActions[next.id].search} />}>{stepActions[next.id].label}</Button> : undefined;
  return <Stack gap={6} className={s.page}>
    <Heading title="Overview" description={writable ? "What this install still needs, and how the platform is used." : "Setup progress and platform totals. Your role is read-only."} actions={primary} />
    {overview.isPending ? <p role="status">Loading platform overview…</p> : overview.isError ? <ErrorNotice error={overview.error} retry={() => void overview.refetch()} /> : <>
      {dismissed && !allDone ? <p className={s.note}>Setup: {required.filter(step => step.done).length} of {required.length} required steps done. <Button size="sm" variant="ghost" onClick={() => remember(false)}>Show setup steps</Button></p>
        : <Checklist title={allDone ? "Setup complete" : "Set up this install"} description={allDone ? "Everything the gateway needs is in place. Steps are checked against the current configuration." : "Each step is checked against the current configuration. Nothing here calls a provider."} onDismiss={allDone ? undefined : () => remember(true)} actionVariant="secondary"
          steps={steps.map(step => ({ ...step, action: writable ? { label: stepActions[step.id].label, render: <ResourceLink search={stepActions[step.id].search} /> } : undefined }))} />}
      <Glance data={overview.data} />
      <InstallationBudgets budgets={overview.data.installation_budgets} />
    </>}
  </Stack>;
}

/**
 * Spend and requests use the same period as Usage & costs (this month, UTC), from the platform usage overview;
 * while it loads or if it fails they read "…" / "—", never zero (review #25, rule 7).
 */
function Glance({ data }: { data: PlatformOverviewData }) {
  const g = data.glance, count = (n: number) => formatCount(n), month = usagePeriod({});
  const usage = useApi<UsageOverview>(`${platformPath}/usage/overview?${usageQuery(month)}`), tiles = usage.data?.tiles;
  const monthly = (value: string | null | undefined, format: (v: string) => string) => usage.isPending ? "…" : usage.isError || value == null ? "—" : format(value);
  return <section aria-labelledby="glance-title"><Stack gap={3}><h2 id="glance-title" className={s.settingTitle}>Platform at a glance</h2><div className={s.stats3}>
    <StatCard label="Users with access" value={count(g.entitled_users)} icon={<UserRound />} hint={`${count(data.setup.oidc_mappings)} SSO group mapping${data.setup.oidc_mappings === 1 ? "" : "s"}`} render={<ResourceLink search={{ page: "users" }} />} />
    <StatCard label="Teams" value={count(g.teams)} icon={<UsersRound />} render={<ResourceLink search={{ page: "platform-teams" }} />} />
    <StatCard label="Projects" value={count(g.projects)} icon={<FolderKanban />} render={<ResourceLink search={{ page: "platform-projects" }} />} />
    <StatCard label="Ready models" value={count(g.ready_models)} icon={<Cpu />} hint={`of ${count(data.setup.models)} model${data.setup.models === 1 ? "" : "s"}`} render={<ResourceLink search={{ page: "models" }} />} />
    <StatCard label="Requests (incl. retries), this month" value={monthly(tiles?.requests.attempts, formatCount)} icon={<Activity />} hint={usage.isError ? "Couldn't load usage. Open Usage & costs to try again." : `Every call to a provider, including fallback attempts · ${periodLabel(month)}`} render={<ResourceLink search={{ page: "platform-costs" }} />} />
    <StatCard label="Spend, this month" value={monthly(tiles?.spend.value, formatMicroUsd)} icon={<Coins />} hint={usage.isError ? "Couldn't load usage. Open Usage & costs to try again." : "Final costs, estimated from configured prices. Amounts on hold aren't included."} render={<ResourceLink search={{ page: "platform-costs" }} />} />
  </div></Stack></section>;
}

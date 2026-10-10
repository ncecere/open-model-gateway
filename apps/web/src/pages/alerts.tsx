/*
 * Alerts (docs/alerts.md): Admin › Settings › Alerts (installation rules and
 * history), the workspace Settings › Alerts tab (Team/Project rules; personal
 * built-in alerts), the routed rule create/edit page, and Notifications.
 * Rules are rows; the rule form keeps Cancel + Save in the page header.
 * Authority is the server's: these pages only hide what a role can't do.
 */
import { useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { BellRing, CheckCheck, History, Plus } from "lucide-react";
import { api, platformPath, type Provider, type Session, type Workspace } from "../lib/api";
import { conditionText, draftOf, emailLabels, eventsPath, kindHints, kindLabels, kindsFor, layerLabels, layersFor, newDraft, notificationsPath, recipientsText, resolutionLabels, ruleBody, sourceText, ruleErrors, rulePath, rulesPath, spendPeriodLabels, unknownCostNote, whereText, type AlertEvent, type AlertKind, type AlertRule, type AlertScope, type BudgetLayer, type Notification, type RuleDraft, type RuleList, type SpendPeriod } from "../lib/alerts";
import { permissions, type DashboardSearch } from "../lib/permissions";
import { Button, ErrorNotice, FormField, Heading, Input, NativeSelect, Stack, Textarea, useAction, useApi, useChoices } from "../components/ui";
import { ResourcePage } from "../components/resource-page";
import { DirectoryTable, LocalTable } from "../components/people";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { StatusBadge } from "../components/ui/badge/badge";
import { TableTime } from "../components/templates/when";
import { BackLink, FormPage, FormSection, Wide } from "../components/templates/form-page";
import { Checkbox, CheckboxGroup } from "../components/ui/checkbox/checkbox";
import { Switch } from "../components/ui/switch/switch";
import { Card } from "../components/ui/card/card";
import { DescriptionList } from "../components/ui/description-list/description-list";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { useResourceName, useResourceParent } from "../components/layout/breadcrumbs";
import { toast } from "../components/ui/toast/toast";
import s from "./shared.module.css";

const recordSearch = (scope: AlertScope, id: string): DashboardSearch => scope.kind === "platform" ? { page: "alert-rule-detail", record: id } : { page: "workspace-alert-detail", ws: scope.ws, record: id };
const listSearch = (scope: AlertScope): DashboardSearch => scope.kind === "platform" ? { page: "settings-alerts" } : { page: "workspace-settings", ws: scope.ws, tab: "alerts" };

function RuleStatus({ rule }: { rule: AlertRule }) {
  if (rule.firing > 0) return <StatusBadge tone="danger" title="Open alerts">Firing · {rule.firing}</StatusBadge>;
  return rule.enabled ? <StatusBadge tone="success">On</StatusBadge> : <StatusBadge tone="neutral" variant="outline">Off</StatusBadge>;
}
function EventState({ event }: { event: AlertEvent }) {
  if (event.state === "firing") return <StatusBadge tone={event.severity === "critical" ? "danger" : "warning"}>Firing</StatusBadge>;
  return <StatusBadge tone="neutral" title={event.resolution ? resolutionLabels[event.resolution] : undefined}>Resolved</StatusBadge>;
}
const eventSecondary = (e: AlertEvent) => [sourceText(e), unknownCostNote(e) ? "Some cost unknown" : ""].filter(Boolean).join(" · ");

export function NewRuleButton({ scope }: { scope: AlertScope }) {
  return <Button render={<ResourceLink search={recordSearch(scope, "new")} />}><Plus aria-hidden /> New rule</Button>;
}

/** Compact rules list: search + columns in one toolbar row. */
export function AlertRules({ scope, writable }: { scope: AlertScope; writable: boolean }) {
  const q = useApi<RuleList>(rulesPath(scope)), ask = useAction();
  const columns: DataTableColumn<AlertRule>[] = [
    { id: "name", header: "Rule", accessor: r => r.name, rowHeader: true, hideable: false, cell: r => <CellText primary={<ResourceLink search={recordSearch(scope, r.id)}>{r.name}</ResourceLink>} secondary={conditionText(r)} /> },
    { id: "kind", header: "Type", accessor: r => kindLabels[r.kind], cell: r => kindLabels[r.kind] },
    { id: "email", header: "Email", accessor: r => recipientsText(r), cell: r => recipientsText(r) },
    { id: "status", header: "Status", accessor: r => r.firing > 0 ? "firing" : r.enabled ? "on" : "off", cell: r => <RuleStatus rule={r} /> },
    { id: "last", header: "Last fired", accessor: r => r.last_fired_at ?? "", cell: r => <TableTime value={r.last_fired_at} fallback="Never" /> },
  ];
  return <LocalTable<AlertRule> rows={q.data?.data} loading={q.isFetching} error={q.error} retry={() => void q.refetch()} label="Alert rules" storageKey={`alert-rules-${scope.kind}`} search={{ placeholder: "Rule name" }} rowKey={r => r.id} rowLabel={r => r.name} columns={columns}
    rowActions={r => [
      { label: writable ? "Edit" : "View", render: <ResourceLink search={recordSearch(scope, r.id)} /> },
      { label: r.enabled ? "Turn off…" : "Turn on", hidden: !writable, onSelect: () => ask({ title: r.enabled ? `Turn off ${r.name}?` : `Turn on ${r.name}?`, description: r.enabled ? "Open alerts close without email." : undefined, submitLabel: r.enabled ? "Turn off" : "Turn on", successNotice: r.enabled ? "Rule turned off" : "Rule turned on", run: (_, signal) => api(rulePath(scope, r.id), { method: "PUT", body: { ...ruleBody(draftOf(r), scope), enabled: !r.enabled }, signal }) }) },
      { label: "Delete rule…", danger: true, hidden: !writable, onSelect: () => ask({ title: `Delete ${r.name}?`, description: "Its history stays. Open alerts close without email.", danger: true, submitLabel: "Delete rule", successNotice: "Rule deleted", run: (_, signal) => api(rulePath(scope, r.id), { method: "DELETE", signal }) }) },
    ]}
    empty={{ icon: <BellRing />, title: "No alert rules yet.", description: scope.kind === "platform" ? "Get told when budgets fill up, spend jumps or connections fail." : "Get told when this workspace's budgets fill up or spend jumps.", action: writable ? <NewRuleButton scope={scope} /> : undefined }} />;
}

/** Incidents of one scope, newest first (server-paged). */
export function AlertHistory({ scope }: { scope: AlertScope }) {
  const columns: DataTableColumn<AlertEvent>[] = [
    { id: "alert", header: "Alert", accessor: e => e.summary, rowHeader: true, hideable: false, cell: e => <CellText primary={e.summary} secondary={eventSecondary(e)} title={unknownCostNote(e)} /> },
    { id: "where", header: "Where", accessor: e => whereText(e), cell: e => whereText(e) },
    { id: "state", header: "State", accessor: e => e.state, cell: e => <EventState event={e} /> },
    { id: "fired", header: "Fired", accessor: e => e.fired_at, cell: e => <TableTime value={e.fired_at} /> },
    { id: "resolved", header: "Resolved", accessor: e => e.resolved_at ?? "", cell: e => <TableTime value={e.resolved_at} /> },
    { id: "email", header: "Email", accessor: e => e.email?.status ?? "", cell: e => e.email ? <span title={e.email.error ?? undefined}>{emailLabels[e.email.status]}</span> : "—" },
  ];
  return <DirectoryTable<AlertEvent> path={eventsPath(scope)} label="Alert history" storageKey={`alert-history-${scope.kind}`} columns={columns} rowKey={e => e.id} rowLabel={e => e.summary}
    facets={[{ id: "status", label: "State", options: [{ value: "firing", label: "Firing" }, { value: "resolved", label: "Resolved" }] }]}
    empty={{ icon: <History />, title: "No alerts yet.", description: "Alerts show up here when a rule fires." }} />;
}

/** Admin › Settings › Alerts. */
export function AdminAlertsPage({ session, tab, onTabChange }: { session: Session; tab?: string; onTabChange: (tab: string) => void }) {
  const scope: AlertScope = { kind: "platform" }, writable = session.capabilities.platform_write;
  return <ResourcePage title="Alerts" description="Budget, spend, error and connection alerts for the installation." actions={writable && <NewRuleButton scope={scope} />} tab={tab} onTabChange={onTabChange} tabs={[
    { value: "rules", label: "Rules", icon: <BellRing aria-hidden />, content: <AlertRules scope={scope} writable={writable} /> },
    { value: "history", label: "History", icon: <History aria-hidden />, content: <AlertHistory scope={scope} /> },
  ]} />;
}

/** Workspace Settings › Alerts: Team/Project rules for admins; Personal has built-in alerts only. */
export function WorkspaceAlerts({ session, workspace }: { session: Session; workspace: Workspace }) {
  const scope: AlertScope = { kind: "workspace", ws: workspace.id };
  if (workspace.kind === "personal") return <Stack gap={6}>
    <Card title="Built-in budget alerts" description="At 80% and 100% of any budget on this workspace or its keys. In-app, and by email when email is set up. Only you see them." />
    <AlertHistory scope={scope} />
  </Stack>;
  const writable = permissions(session, workspace).managePolicy;
  return <Stack gap={6}>
    <Heading title="Rules" actions={writable && <NewRuleButton scope={scope} />} />
    <AlertRules scope={scope} writable={writable} />
    <Heading title="History" />
    <AlertHistory scope={scope} />
  </Stack>;
}

/** Routed create/edit page; read-only facts for Auditors. `id` "new" creates. */
export function AlertRulePage({ session, scope, workspace, id }: { session: Session; scope: AlertScope; workspace?: Workspace; id: string }) {
  const creating = id === "new", nav = useDashboardNavigation(), client = useQueryClient(), leaving = useRef(false);
  const writable = scope.kind === "platform" ? session.capabilities.platform_write : !!workspace && permissions(session, workspace).managePolicy;
  const q = useApi<AlertRule>(rulePath(scope, id), !creating);
  const connections = useChoices<Provider>(`${platformPath}/providers`, scope.kind === "platform" && writable);
  const [edits, setEdits] = useState<RuleDraft>(), [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const saved = creating ? newDraft(scope) : q.data ? draftOf(q.data) : undefined, form = edits ?? saved;
  const back = { label: "Alerts", search: listSearch(scope) };
  useResourceParent(scope.kind === "workspace" ? { label: "Alerts", to: back.search } : undefined);
  const leave = () => { leaving.current = true; nav?.navigate(back.search); };
  if (!creating && q.isError) return <Stack gap={6} className={s.page}><BackLink {...back} /><ErrorNotice error={q.error} retry={() => void q.refetch()} /></Stack>;
  if (!writable) return <ReadOnlyRule rule={q.data} back={back} />;
  const errors = form ? ruleErrors(form) : {}, invalid = Object.keys(errors).length > 0, shown = submitted ? errors : {};
  const dirty = !!form && !!saved && JSON.stringify(form) !== JSON.stringify(saved);
  const set = <K extends keyof RuleDraft>(k: K, v: RuleDraft[K]) => form && setEdits({ ...form, [k]: v });
  const changeKind = (kind: AlertKind) => form && setEdits({ ...newDraft(scope, kind), name: form.name, enabled: form.enabled, notifyWorkspaceAdmins: form.notifyWorkspaceAdmins, notifyPlatformAdmins: form.notifyPlatformAdmins, emails: form.emails });
  async function save() {
    setSubmitted(true); if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try {
      await api(creating ? rulesPath(scope) : rulePath(scope, id), { method: creating ? "POST" : "PUT", body: ruleBody(form, scope) });
      await client.invalidateQueries({ queryKey: ["api"] });
      toast.success(creating ? "Alert rule created" : "Alert rule saved"); leave();
    } catch (caught) { setError(caught); } finally { setBusy(false); }
  }
  const field = (k: keyof RuleDraft, label: string, props: { hint?: string; description?: string; placeholder?: string; suffix?: string; optional?: boolean; inputMode?: "decimal" | "numeric" } = {}) =>
    <FormField label={label} labelHint={props.hint ?? (props.optional ? "Optional" : undefined)} description={props.description} error={shown[k]}>
      <Input value={String(form?.[k] ?? "")} inputMode={props.inputMode} placeholder={props.placeholder} maxLength={k === "name" ? 120 : 32} autoComplete="off" disabled={busy} onChange={ev => set(k, ev.target.value as never)} />
    </FormField>;
  const title = creating ? "New alert rule" : "Edit alert rule";
  return <FormPage label={creating ? "New alert rule" : q.data?.name ?? "Alert rule"} title={title} back={back} onCancel={leave} onSubmit={() => void save()} submitLabel={creating ? "Create rule" : "Save rule"} busy={busy} dirty={dirty} allowLeave={() => leaving.current} error={error} loading={!form ? "Loading rule…" : undefined}>
    {form && <>
      <FormSection title="Rule">
        {field("name", "Name", { placeholder: "Monthly budgets" })}
        <FormField label="Type" description={kindHints[form.kind]}>{creating
          ? <NativeSelect value={form.kind} disabled={busy} onChange={ev => changeKind(ev.target.value as AlertKind)}>{kindsFor(scope).map(k => <option key={k} value={k}>{kindLabels[k]}</option>)}</NativeSelect>
          : <Input value={kindLabels[form.kind]} disabled readOnly />}</FormField>
        <Wide><Switch label="On" checked={form.enabled} disabled={busy} onCheckedChange={checked => set("enabled", checked)} /></Wide>
      </FormSection>
      <FormSection title="When">
        {form.kind === "budget_threshold" && <>
          <Wide><CheckboxGroup legend="Budgets" error={shown.layers} orientation="horizontal" value={form.layers} disabled={busy} onValueChange={next => set("layers", next as BudgetLayer[])}>{layersFor(scope).map(l => <Checkbox key={l} value={l} label={layerLabels[l]} />)}</CheckboxGroup></Wide>
          {field("thresholds", "Alert at (%)", { description: "Up to five, such as 50, 80, 100.", inputMode: "numeric" })}
        </>}
        {form.kind === "spend_threshold" && <>
          <FormField label="Period"><NativeSelect value={form.spendPeriod} disabled={busy} onChange={ev => set("spendPeriod", ev.target.value as SpendPeriod)}>{(Object.keys(spendPeriodLabels) as SpendPeriod[]).map(p => <option key={p} value={p}>{spendPeriodLabels[p]}{p === "lifetime" ? "" : " (UTC)"}</option>)}</NativeSelect></FormField>
          {field("spendAmount", "Amount (USD)", { description: "Spent plus on hold, all workspaces.", inputMode: "decimal" })}
          {field("thresholds", "Alert at (%)", { description: "Up to five, such as 80, 100.", inputMode: "numeric" })}
        </>}
        {form.kind === "spend_spike" && <>
          {field("factor", "Last hour vs. 7-day hourly average", { description: "A multiple, such as 3 (3×).", inputMode: "decimal" })}
          {field("minSpend", "Only above (USD)", { description: "Ignores spikes on small amounts.", inputMode: "decimal" })}
        </>}
        {form.kind === "provider_failing" && <>
          <FormField label="Connection"><NativeSelect value={form.connection} disabled={busy} onChange={ev => set("connection", ev.target.value)}><option value="">Any enabled connection</option>{connections.data?.map(c => <option key={c.id} value={c.id}>{c.name}</option>)}</NativeSelect></FormField>
          {field("consecutive", "Failures in a row", { optional: true, inputMode: "numeric" })}
        </>}
        {form.kind === "batch_stalled" && field("window", "No progress for (minutes)", { inputMode: "numeric" })}
        {form.kind === "batch_failed" && <p className={s.note}>Fires once per failed or expired batch{scope.kind === "platform" ? " in teams and projects" : ""}.</p>}
        {(form.kind === "error_rate" || form.kind === "provider_failing") && <>
          {field("window", "Window (minutes)", { inputMode: "numeric" })}
          {field("rate", "Failed requests (%)", { optional: form.kind === "provider_failing", inputMode: "numeric" })}
          {field("minRequests", "At least (requests)", { optional: form.kind === "provider_failing", inputMode: "numeric", description: "Too few requests never alert." })}
        </>}
      </FormSection>
      <FormSection title="Email" description={scope.kind === "platform" ? "Platform admins and auditors always see it in Notifications." : "Workspace admins always see it in Notifications."}>
        <Wide><Stack gap={2}>
          {scope.kind === "workspace" && <Checkbox label="Workspace admins" checked={form.notifyWorkspaceAdmins} disabled={busy} onCheckedChange={checked => set("notifyWorkspaceAdmins", checked === true)} />}
          <Checkbox label="Platform admins" checked={form.notifyPlatformAdmins} disabled={busy} onCheckedChange={checked => set("notifyPlatformAdmins", checked === true)} />
        </Stack></Wide>
        <Wide><FormField label="Other addresses" labelHint="Optional" error={shown.emails}><Textarea rows={2} value={form.emails} placeholder="finance@example.com" disabled={busy} onChange={ev => set("emails", ev.target.value)} /></FormField></Wide>
      </FormSection>
    </>}
  </FormPage>;
}

function ReadOnlyRule({ rule, back }: { rule?: AlertRule; back: { label: string; search: DashboardSearch } }) {
  useResourceName(rule?.name ?? "Alert rule");
  return <Stack gap={6} className={s.page}>
    <BackLink {...back} />
    <Heading title={rule?.name ?? "Alert rule"} description={rule ? kindLabels[rule.kind] : undefined} />
    {rule ? <Card><DescriptionList dividers items={[
      { label: "When", value: conditionText(rule) },
      { label: "Status", value: <RuleStatus rule={rule} /> },
      { label: "Email", value: [recipientsText(rule), ...rule.notify_emails].join(" · ") },
      { label: "Last fired", value: <TableTime value={rule.last_fired_at} fallback="Never" /> },
    ]} /></Card> : <p role="status">Loading rule…</p>}
  </Stack>;
}

/** Notifications: the caller's alerts, newest first, with per-user read state. */
export function NotificationsPage({ session }: { session: Session }) {
  const client = useQueryClient(), [busy, setBusy] = useState(false);
  async function markRead(body: { ids: string[] } | { all: true }) {
    setBusy(true);
    try { await api(`${notificationsPath}/read`, { method: "POST", body }); await client.invalidateQueries({ queryKey: ["api"] }); }
    catch { toast.error("Couldn't mark as read. Try again."); } finally { setBusy(false); }
  }
  const ruleLink = (n: Notification): DashboardSearch | undefined => {
    if (n.builtin && n.workspace) return { page: "workspace-settings", ws: n.workspace.id, tab: "alerts" };
    if (n.rule?.deleted) return;
    if (n.rule?.scope === "installation" && session.capabilities.platform_read) return { page: "alert-rule-detail", record: n.rule.id };
    const ws = n.workspace && session.workspaces.find(w => w.id === n.workspace!.id);
    return n.rule && ws && permissions(session, ws).managePolicy ? { page: "workspace-alert-detail", ws: ws.id, record: n.rule.id } : undefined;
  };
  const columns: DataTableColumn<Notification>[] = [
    { id: "alert", header: "Alert", accessor: n => n.summary, rowHeader: true, hideable: false, cell: n => <CellText primary={n.read ? n.summary : <strong>{n.summary}</strong>} secondary={eventSecondary(n)} title={unknownCostNote(n)} /> },
    { id: "where", header: "Where", accessor: n => whereText(n), cell: n => whereText(n) },
    { id: "state", header: "State", accessor: n => n.state, cell: n => <EventState event={n} /> },
    { id: "when", header: "When", accessor: n => n.fired_at, cell: n => <TableTime value={n.fired_at} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Notifications" description="Alerts for what you look after." actions={<Button variant="secondary" loading={busy} onClick={() => void markRead({ all: true })}><CheckCheck aria-hidden /> Mark all read</Button>} />
    <DirectoryTable<Notification> path={notificationsPath} label="Notifications" storageKey="notifications" columns={columns} rowKey={n => n.id} rowLabel={n => n.summary}
      facets={[{ id: "status", label: "Show", allLabel: "All", options: [{ value: "unread", label: "Unread" }] }]}
      rowActions={n => { const link = ruleLink(n); return [
        { label: "Mark as read", hidden: n.read, onSelect: () => void markRead({ ids: [n.id] }) },
        { label: "Open rule", hidden: !link, render: link ? <ResourceLink search={link} /> : undefined },
      ]; }}
      empty={{ icon: <BellRing />, title: "No notifications.", description: "Alerts for your budgets and workspaces show up here." }} />
  </Stack>;
}

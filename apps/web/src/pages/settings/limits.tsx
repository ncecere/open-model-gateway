/*
 * Admin › Settings › Defaults & limits (formerly Admin › Limits; /admin/limits
 * redirects here): Grounded/ResourcePage pill tabs, one scope per tab (URL
 * `?tab=installation|personal|team|project`): the installation ceiling and the
 * live Personal/Team/Project type defaults. Each tab shows that scope's limits
 * table (components/scope-limits.tsx): three rate rows and stacked budgets
 * (one per period: daily, weekly, monthly, lifetime; each enforced over its
 * own window, with its reset rule in plain words). Each tab saves or discards
 * on its own (PUT replaces that scope's rates and full budget set); switching
 * tabs with unsaved edits asks first, and leaving the page is guarded too.
 * Blank means no limit at that scope. Money is exact integer micro-USD via
 * BigInt. The installation ceiling is an additional shared limit, not a default.
 *
 * Provenance: layout adapted from Grounded web/src/pages/admin/limits/
 * {platform,fields}.tsx (read-only reference).
 */
import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, platformPath, type Session } from "../../lib/api";
import type { Policy } from "../../lib/governance";
import { draftErrors, draftLimits, draftOf, hasErrors, limitsBody, limitsOf, limitsSaveError, sameDraft, type LimitsDraft } from "../../lib/limits";
import { Button, ErrorNotice, Stack, useApi } from "../../components/ui";
import { DiscardChangesDialog, NavigationGuard } from "../../components/navigation-guard";
import { ResourcePage } from "../../components/resource-page";
import { LimitsTable } from "../../components/scope-limits";
import { Card } from "../../components/ui/card/card";
import { StickySaveBar } from "../../components/templates/sticky-save-bar";
import { toast } from "../../components/ui/toast/toast";
import s from "../shared.module.css";

export const limitScopes = [
  { id: "installation", label: "Installation ceiling", description: "Shared by all workspaces together, on top of everything else.", path: `${platformPath}/installation/policy` },
  { id: "personal", label: "Personal default", description: "Each personal workspace, unless overridden.", path: `${platformPath}/workspace-types/personal/policy` },
  { id: "team", label: "Team default", description: "Each team, unless overridden.", path: `${platformPath}/workspace-types/team/policy` },
  { id: "project", label: "Project default", description: "Each project, unless overridden.", path: `${platformPath}/workspace-types/project/policy` },
] as const;
export type LimitScopeId = typeof limitScopes[number]["id"];
/** The tab in the URL, or the first (installation) for anything else. */
export const limitScopeOf = (tab: string | undefined): LimitScopeId => limitScopes.find(scope => scope.id === tab)?.id ?? "installation";

export function PlatformLimits({ session, tab, onTabChange }: { session: Session; /** `?tab=` (routed); local state otherwise. */ tab?: string; onTabChange?: (tab: string) => void }) {
  const [localTab, setLocalTab] = useState<string>();
  const current = limitScopeOf(onTabChange ? tab : localTab);
  const go = (next: string) => onTabChange ? onTabChange(next) : setLocalTab(next);
  // Dirty guard on tab switch: a tab with unsaved edits asks before switching (the edits are discarded on confirm).
  const [dirty, setDirty] = useState(false), [pending, setPending] = useState<string>();
  const switching = useRef(false);
  useEffect(() => { switching.current = false; }, [current]);
  const changeTab = (next: string) => { if (next === current) return; if (dirty) { setPending(next); return; } go(next); };
  const confirmSwitch = () => { if (!pending) return; switching.current = true; setDirty(false); setPending(undefined); go(pending); };
  return <>
    <ResourcePage title="Defaults & limits" description="Every limit that applies is enforced; the lowest wins."
      tab={current} onTabChange={changeTab}
      tabs={limitScopes.map(scope => ({ value: scope.id, label: scope.label, content: <ScopeTab key={scope.id} scope={scope} writable={session.capabilities.platform_write} onDirtyChange={setDirty} /> }))} />
    <DiscardChangesDialog open={pending !== undefined} onOpenChange={open => { if (!open) setPending(undefined); }}
      description={`Your changes to ${limitScopes.find(scope => scope.id === current)!.label} haven't been saved.`} onDiscard={confirmSwitch} />
    {/* Leaving the page (not a confirmed tab switch) is guarded by the router. */}
    {session.capabilities.platform_write && <NavigationGuard dirty={dirty} allow={() => switching.current} />}
  </>;
}

/** One scope: its limits table (rates + stacked budgets) with its own save bar. */
function ScopeTab({ scope, writable, onDirtyChange }: { scope: typeof limitScopes[number]; writable: boolean; onDirtyChange: (dirty: boolean) => void }) {
  const client = useQueryClient(), q = useApi<{ policy: Policy }>(scope.path);
  const saved = q.isSuccess ? draftOf(limitsOf(q.data.policy)) : undefined;
  // Edits overlay the saved values; none means the table shows what the server has.
  const [edits, setEdits] = useState<LimitsDraft>(), form = edits ?? saved;
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const changed = !!form && !!saved && !sameDraft(form, saved);
  const errors = form ? draftErrors(form, "free") : undefined, invalid = !!errors && hasErrors(errors);
  useEffect(() => { onDirtyChange(changed && !busy); }, [changed, busy]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => () => onDirtyChange(false), []); // eslint-disable-line react-hooks/exhaustive-deps
  async function save() {
    if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try {
      await api(scope.path, { method: "PUT", body: limitsBody(draftLimits(form)) });
      await client.invalidateQueries({ queryKey: ["api"] });
      setEdits(undefined);
      toast.success("Limits saved", scope.label);
    } catch (caught) { setError(limitsSaveError(caught)); void client.invalidateQueries({ queryKey: ["api"] }); }
    finally { setBusy(false); }
  }
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (!form || !errors) return <p role="status">Loading limits…</p>;
  return <Stack gap={6}>
    {error !== undefined && <ErrorNotice error={error} />}
    <Card title={scope.label} description={scope.description} flush>
      <LimitsTable caption={`${scope.label} limits`} scopeLabel={scope.label} draft={form} onChange={setEdits} editing={writable} busy={busy} errors={errors} emptyText="No limit" placeholder={() => "No limit"} />
    </Card>
    <p className={s.note}>Workspace overrides and workspace caps are on each team's and project's page. Raising a limit or changing a budget never resets spending.</p>
    {writable && <StickySaveBar open={changed} message={invalid ? "Not saved: fix the highlighted limits" : `Unsaved changes: ${scope.label}`}><Button variant="secondary" disabled={busy} onClick={() => { setEdits(undefined); setError(undefined); }}>Discard</Button><Button loading={busy} disabled={invalid} onClick={() => void save()}>Save limits</Button></StickySaveBar>}
  </Stack>;
}

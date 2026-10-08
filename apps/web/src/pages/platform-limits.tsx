/*
 * Admin › Limits: the installation ceiling and the live Personal/Team/Project
 * type defaults, one card each, with the same limits table as workspaces and
 * keys (components/scope-limits.tsx): three rate rows and stacked budgets
 * (one per period: daily, weekly, monthly, lifetime; each enforced over its
 * own window, with its reset rule in plain words). Edits to any card are saved
 * together from one save bar; each scope is its own policy (PUT replaces its
 * rates and full budget set). Blank means no limit at that scope. Money is
 * exact integer micro-USD via BigInt. The installation ceiling is an additional
 * shared limit, not a default.
 *
 * Provenance: layout adapted from Grounded web/src/pages/admin/limits/
 * {platform,fields}.tsx (read-only reference).
 */
import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, platformPath, type Session } from "../lib/api";
import type { Policy } from "../lib/governance";
import { draftErrors, draftLimits, draftOf, hasErrors, limitsBody, limitsOf, limitsSaveError, sameDraft, type LimitsDraft } from "../lib/limits";
import { Button, ErrorNotice, Heading, Stack, useApi } from "../components/ui";
import { NavigationGuard } from "../components/navigation-guard";
import { LimitsTable } from "../components/scope-limits";
import { Card } from "../components/ui/card/card";
import { StickySaveBar } from "../components/templates/sticky-save-bar";
import { toast } from "../components/ui/toast/toast";
import s from "./shared.module.css";

export const limitScopes = [
  { id: "installation", label: "Installation ceiling", description: "An extra limit shared by every workspace together, on top of everything else. It isn't a default.", path: `${platformPath}/installation/policy` },
  { id: "personal", label: "Personal default", description: "Applies to each personal workspace on its own, unless a Platform Admin overrides it.", path: `${platformPath}/workspace-types/personal/policy` },
  { id: "team", label: "Team default", description: "Applies to each team on its own, unless the team has an override.", path: `${platformPath}/workspace-types/team/policy` },
  { id: "project", label: "Project default", description: "Applies to each project on its own, unless the project has an override.", path: `${platformPath}/workspace-types/project/policy` },
] as const;
type ScopeId = typeof limitScopes[number]["id"];
export type LimitsForm = Record<ScopeId, LimitsDraft>;

export function PlatformLimits({ session }: { session: Session }) {
  const client = useQueryClient(), writable = session.capabilities.platform_write;
  const queries = { installation: useApi<{ policy: Policy }>(limitScopes[0].path), personal: useApi<{ policy: Policy }>(limitScopes[1].path), team: useApi<{ policy: Policy }>(limitScopes[2].path), project: useApi<{ policy: Policy }>(limitScopes[3].path) };
  const loaded = limitScopes.every(scope => queries[scope.id].isSuccess);
  const saved = loaded ? Object.fromEntries(limitScopes.map(scope => [scope.id, draftOf(limitsOf(queries[scope.id].data!.policy))])) as LimitsForm : undefined;
  // Edits overlay the saved values; none means the cards show what the server has.
  const [edits, setEdits] = useState<LimitsForm>(), form = edits ?? saved;
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const failed = limitScopes.find(scope => queries[scope.id].isError);
  const changed = form && saved ? limitScopes.filter(scope => !sameDraft(form[scope.id], saved[scope.id])) : [];
  const errors = form ? Object.fromEntries(limitScopes.map(scope => [scope.id, draftErrors(form[scope.id], "free")])) as Record<ScopeId, ReturnType<typeof draftErrors>> : undefined;
  const invalid = !!errors && limitScopes.some(scope => hasErrors(errors[scope.id]));
  async function save() {
    if (!form || invalid || busy) return;
    setBusy(true); setError(undefined);
    try {
      // Each scope is an independent policy; saved scopes stay saved if a later one fails.
      for (const scope of changed) await api(scope.path, { method: "PUT", body: limitsBody(draftLimits(form[scope.id])) });
      await client.invalidateQueries({ queryKey: ["api"] });
      setEdits(undefined);
      toast.success("Limits saved", changed.map(scope => scope.label).join(", "));
    } catch (caught) { setError(limitsSaveError(caught)); void client.invalidateQueries({ queryKey: ["api"] }); }
    finally { setBusy(false); }
  }
  return <Stack gap={6} className={s.page}>
    <Heading title="Limits" description="Defaults apply to each workspace on its own, unless it has an override. The installation ceiling is shared by all of them. Every limit that applies is enforced: the lowest wins." />
    {failed ? <ErrorNotice error={queries[failed.id].error} retry={() => void queries[failed.id].refetch()} /> : !form || !errors ? <p role="status">Loading limits…</p> : <>
      {error !== undefined && <ErrorNotice error={error} />}
      {limitScopes.map(scope => <Card key={scope.id} title={scope.label} description={scope.description} flush>
        <LimitsTable caption={`${scope.label} limits`} scopeLabel={scope.label} draft={form[scope.id]} onChange={next => setEdits({ ...form, [scope.id]: next })} editing={writable} busy={busy} errors={errors[scope.id]} emptyText="No limit" placeholder={() => "No limit"} />
      </Card>)}
      <p className={s.note}>Workspace overrides and workspace caps are on each team's and project's page. Raising a limit or changing a budget never resets spending.</p>
      {writable && <><StickySaveBar open={changed.length > 0} message={invalid ? "Not saved: fix the highlighted limits" : `Unsaved changes: ${changed.map(scope => scope.label).join(", ")}`}><Button variant="secondary" disabled={busy} onClick={() => { setEdits(undefined); setError(undefined); }}>Discard</Button><Button loading={busy} disabled={invalid} onClick={() => void save()}>Save limits</Button></StickySaveBar><NavigationGuard dirty={changed.length > 0 && !busy} /></>}
    </>}
  </Stack>;
}

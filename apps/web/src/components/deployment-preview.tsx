import { useId, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, platformPath, type DeploymentApplyResult, type DeploymentPreview } from "../lib/api";
import { Button, DateTime, ErrorNotice, Stack, useAction, useApiScope } from "./ui";
import { Dialog } from "./ui/dialog/dialog";
import { DescriptionList } from "./ui/description-list/description-list";
import { Alert } from "./ui/alert/alert";
import { ActionMenu } from "./templates/action-menu";
import { HeaderActions } from "./templates/header-actions";
import s from "./deployment-preview.module.css";

type Record = { id: string; enabled: boolean };
export function DeploymentReviewButton({ record, name }: { record: Record; name: string }) {
  const [reviewing, setReviewing] = useState(false), trigger = useRef<HTMLButtonElement>(null), triggerId = useId();
  return <><Button id={triggerId} ref={trigger} variant="secondary" onClick={() => setReviewing(true)}>Review changes</Button>{reviewing && <DeploymentReviewDialog record={record} name={name} returnFocus={trigger.current ?? undefined} fallbackFocus={() => document.getElementById(triggerId)} onClose={() => setReviewing(false)} />}</>;
}
/** Retire is also disablement, preserving its existing DELETE and audit semantics. */
export function DeploymentStatusActions({ record, name, children }: { record: Record; name: string; children?: (actions: ReactNode) => ReactNode }) {
  const [review, setReview] = useState<{ retire: boolean; returnFocus: HTMLElement } | null>(null), triggerId = useId();
  // Pass plain buttons/menu items to the containing header, not an opaque
  // component: its narrow layout must merge these with any sibling actions.
  const actions = <><Button id={triggerId} variant="secondary" onClick={event => setReview({ retire: false, returnFocus: event.currentTarget })}>Review changes</Button><ActionMenu label={`Actions for ${name}`} actions={[{ label: "Retire resource…", danger: true, hidden: !record.enabled, onSelectWithTrigger: origin => setReview({ retire: true, returnFocus: origin }) }]} /></>;
  return <>{children ? children(actions) : <HeaderActions>{actions}</HeaderActions>}{review && <DeploymentReviewDialog record={record} name={name} retire={review.retire} returnFocus={review.returnFocus} fallbackFocus={() => document.getElementById(triggerId)} onClose={() => setReview(null)} />}</>;
}
/** Mounted only while open: every new review fetches a fresh observation. */
export function DeploymentReviewDialog({ record, name, retire = false, restoreOf, returnFocus, fallbackFocus, onClose }: { record: Record; name: string; retire?: boolean; /** Restore the value a recorded change produced (#87). */ restoreOf?: string; returnFocus?: HTMLElement; fallbackFocus?: () => HTMLElement | null; onClose: () => void }) {
  const ask = useAction(), scope = useApiScope(), client = useQueryClient(), confirming = useRef(false);
  const path = `${platformPath}/deployments/${encodeURIComponent(record.id)}`;
  const previewPath = restoreOf ? `${path}/configuration/restore-preview` : `${path}/configuration/preview`;
  const requested = retire ? false : !record.enabled;
  const q = useQuery({ queryKey: ["api", scope, previewPath, restoreOf ?? requested], queryFn: ({ signal }) => api<DeploymentPreview>(previewPath, { method: "POST", body: restoreOf ? { change_id: restoreOf } : { enabled: requested }, signal }), retry: false, staleTime: 0, gcTime: 0, refetchOnMount: "always", refetchOnWindowFocus: false });
  // A restore applies the source's recorded value; otherwise the toggle.
  const enabled = restoreOf ? q.data?.restore?.enabled ?? requested : requested;
  const mismatch = q.data && (q.data.target.kind !== "deployment" || q.data.target.id !== record.id || (restoreOf !== undefined && q.data.restore?.change_id !== restoreOf));
  const preview = !q.isError && !q.isFetching && !mismatch ? q.data : undefined;
  const restoreAfterRefresh = () => {
    const previousFallback = fallbackFocus?.();
    // ActionProvider has already invalidated these queries. Join its in-flight
    // refetches without cancelling/restarting them, then resolve the stable id
    // after the refreshed render (headers and regrouped rows can remount).
    void client.refetchQueries({ queryKey: ["api", scope], type: "active", fetchStatus: "fetching" }, { cancelRefetch: false }).then(() => {
      requestAnimationFrame(() => {
        const active = document.activeElement;
        // Do not steal focus if the user moved on while the refresh was pending.
        if (active !== document.body && active !== returnFocus && active !== previousFallback) return;
        const destination = fallbackFocus?.();
        if (destination?.isConnected) destination.focus();
      });
    });
  };
  const unchanged = preview !== undefined && preview.diff.length === 0;
  const confirm = () => {
    if (!preview || unchanged) return;
    confirming.current = true;
    onClose();
    const verb = restoreOf ? "Restore" : retire ? "Retire" : enabled ? "Enable" : "Disable";
    // Retire keeps its DELETE semantics; every other change applies only at the
    // reviewed revision, so an intervening change refuses it (409) instead of
    // silently overwriting it.
    ask({ title: `${verb} ${name}?`, returnFocus, fallbackFocus, after: restoreAfterRefresh, description: `This changes eligibility for new inference requests. The preview is an estimate, not a reservation; current authority and capability checks still apply.${retire ? "" : ` It applies only at revision ${preview.expected_revision}: if the route changed since this preview, nothing is changed and you review it again.`}${retire ? " Retire does not check the previewed revision: refresh the preview if the route may have changed." : ""}`, danger: !enabled, submitLabel: restoreOf ? "Restore configuration" : retire ? "Retire resource" : enabled ? "Enable" : "Disable", run: (_, signal) => retire ? api(path, { method: "DELETE", signal }) : api<DeploymentApplyResult>(`${path}/configuration/apply`, { method: "POST", body: { enabled, expected_revision: preview.expected_revision, ...(restoreOf ? { restore_of: restoreOf } : {}) }, signal }) });
  };
  return <Dialog open title={restoreOf ? "Review restore" : "Review changes"} description={restoreOf ? `Restore ${name} to a recorded configuration` : `${retire ? "Retire" : enabled ? "Enable" : "Disable"} ${name}`} onOpenChange={open => { if (!open) onClose(); }} finalFocus={() => confirming.current ? false : returnFocus?.isConnected ? returnFocus : fallbackFocus?.() ?? true} size="lg" footer={<><Button variant="secondary" onClick={onClose}>Cancel</Button><Button variant="secondary" disabled={q.isFetching} onClick={() => void q.refetch()}>Refresh preview</Button><Button disabled={!preview || unchanged} onClick={confirm}>Continue to confirmation</Button></>}>
    {q.isFetching && <p role="status">Loading impact preview…</p>}
    {q.isError && <ErrorNotice error={q.error} retry={() => void q.refetch()} />}
    {mismatch && <ErrorNotice error={new Error("The gateway returned a different deployment. Confirmation is unavailable.")} />}
    {preview && <Stack gap={4} role="region" aria-label="Deployment change details" tabIndex={0}>
      {preview.restore && <p>Restores the configuration recorded at revision {preview.restore.revision} (<DateTime value={preview.restore.created_at} />): {preview.restore.enabled ? "enabled" : "disabled"}.</p>}
      {unchanged && <Alert tone="info" title="No change">The route already has this configuration. Nothing would be applied.</Alert>}
      <DescriptionList className={s.facts} items={[
        { label: "Enabled", value: preview.diff.length ? preview.diff.map(d => `${d.before ? "Enabled" : "Disabled"} → ${d.after ? "Enabled" : "Disabled"}`).join(", ") : "No configuration change" },
        { label: "Serving routes gained / lost", value: `${preview.serving_routes_gained.length} / ${preview.serving_routes_lost.length}` },
        { label: "Observed", value: <DateTime value={preview.observed_at} /> },
        { label: "Expected revision (estimate only)", value: <code>{preview.expected_revision}</code> },
      ]} />
      {preview.models_losing_last_serving_route.length > 0 && <Alert tone="warning" title="Last serving route lost"><ul className={s.notes}>{preview.models_losing_last_serving_route.map(m => <li key={m.id}>{m.public_name}</li>)}</ul></Alert>}
      <section aria-label="Affected workspaces"><p>{preview.affected_workspaces.total} active shared workspaces with model selections affected.</p>{preview.affected_workspaces.truncated && <p>Showing the first {preview.affected_workspaces.limit}; more authorized workspaces are affected.</p>}<ul className={s.notes}>{preview.affected_workspaces.data.map(w => <li key={w.id}>{w.name}</li>)}</ul></section>
      <Alert tone="info" title="Configuration estimate"><ul className={s.notes} aria-label="Configuration estimate warnings">{preview.warnings.map(w => <li key={w}>{w}</li>)}</ul></Alert>
    </Stack>}
  </Dialog>;
}

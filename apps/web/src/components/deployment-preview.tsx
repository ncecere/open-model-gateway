import { useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, platformPath, type DeploymentPreview } from "../lib/api";
import { Button, DateTime, ErrorNotice, Stack, useAction, useApiScope } from "./ui";
import { Dialog } from "./ui/dialog/dialog";
import { DescriptionList } from "./ui/description-list/description-list";
import { Alert } from "./ui/alert/alert";
import { ActionMenu } from "./templates/action-menu";

type Record = { id: string; enabled: boolean };
export function DeploymentReviewButton({ record, name }: { record: Record; name: string }) {
  const [reviewing, setReviewing] = useState(false), trigger = useRef<HTMLButtonElement>(null);
  return <><Button ref={trigger} variant="secondary" onClick={() => setReviewing(true)}>Review changes</Button>{reviewing && <DeploymentReviewDialog record={record} name={name} returnFocus={trigger.current ?? undefined} onClose={() => setReviewing(false)} />}</>;
}
/** Retire is also disablement, preserving its existing DELETE and audit semantics. */
export function DeploymentStatusActions({ record, name }: { record: Record; name: string }) {
  const [retiring, setRetiring] = useState(false);
  return <><DeploymentReviewButton record={record} name={name} /><ActionMenu label={`Actions for ${name}`} actions={[{ label: "Retire resource…", danger: true, hidden: !record.enabled, onSelect: () => setRetiring(true) }]} />{retiring && <DeploymentReviewDialog record={record} name={name} retire onClose={() => setRetiring(false)} />}</>;
}
/** Mounted only while open: every new review fetches a fresh observation. */
export function DeploymentReviewDialog({ record, name, retire = false, returnFocus, onClose }: { record: Record; name: string; retire?: boolean; returnFocus?: HTMLElement; onClose: () => void }) {
  const ask = useAction(), scope = useApiScope(), enabled = retire ? false : !record.enabled, confirming = useRef(false);
  const path = `${platformPath}/deployments/${encodeURIComponent(record.id)}`;
  const q = useQuery({ queryKey: ["api", scope, `${path}/configuration/preview`, enabled], queryFn: ({ signal }) => api<DeploymentPreview>(`${path}/configuration/preview`, { method: "POST", body: { enabled }, signal }), retry: false, staleTime: 0, gcTime: 0, refetchOnMount: "always", refetchOnWindowFocus: false });
  const mismatch = q.data && (q.data.target.kind !== "deployment" || q.data.target.id !== record.id);
  const preview = !q.isError && !q.isFetching && !mismatch ? q.data : undefined;
  const confirm = () => {
    if (!preview) return;
    confirming.current = true;
    onClose();
    ask({ title: retire ? `Retire ${name}?` : `${enabled ? "Enable" : "Disable"} ${name}?`, returnFocus, description: "This changes eligibility for new inference requests. The preview is an estimate, not a reservation; current authority and capability checks still apply. Existing apply routes do not enforce preview revisions.", danger: !enabled, submitLabel: retire ? "Retire resource" : enabled ? "Enable" : "Disable", run: (_, signal) => api(path, { method: retire ? "DELETE" : "PATCH", ...(retire ? {} : { body: { enabled } }), signal }) });
  };
  return <Dialog open title="Review changes" description={`${retire ? "Retire" : enabled ? "Enable" : "Disable"} ${name}`} onOpenChange={open => { if (!open) onClose(); }} finalFocus={() => confirming.current ? false : returnFocus?.isConnected ? returnFocus : true} size="lg" footer={<><Button variant="secondary" onClick={onClose}>Cancel</Button><Button variant="secondary" disabled={q.isFetching} onClick={() => void q.refetch()}>Refresh preview</Button><Button disabled={!preview} onClick={confirm}>Continue to confirmation</Button></>}>
    {q.isFetching && <p role="status">Loading impact preview…</p>}
    {q.isError && <ErrorNotice error={q.error} retry={() => void q.refetch()} />}
    {mismatch && <ErrorNotice error={new Error("The gateway returned a different deployment. Confirmation is unavailable.")} />}
    {preview && <Stack gap={4}>
      <DescriptionList items={[
        { label: "Enabled", value: preview.diff.length ? preview.diff.map(d => `${d.before ? "Enabled" : "Disabled"} → ${d.after ? "Enabled" : "Disabled"}`).join(", ") : "No configuration change" },
        { label: "Serving routes gained / lost", value: `${preview.serving_routes_gained.length} / ${preview.serving_routes_lost.length}` },
        { label: "Observed", value: <DateTime value={preview.observed_at} /> },
        { label: "Expected revision (estimate only)", value: <code>{preview.expected_revision}</code> },
      ]} />
      {preview.models_losing_last_serving_route.length > 0 && <Alert tone="warning" title="Last serving route lost"><ul>{preview.models_losing_last_serving_route.map(m => <li key={m.id}>{m.public_name}</li>)}</ul></Alert>}
      <section aria-label="Affected workspaces"><p>{preview.affected_workspaces.total} active shared workspaces with model selections affected.</p>{preview.affected_workspaces.truncated && <p>Showing the first {preview.affected_workspaces.limit}; more authorized workspaces are affected.</p>}<ul>{preview.affected_workspaces.data.map(w => <li key={w.id}>{w.name}</li>)}</ul></section>
      <Alert tone="info" title="Configuration estimate"><ul>{preview.warnings.map(w => <li key={w}>{w}</li>)}</ul></Alert>
    </Stack>}
  </Dialog>;
}

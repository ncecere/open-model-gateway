import { useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError, platformPath, type DeploymentBulkOperation, type DeploymentPreview } from "../lib/api";
import { Button, ErrorNotice, Stack, useApiScope } from "./ui";
import { Dialog } from "./ui/dialog/dialog";
import { Alert } from "./ui/alert/alert";

export type BulkTarget = { id: string; name: string; model?: { id: string; name: string } };
const outcomeLabel: Record<string, string> = { committed: "Disabled", unchanged: "Already disabled", conflict: "Changed since review: not disabled", refused: "Refused: not disabled", unattempted: "Not attempted: not disabled" };
const reasonLabel: Record<string, string> = { not_found: "route no longer exists", forbidden: "your Platform Admin access ended", revision_conflict: "review it again" };

/**
 * Explicit targets only (#88): every selected route is previewed for its
 * current revision, then disabled at that revision in its own transaction.
 * One idempotency key per opened dialog: a retry never applies twice.
 */
// No answer (network), a server failure, an incomplete operation, or a key
// already used: some targets may have committed. Other refusals applied
// nothing.
const uncertain = (e: unknown) => !(e instanceof ApiError) || e.status === 0 || e.status >= 500 || e.code === "invalid_response" || e.status === 408 || e.reason === "bulk_incomplete" || e.reason === "idempotency_key_reused";

export function BulkDisableDialog({ targets: selection, onClose, onDone }: { targets: BulkTarget[]; onClose: () => void; onDone: (disabled: string[]) => void }) {
  // The selection as reviewed: the page may change its own while this is open.
  const [targets] = useState(selection);
  const scope = useApiScope(), client = useQueryClient();
  const key = useRef(crypto.randomUUID()).current;
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>(null), [result, setResult] = useState<DeploymentBulkOperation | null>(null);
  // Once a request's outcome is unknown, its key is never posted again with
  // another review: only its status is read.
  const [mustCheck, setMustCheck] = useState(false);
  const previews = useQuery({
    queryKey: ["api", scope, "bulk-disable-preview", targets.map(t => t.id).join(",")],
    queryFn: ({ signal }) => Promise.all(targets.map(t => api<DeploymentPreview>(`${platformPath}/deployments/${encodeURIComponent(t.id)}/configuration/preview`, { method: "POST", body: { enabled: false }, signal }))),
    retry: false, staleTime: 0, gcTime: 0, refetchOnMount: "always", refetchOnWindowFocus: false,
  });
  const reviewed = previews.data && previews.data.every((p, i) => p.target.id === targets[i].id) ? previews.data : undefined;
  const names = new Map(targets.map(t => [t.id, t.name]));
  const finish = (operation: DeploymentBulkOperation) => { setResult(operation); void client.invalidateQueries({ queryKey: ["api", scope] }); if (operation.completed) onDone(operation.items.filter(i => i.outcome === "committed" || i.outcome === "unchanged").map(i => i.id)); };
  const run = async () => {
    if (!reviewed) return;
    setBusy(true); setError(null);
    try {
      finish(await api<DeploymentBulkOperation>(`${platformPath}/deployment-bulk-operations`, { method: "POST", body: { action: "disable", idempotency_key: key, items: targets.map((t, i) => ({ id: t.id, expected_revision: reviewed[i].expected_revision })) } }));
    } catch (e) { setError(e); if (uncertain(e)) setMustCheck(true); } finally { setBusy(false); }
  };
  // The outcome of an interrupted request is read, never replayed blindly.
  const status = async () => {
    setBusy(true); setError(null);
    try { finish(await api<DeploymentBulkOperation>(`${platformPath}/deployment-bulk-operations/${encodeURIComponent(key)}`)); }
    catch (e) {
      // No operation under this key: the request never reached the gateway
      // and nothing was applied, so it may be reviewed and sent again.
      if (e instanceof ApiError && e.status === 404) { setMustCheck(false); setError(null); void previews.refetch(); }
      else setError(e);
    } finally { setBusy(false); }
  };
  const unknown = mustCheck && !result;
  const lastRoute = reviewed?.flatMap(p => p.models_losing_last_serving_route.map(m => m.public_name)) ?? [];
  // Each review counts its route alone: routes of one model selected together
  // can remove its last serving route although no single review says so.
  const counts = new Map<string, { name: string; n: number }>();
  for (const t of targets) if (t.model) counts.set(t.model.id, { name: t.model.name, n: (counts.get(t.model.id)?.n ?? 0) + 1 });
  const shared = [...counts.values()].filter(m => m.n > 1).map(m => m.name);
  return <Dialog open title={result ? "Bulk disable result" : `Disable ${targets.length} routes`} description={result ? undefined : "Each route is disabled in its own transaction at the revision shown. A route that changed since this review is not disabled."} onOpenChange={open => { if (!open) onClose(); }} size="lg" footer={result
    ? <>{!result.completed && <Button variant="secondary" disabled={busy} onClick={() => void status()}>Check outcome</Button>}<Button onClick={onClose}>Close</Button></>
    : <><Button variant="secondary" onClick={onClose}>Cancel</Button><Button variant="secondary" disabled={previews.isFetching || busy || mustCheck} onClick={() => void previews.refetch()}>Refresh review</Button>{unknown && <Button variant="secondary" disabled={busy} onClick={() => void status()}>Check outcome</Button>}<Button disabled={!reviewed || busy || previews.isFetching || mustCheck} onClick={() => void run()}>{`Disable ${targets.length} routes`}</Button></>}>
    <Stack gap={4}>
      {previews.isFetching && <p role="status">Reviewing the selected routes…</p>}
      {previews.isError && <ErrorNotice error={previews.error} retry={() => void previews.refetch()} />}
      {error !== null && <ErrorNotice error={error} />}
      {unknown && <Alert tone="warning" title="Outcome unknown">The request did not finish. Check the outcome before trying again: routes that were already disabled stay disabled. Close this dialog only after checking; the outcome cannot be looked up here afterwards.</Alert>}
      {result && !result.completed && <Alert tone="warning" title="Still in progress">Some routes have no outcome yet and may still be disabled. Check again in a moment; an interrupted operation completes within about 30 seconds.</Alert>}
      {result ? <ul aria-label="Bulk disable outcomes">{result.items.map(item => <li key={item.id}>{names.get(item.id) ?? item.id}: {item.outcome ? outcomeLabel[item.outcome] ?? item.outcome : "No outcome yet"}{item.reason ? ` (${reasonLabel[item.reason] ?? item.reason})` : ""}</li>)}</ul>
        : reviewed && <>
          {lastRoute.length > 0 && <Alert tone="warning" title="Last serving route">Disabling these routes one by one can leave a model without a serving route: {[...new Set(lastRoute)].join(", ")}.</Alert>}
          {shared.length > 0 && <Alert tone="warning" title="Several routes of one model">Each route is reviewed on its own. Disabling these together may leave a model without a serving route: {shared.join(", ")}.</Alert>}
          <ul aria-label="Selected routes">{targets.map((t, i) => <li key={t.id}>{t.name} · {reviewed[i].diff.length ? "Enabled → Disabled" : "Already disabled"} · revision <code>{reviewed[i].expected_revision}</code></li>)}</ul>
        </>}
    </Stack>
  </Dialog>;
}

/*
 * Route page › Batch scheduling: the route's batch queue (lines queued and
 * running, why it is paused, the last server metrics reading) and its
 * settings. Edited in place from the section header (one dialog).
 */
import { api } from "../lib/api";
import { batchSchedulingPath, isFault, pauseLabel, readingText, schedulingBody, schedulingFields, settingsRows, type BatchSchedulingDocument } from "../lib/batch-scheduling";
import { DateTime, ErrorNotice, useAction, useApi } from "./ui";
import { DescriptionList } from "./ui/description-list/description-list";
import { Stack } from "./ui/layout/layout";
import { StatTile, StatTileGrid } from "./templates/stat-tile";

export const useBatchScheduling = (deployment: string) => useApi<BatchSchedulingDocument>(batchSchedulingPath(deployment));

/** Open the edit dialog. */
export function editBatchScheduling(ask: ReturnType<typeof useAction>, deployment: string, doc: BatchSchedulingDocument, title: string) {
  ask({
    title: `Batch scheduling · ${title}`,
    description: "Gateway-run batch lines only. Running lines always finish.",
    fields: schedulingFields(doc.settings, doc.priority_supported),
    submitLabel: "Save",
    successNotice: "Batch scheduling saved.",
    run: (values, signal) => api(batchSchedulingPath(deployment), { method: "PUT", body: schedulingBody(values), signal }),
  });
}

export function RouteBatchScheduling({ query }: { query: ReturnType<typeof useBatchScheduling> }) {
  if (query.isPending) return <p role="status">Loading batch scheduling…</p>;
  if (query.isError) return <ErrorNotice error={query.error} retry={() => void query.refetch()} />;
  const { settings, status, priority_supported } = query.data;
  const reading = readingText(status.metrics);
  const state = status.paused_reason ? pauseLabel(status.paused_reason) : status.running_lines ? "Starting lines" : "Idle";
  return <Stack gap={4}>
    <StatTileGrid label="Batch queue" columns={4}>
      <StatTile label="Queued lines" value={status.queued_lines.toLocaleString("en-US")} hint={status.waiting_batches ? `${status.waiting_batches} batch${status.waiting_batches === 1 ? "" : "es"}` : undefined} />
      <StatTile label="Running" value={`${status.running_lines} / ${settings.max_concurrency}`} hint={status.live_in_flight != null ? `${status.live_in_flight} live in flight` : undefined} />
      <StatTile label="Status" value={<span title={isFault(status.paused_reason) ? "The server's metrics could not be read: no new lines start until they can." : undefined}>{state}</span>} hint={status.checked_at ? <>Checked <DateTime value={status.checked_at} /></> : undefined} />
      <StatTile label="Server load" value={reading ?? (settings.metrics ? "Not read yet" : "Not configured")} hint={status.metrics?.checked_at ? <>Read <DateTime value={status.metrics.checked_at} /></> : undefined} />
    </StatTileGrid>
    <DescriptionList dividers items={settingsRows(settings, priority_supported)} />
  </Stack>;
}

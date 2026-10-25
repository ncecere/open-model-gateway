import { useRef } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type MappingPreview, type MappingPreviewEntry } from "../lib/api";
import { Button, ErrorNotice, Stack, useAction } from "./ui";
import { Dialog } from "./ui/dialog/dialog";
import { DescriptionList } from "./ui/description-list/description-list";
import { Alert } from "./ui/alert/alert";

/**
 * A fully-prepared mapping proposal (#89): the field editor (create/edit) or
 * the row menu (delete) has already collected everything the server needs.
 * `applyMethod`/`applyPath`/`body` are exactly what the existing save path
 * (create/update/delete) would be called with directly -- this review step
 * changes nothing about that call, it only interposes a read-only preview
 * (and a second, explicit confirmation) before it.
 */
export type MappingReviewRequest = {
  previewPath: string;
  body: Record<string, unknown>;
  /** What is being proposed, shown as the review dialog's description. */
  title: string;
  confirmTitle: string;
  confirmDescription?: string;
  confirmLabel: string;
  danger?: boolean;
  applyPath: string;
  applyMethod: "POST" | "PATCH" | "DELETE";
  /** The control that opened this review (preserved across the whole
   * create/edit/delete → review → confirm chain, learned from #85's
   * deployment-configuration preview focus fixes). */
  returnFocus?: HTMLElement;
  /** A stable element to fall back to once `returnFocus` is no longer
   * connected (for example, a row menu item removed when its menu closed). */
  fallbackFocus?: () => HTMLElement | null;
};

const entryName = (e: MappingPreviewEntry) => e.display_name?.trim() || e.email || e.user_id;
function EntryList({ title, entries }: { title: string; entries: MappingPreviewEntry[] }) {
  if (entries.length === 0) return null;
  return (
    <section aria-label={title}>
      <p>
        {title} ({entries.length})
      </p>
      <ul>
        {entries.map(e => (
          <li key={e.user_id}>
            {entryName(e)}
            {e.role ? ` \u00b7 ${e.role}` : ""}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** Mounted only while open: every review fetches a fresh observation (mirrors #85's `DeploymentReviewDialog`). */
export function MappingReviewDialog({ request, onClose }: { request: MappingReviewRequest; onClose: () => void }) {
  const ask = useAction(), client = useQueryClient(), confirming = useRef(false);
  const q = useQuery({
    queryKey: ["api", "mapping-preview", request.previewPath, JSON.stringify(request.body)],
    queryFn: ({ signal }) => api<MappingPreview>(request.previewPath, { method: "POST", body: request.body, signal }),
    retry: false,
    staleTime: 0,
    gcTime: 0,
    refetchOnMount: "always",
    refetchOnWindowFocus: false,
  });
  const preview = !q.isError && !q.isFetching ? q.data : undefined;
  const restoreAfterRefresh = () => {
    const previousFallback = request.fallbackFocus?.();
    // ActionProvider has already invalidated these queries. Join its
    // in-flight refetches without cancelling/restarting them, then resolve
    // the stable destination after the refreshed render (rows/menus can
    // remount).
    void client
      .refetchQueries({ queryKey: ["api"], type: "active", fetchStatus: "fetching" }, { cancelRefetch: false })
      .then(() => {
        requestAnimationFrame(() => {
          const active = document.activeElement;
          // Do not steal focus if the user moved on while the refresh was pending.
          if (active !== document.body && active !== request.returnFocus && active !== previousFallback) return;
          const destination = request.fallbackFocus?.();
          if (destination?.isConnected) destination.focus();
        });
      });
  };
  const confirm = () => {
    if (!preview) return;
    confirming.current = true;
    onClose();
    ask({
      title: request.confirmTitle,
      description: request.confirmDescription,
      danger: request.danger,
      submitLabel: request.confirmLabel,
      returnFocus: request.returnFocus,
      after: restoreAfterRefresh,
      run: (_, signal) =>
        api(request.applyPath, {
          method: request.applyMethod,
          ...(request.applyMethod === "DELETE" ? {} : { body: request.body }),
          signal,
        }),
    });
  };
  return (
    <Dialog
      open
      title="Review changes"
      description={request.title}
      onOpenChange={open => {
        if (!open) onClose();
      }}
      finalFocus={() => (confirming.current ? false : request.returnFocus?.isConnected ? request.returnFocus : request.fallbackFocus?.() ?? true)}
      size="lg"
      footer={
        <>
          <Button variant="secondary" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="secondary" disabled={q.isFetching} onClick={() => void q.refetch()}>
            Refresh preview
          </Button>
          <Button disabled={!preview} onClick={confirm}>
            Continue to confirmation
          </Button>
        </>
      }
    >
      {q.isFetching && <p role="status">Loading impact preview…</p>}
      {q.isError && <ErrorNotice error={q.error} retry={() => void q.refetch()} />}
      {preview && (
        <Stack gap={4}>
          <DescriptionList
            items={[
              { label: "Membership basis", value: preview.membership_basis === "scim" ? "SCIM roster" : "Sign-in derived" },
              { label: "Gained / lost", value: `${preview.counts.gained} / ${preview.counts.lost}` },
            ]}
          />
          <EntryList title="Platform role gained" entries={preview.diff.platform_role_gained} />
          <EntryList title="Platform role lost" entries={preview.diff.platform_role_lost} />
          <EntryList title="Workspace membership gained" entries={preview.diff.workspace_membership_gained} />
          <EntryList title="Workspace membership lost" entries={preview.diff.workspace_membership_lost} />
          {preview.counts.truncated && (
            <Alert tone="warning" title="List truncated">
              Showing the first 200 of each affected list; more are affected.
            </Alert>
          )}
          <Alert tone="info" title="Configuration estimate">
            <ul>
              {preview.warnings.map(w => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          </Alert>
        </Stack>
      )}
    </Dialog>
  );
}

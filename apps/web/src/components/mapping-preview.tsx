import { useRef } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, type MappingPreview, type MappingPreviewEntry, type MappingPreviewImmediate } from "../lib/api";
import { Button, Stack, useAction } from "./ui";
import { Dialog } from "./ui/dialog/dialog";
import { DescriptionList } from "./ui/description-list/description-list";
import { Alert, ErrorAlert, errorMessage } from "./ui/alert/alert";

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
  /** Called when the confirmed apply is refused with 412 (the mapping
   * changed between review and apply, #89 review item 6): reopen a fresh
   * review instead of leaving the user with a stale one. */
  onStale?: () => void;
};

const entryName = (e: { display_name: string | null; email: string | null; user_id: string }) => e.display_name?.trim() || e.email || e.user_id;
function roleChange(before: string | null, after: string | null) {
  if (before && after) return `${before} → ${after}`;
  return after ?? before ?? "";
}
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
            {entryName(e)} · {roleChange(e.before, e.after)}
            {e.reactivated ? " (reactivated)" : ""}
            {e.would_be_suspended ? " (would be suspended)" : ""}
          </li>
        ))}
      </ul>
    </section>
  );
}
function RevokedList({ title, entries }: { title: string; entries: { user_id: string; email: string | null; display_name: string | null; role: string }[] }) {
  if (entries.length === 0) return null;
  return (
    <section aria-label={title}>
      <p>
        {title} ({entries.length})
      </p>
      <ul>
        {entries.map(e => (
          <li key={e.user_id}>
            {entryName(e)} · {e.role}
          </li>
        ))}
      </ul>
    </section>
  );
}
/** What the real write does synchronously, the instant it commits -- distinct
 * from `diff`, which is the net state once reconciliation has caught up. */
function ImmediateSection({ immediate }: { immediate: MappingPreviewImmediate }) {
  if (!immediate.applies) return null;
  return (
    <Alert tone={immediate.blocked_by_last_admin ? "danger" : "warning"} title="Applied immediately, not at next sign-in">
      <Stack gap={2}>
        {immediate.blocked_by_last_admin && <p>This would strand the last platform administrator; the real request would be refused.</p>}
        <RevokedList title="Platform role revoked now" entries={immediate.platform_role_revoked} />
        <RevokedList title="Workspace membership revoked now" entries={immediate.workspace_membership_revoked} />
        {immediate.users_suspended.length > 0 && (
          <p>
            {immediate.users_suspended.length} user{immediate.users_suspended.length === 1 ? "" : "s"} would be suspended immediately for losing their only platform entitlement:{" "}
            {immediate.users_suspended.map(entryName).join(", ")}.
          </p>
        )}
        {immediate.keys_revoked_count > 0 && <p>{immediate.keys_revoked_count} API key{immediate.keys_revoked_count === 1 ? "" : "s"} would be permanently revoked for losing their only workspace membership. Revoked keys never reactivate, even once reconciliation restores the membership.</p>}
      </Stack>
    </Alert>
  );
}

/** `ErrorNotice`'s generic per-status title (components/ui.tsx) can read
 * identically to the server's own message (a plain 403 "Access denied" is
 * common) -- showing both then repeats the same text twice in one alert.
 * This keeps the same title mapping but drops the message body when it
 * would only restate the title, so the preview's refusal renders once. */
function PreviewError({ error, retry }: { error: unknown; retry: () => void }) {
  const title = error instanceof ApiError && error.status === 403 ? "Access denied" : error instanceof ApiError && error.status === 404 ? "Not found" : "Unable to complete request";
  return <ErrorAlert error={error} title={title} describe={e => (errorMessage(e) === title ? { message: "" } : undefined)} onRetry={retry} />;
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
      run: async (_, signal) => {
        try {
          return await api(request.applyPath, {
            method: request.applyMethod,
            // The server compares this against a quoted ETag-shaped digest
            // (`"<64 lowercase hex>"`, directory.rs's `check_if_match`); an
            // unquoted value is refused outright as malformed.
            headers: preview.digest ? { "If-Match": `"${preview.digest}"` } : undefined,
            ...(request.applyMethod === "DELETE" ? {} : { body: request.body }),
            signal,
          });
        } catch (error) {
          if (error instanceof ApiError && error.status === 412) request.onStale?.();
          throw error;
        }
      },
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
      {q.isError && <PreviewError error={q.error} retry={() => void q.refetch()} />}
      {preview && (
        <Stack gap={4}>
          <DescriptionList
            items={[
              { label: "Membership basis", value: preview.membership_basis === "scim" ? "SCIM roster" : "Sign-in derived" },
              { label: "Gained / lost at next sign-in", value: `${preview.counts.gained} / ${preview.counts.lost}` },
            ]}
          />
          <ImmediateSection immediate={preview.immediate} />
          {!preview.complete && (
            <Alert tone="warning" title="Incomplete preview">
              This group's membership is too large to fully evaluate; this preview may undercount who would gain access.
            </Alert>
          )}
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

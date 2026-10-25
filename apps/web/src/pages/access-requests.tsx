/*
 * Self-service membership requests (#83, slice A): join a discoverable Team/Project as
 * `member`. Three surfaces, all browser-session-only (docs/management-api.md):
 *   - Workspace portal "Request access" (discover + submit) and "My requests" (status, withdraw).
 *   - Workspace Settings › Access requests: the owner/admin approval queue for that workspace.
 *   - Admin › Access requests: the platform queue/history (Platform Admin approves; Auditor reads).
 * Server capabilities are presentation only: Rust rechecks authority, discoverability, live
 * membership and stale-request conflicts on every write. Reasons are always rendered as plain
 * text, never HTML.
 */
import { UserPlus, Inbox } from "lucide-react";
import { api, type Session, type Workspace } from "../lib/api";
import { accessRequestStatusLabels, accessRequestStatusTone, accessRequestTargetsPath, myAccessRequestsPath, newIdempotencyKey, platformAccessRequestDecisionPath, platformAccessRequestsPath, reasonError, withdrawAccessRequestPath, workspaceAccessRequestDecisionPath, workspaceAccessRequestsPath, type AccessRequest, type AccessRequestTarget } from "../lib/access-requests";
import { permissions } from "../lib/permissions";
import { Heading, Id, Stack, useAction, useChoices } from "../components/ui";
import { ResourcePage } from "../components/resource-page";
import { DirectoryTable, LocalTable, PersonIdentity } from "../components/people";
import { StatusBadge } from "../components/ui/badge/badge";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import { kindLabels } from "../lib/people";
import s from "./shared.module.css";

function StatusCell({ status }: { status: AccessRequest["status"] }) {
  return <StatusBadge tone={accessRequestStatusTone[status]}>{accessRequestStatusLabels[status]}</StatusBadge>;
}
/** Plain text only, as required (#83): never render a reason as HTML. */
function ReasonCell({ reason }: { reason: string | null }) {
  return reason ? <span className={s.secondary}>{reason}</span> : <span className={s.muted}>No reason given</span>;
}

/** Workspace portal: discoverable Teams/Projects the caller has no live grant on. */
function DiscoverTargets() {
  const ask = useAction();
  const columns: DataTableColumn<AccessRequestTarget>[] = [
    { id: "name", header: "Name", accessor: t => t.name, rowHeader: true, hideable: false, cell: t => t.name },
    { id: "kind", header: "Type", accessor: t => kindLabels[t.kind], cell: t => kindLabels[t.kind] },
  ];
  const request = (t: AccessRequestTarget) => {
    const idempotencyKey = newIdempotencyKey();
    ask({
      title: `Request to join ${t.name}?`, description: "A workspace owner or admin reviews your request. You'll see the outcome in My requests.",
      fields: [{ name: "reason", label: "Reason", type: "textarea", placeholder: "Why do you need access?", validate: reasonError }],
      submitLabel: "Send request", successNotice: "Request sent.",
      run: (v, signal) => api<AccessRequest>(myAccessRequestsPath, { method: "POST", body: { workspace_id: t.id, idempotency_key: idempotencyKey, reason: v.reason || undefined }, signal }),
    });
  };
  return <DirectoryTable<AccessRequestTarget> path={accessRequestTargetsPath} label="Discoverable workspaces" storageKey="access-request-targets" columns={columns} rowKey={t => t.id} rowLabel={t => t.name}
    rowActions={t => [{ label: "Request access…", icon: <UserPlus aria-hidden />, onSelect: () => request(t) }]}
    empty={{ icon: <Inbox />, title: "No discoverable workspaces.", description: "A platform admin makes a Team or Project discoverable before you can ask to join it." }} />;
}
/**
 * Workspace portal: the caller's own requests. Only submitted references are shown; the
 * workspace name shown here is `workspace_name_at_request`, frozen at submission time, never
 * a live/renamed name or other newly disclosed membership data (docs/management-api.md).
 */
function MyRequests() {
  const ask = useAction();
  const withdraw = (r: AccessRequest) => ask({ title: "Withdraw this request?", description: "You can send a new request later.", submitLabel: "Withdraw", successNotice: "Request withdrawn.", run: (_, signal) => api<AccessRequest>(withdrawAccessRequestPath(r.id), { method: "POST", signal }) });
  const columns: DataTableColumn<AccessRequest>[] = [
    { id: "target", header: "Workspace", accessor: r => r.workspace_name_at_request, rowHeader: true, hideable: false, cell: r => <CellText primary={r.workspace_name_at_request} secondary={<ReasonCell reason={r.reason} />} /> },
    { id: "status", header: "Status", accessor: r => r.status, cell: r => <StatusCell status={r.status} /> },
    { id: "created", header: "Submitted", accessor: r => r.created_at, cell: r => <span className={s.secondary}>{new Date(r.created_at).toLocaleString()}</span> },
  ];
  return <DirectoryTable<AccessRequest> path={myAccessRequestsPath} label="My requests" storageKey="my-access-requests" columns={columns} rowKey={r => r.id} rowLabel={r => r.workspace_name_at_request}
    rowActions={r => [{ label: "Withdraw…", danger: true, hidden: r.status !== "pending", onSelect: () => withdraw(r) }]}
    empty={{ icon: <Inbox />, title: "No requests yet.", description: "Requests you send from Request access appear here." }} />;
}
/** Workspace portal top-level page: `?page=access-requests`. */
export function AccessRequestsPage({ tab, onTabChange }: { tab?: string; onTabChange: (tab: string) => void }) {
  return <ResourcePage title="Request access" description="Ask to join a discoverable Team or Project as a member." tab={tab} onTabChange={onTabChange} tabs={[
    { value: "discover", label: "Request access", icon: <UserPlus aria-hidden />, content: <DiscoverTargets /> },
    { value: "mine", label: "My requests", icon: <Inbox aria-hidden />, content: <MyRequests /> },
  ]} />;
}

/**
 * The requester's label for an approval queue row: display name, else email, else the opaque
 * ID as a last resort for an older gateway that hasn't deployed this field yet.
 */
const requesterLabel = (r: AccessRequest) => r.requester_display_name?.trim() || r.requester_email || r.requester;
/** A pending request's decision row actions: never the requester, approve/deny only while pending. */
function decisionActions(r: AccessRequest, selfId: string, writable: boolean, decide: (r: AccessRequest, approve: boolean) => void) {
  if (r.status !== "pending") return [];
  const disabled = r.requester === selfId, disabledReason = disabled ? "You can't approve or deny your own request." : undefined;
  return [
    { label: "Approve…", hidden: !writable, disabled, disabledReason, onSelect: () => decide(r, true) },
    { label: "Deny…", danger: true, hidden: !writable, disabled, disabledReason, onSelect: () => decide(r, false) },
  ];
}
/**
 * Workspace Settings › Access requests (owner/admin). This is an approval queue, not a history:
 * the server keeps every decided request for `mine` (the requester's own record) and the platform
 * page, but once a request here leaves `pending` it has nothing left to do and must stop occupying
 * the queue — a decided row that lingered would look like an approve/deny that silently failed.
 */
export function WorkspaceAccessRequestQueue({ session, workspace }: { session: Session; workspace: Workspace }) {
  const ask = useAction(), writable = permissions(session, workspace).manageTeam;
  const decide = (r: AccessRequest, approve: boolean) => ask({
    title: approve ? "Approve this request?" : "Deny this request?", danger: !approve,
    description: approve ? "Grants member access to this workspace." : "The requester can send a new request later.",
    submitLabel: approve ? "Approve" : "Deny", successNotice: approve ? "Request approved." : "Request denied.",
    run: (_, signal) => api<AccessRequest>(workspaceAccessRequestDecisionPath(workspace.id, r.id, approve), { method: "POST", signal }),
  });
  // An owner/admin already sees any entitled user's display name/email through the Members
  // "Add member" picker, so showing the same pair here (instead of a raw requester ID) discloses
  // nothing new (docs/management-api.md#self-service-membership-requests-83-slice-a).
  const columns: DataTableColumn<AccessRequest>[] = [
    { id: "requester", header: "Requester", accessor: requesterLabel, rowHeader: true, hideable: false, cell: r => <PersonIdentity person={{ display_name: r.requester_display_name, email: r.requester_email ?? null }} self={r.requester === session.user.id} /> },
    { id: "reason", header: "Reason", accessor: r => r.reason ?? "", cell: r => <ReasonCell reason={r.reason} /> },
    { id: "status", header: "Status", accessor: r => r.status, cell: r => <StatusCell status={r.status} /> },
    { id: "created", header: "Submitted", accessor: r => r.created_at, cell: r => <span className={s.secondary}>{new Date(r.created_at).toLocaleString()}</span> },
  ];
  const choices = useChoices<AccessRequest>(workspaceAccessRequestsPath(workspace.id));
  const pending = (choices.data ?? []).filter(r => r.status === "pending");
  return <LocalTable<AccessRequest> rows={pending} loading={choices.isFetching} error={choices.error} retry={() => void choices.refetch()}
    label="Access requests" storageKey="workspace-access-requests" columns={columns} rowKey={r => r.id} rowLabel={requesterLabel}
    rowActions={r => decisionActions(r, session.user.id, writable, decide)}
    empty={{ icon: <Inbox />, title: "No access requests.", description: "Requests to join this workspace appear here." }} />;
}
/** Admin › Access requests: platform queue and history. Platform Admin approves; Auditor reads only. */
export function PlatformAccessRequestsPage({ session }: { session: Session }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const decide = (r: AccessRequest, approve: boolean) => ask({
    title: approve ? "Approve this request?" : "Deny this request?", danger: !approve,
    description: approve ? "Grants member access to that workspace." : "The requester can send a new request later.",
    submitLabel: approve ? "Approve" : "Deny", successNotice: approve ? "Request approved." : "Request denied.",
    run: (_, signal) => api<AccessRequest>(platformAccessRequestDecisionPath(r.id, approve), { method: "POST", signal }),
  });
  // A Platform Admin/Auditor already sees any user's display name/email through the platform
  // user directory, so showing the same pair here (instead of a raw requester ID) discloses
  // nothing new (docs/management-api.md#self-service-membership-requests-83-slice-a).
  const columns: DataTableColumn<AccessRequest>[] = [
    { id: "requester", header: "Requester", accessor: requesterLabel, rowHeader: true, hideable: false, cell: r => <PersonIdentity person={{ display_name: r.requester_display_name, email: r.requester_email ?? null }} self={r.requester === session.user.id} /> },
    { id: "reason", header: "Reason", accessor: r => r.reason ?? "", cell: r => <ReasonCell reason={r.reason} /> },
    { id: "workspace", header: "Workspace", accessor: r => r.workspace_id, cell: r => <Id value={r.workspace_id} /> },
    { id: "status", header: "Status", accessor: r => r.status, cell: r => <StatusCell status={r.status} /> },
    { id: "created", header: "Submitted", accessor: r => r.created_at, cell: r => <span className={s.secondary}>{new Date(r.created_at).toLocaleString()}</span> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Access requests" description="Membership requests across every Team and Project." />
    <DirectoryTable<AccessRequest> path={platformAccessRequestsPath} label="Access requests" storageKey="platform-access-requests" columns={columns} rowKey={r => r.id} rowLabel={requesterLabel}
      rowActions={r => decisionActions(r, session.user.id, writable, decide)}
      empty={{ icon: <Inbox />, title: "No access requests yet.", description: "Requests to join a Team or Project appear here." }} />
  </Stack>;
}

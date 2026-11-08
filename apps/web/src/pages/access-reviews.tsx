/*
 * Access reviews (#90). Three surfaces, browser-session only
 * (docs/management-api.md#access-reviews-90):
 *   - Review access (/access-reviews): the items the signed-in person may
 *     decide now: keep, remove (applied at once, after a confirmation that
 *     says what it does) or flag group-provided access for the identity
 *     provider.
 *   - Admin › Access reviews: campaigns, and starting one (Platform Admins).
 *   - A campaign: its counts and items with each latest decision and
 *     reviewer; Platform Admins assign a reviewer and close it.
 * The server decides who may review what and refuses anything else; the
 * dashboard offers only the steps the viewer can take.
 */
import { ClipboardCheck, Plus } from "lucide-react";
import { api, platformPath, type Session } from "../lib/api";
import { accessText, itemLabel, removalTarget, campaignState, campaignStateLabels, campaignStateTone, decisionLabels, decisionPath, decisionTone, inboxPath, personText, removalConsequence, reviewClosePath, reviewExportPath, reviewerPath, reviewItemsPath, reviewPath, reviewsPath, sourceLabels, subjectLabels, type InboxItem, type ReviewCampaign, type ReviewCampaignDetail, type ReviewItem } from "../lib/access-reviews";
import { nameField, type Field } from "../lib/forms";
import type { DirectoryUser, DirectoryWorkspace } from "../lib/people";
import { Button, ErrorNotice, Heading, Inline, Stack, useAction, useApi } from "../components/ui";
import { DirectoryTable } from "../components/people";
import { ResourceLink } from "../components/navigation-link";
import { useResourceName } from "../components/layout/breadcrumbs";
import { StatusBadge } from "../components/ui/badge/badge";
import { TextLink } from "../components/ui/text-link/text-link";
import { CellText, type DataTableColumn } from "../components/ui/data-table/data-table";
import s from "./shared.module.css";

function DecisionCell({ item }: { item: ReviewItem }) {
  if (item.decision) return <Inline gap={2}><StatusBadge tone={decisionTone[item.decision]}>{decisionLabels[item.decision]}</StatusBadge>{!item.active && item.decision !== "remove" && <span className={s.muted}>No longer in effect</span>}</Inline>;
  return item.active ? <span className={s.muted}>Not decided</span> : <span className={s.muted}>No longer in effect</span>;
}
const personColumn = <T extends ReviewItem>(): DataTableColumn<T> => ({ id: "person", header: "Person", accessor: personText, rowHeader: true, hideable: false, cell: i => <CellText primary={personText(i)} secondary={i.display_name ? i.email ?? undefined : undefined} /> });
const accessColumn = <T extends ReviewItem>(): DataTableColumn<T> => ({ id: "access", header: "Access", accessor: accessText, cell: i => <CellText primary={accessText(i)} secondary={sourceLabels[i.source]} /> });

/**
 * Removing an owner's membership needs an owner of that workspace or a
 * Platform Admin (as on the members routes); the server refuses others.
 */
function mayRemove(session: Session, i: ReviewItem): boolean {
  if (i.source === "group") return false;
  if (i.subject_kind !== "workspace_membership" || i.role !== "owner") return true;
  return session.capabilities.platform_write || session.workspaces.some(w => w.id === i.workspace_id && w.role === "owner");
}

/** Review access: the viewer's items to decide. */
export function AccessReviewInbox({ session }: { session: Session }) {
  const ask = useAction();
  const decide = (i: InboxItem, decision: "keep" | "flag_idp" | "remove") => ask({
    title: decision === "remove" ? `Remove ${removalTarget(i)}?` : decision === "flag_idp" ? `Flag ${personText(i)}'s access for the identity provider?` : `Keep ${personText(i)} as ${accessText(i)}?`,
    danger: decision === "remove",
    description: decision === "remove" ? removalConsequence(i) : decision === "flag_idp" ? "This access comes from an identity provider group: it is fixed there, not here. The flag records your decision for the review." : `${personText(i)} keeps this access.`,
    submitLabel: decision === "remove" ? "Remove access" : decision === "flag_idp" ? "Flag" : "Keep",
    successNotice: decision === "remove" ? "Access removed." : decision === "flag_idp" ? "Flagged." : "Kept.",
    run: (_, signal) => api<ReviewItem>(decisionPath(i.id), { method: "POST", body: { decision }, signal }),
  });
  const columns: DataTableColumn<InboxItem>[] = [
    personColumn<InboxItem>(),
    accessColumn<InboxItem>(),
    { id: "review", header: "Review", accessor: i => i.campaign_name, cell: i => <CellText primary={i.campaign_name} secondary={`Due ${new Date(i.due_at).toLocaleDateString()}`} /> },
    { id: "decision", header: "Decision", accessor: i => i.decision ?? "", cell: i => <DecisionCell item={i} /> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Review access" description="Access you are asked to review: memberships of the Teams and Projects you own or administer, platform roles if you are a Platform Admin, and anything assigned to you. Keep it, remove it, or flag access from an identity provider group." />
    <DirectoryTable<InboxItem> path={inboxPath} label="Access to review" storageKey="access-review-inbox" columns={columns} rowKey={i => i.id} rowLabel={itemLabel}
      rowActions={i => [
        { label: "Keep…", onSelect: () => decide(i, "keep") },
        { label: "Flag for identity provider…", hidden: i.source !== "group", onSelect: () => decide(i, "flag_idp") },
        { label: "Remove…", danger: true, hidden: !mayRemove(session, i), onSelect: () => decide(i, "remove") },
      ]}
      empty={{ icon: <ClipboardCheck />, title: "Nothing to review.", description: "Access reviews you take part in appear here." }} />
  </Stack>;
}

const workspaceField: Field = {
  name: "workspace_ids", label: "Only these Teams and Projects", type: "search", hint: "Optional", placeholder: "Search teams and projects", maxSelections: 50,
  help: "Leave empty to review every Team and Project. Applies to memberships only.",
  visibleWhen: v => (v.subjects ?? "").includes("workspace_memberships"),
  search: { path: `${platformPath}/workspaces?status=active`, multiple: true, emptyText: "No Team or Project matches.", option: ((w: DirectoryWorkspace) => ({ value: w.id, label: w.name })) as never },
};
function parseIds(value: string | undefined): string[] {
  try { const ids = JSON.parse(value || "[]"); return Array.isArray(ids) ? ids.filter((v): v is string => typeof v === "string") : []; } catch { return []; }
}

/** Admin › Access reviews: campaigns, newest first; Platform Admins start one. */
export function PlatformAccessReviewsPage({ session }: { session: Session }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const start = () => ask({
    title: "Start an access review", description: "Takes a snapshot of the access below now. Reviewers decide each item; removals apply at once.",
    fields: [nameField,
      { name: "subjects", label: "What to review", type: "checkboxes", required: true, value: JSON.stringify(["workspace_memberships"]), options: Object.entries(subjectLabels).map(([value, label]) => ({ value, label })) },
      workspaceField,
      { name: "due_in_days", label: "Due in (days)", type: "number", value: "14", required: true, min: 1, max: 90 }],
    submitLabel: "Start review", successNotice: "Access review started.",
    run: (v, signal) => {
      const subjects = parseIds(v.subjects), ids = subjects.includes("workspace_memberships") ? parseIds(v.workspace_ids) : [];
      return api<ReviewCampaignDetail>(reviewsPath, { method: "POST", body: { name: v.name, subjects, workspace_ids: ids, due_in_days: Number(v.due_in_days) }, signal });
    },
  });
  const columns: DataTableColumn<ReviewCampaign>[] = [
    { id: "name", header: "Review", accessor: c => c.name, rowHeader: true, hideable: false, cell: c => <ResourceLink search={{ page: "access-review-detail", record: c.id }}>{c.name}</ResourceLink> },
    { id: "scope", header: "Covers", accessor: c => c.subjects.join(","), cell: c => <CellText primary={c.subjects.map(x => subjectLabels[x]).join(" and ")} secondary={c.workspace_ids.length ? `${c.workspace_ids.length} selected workspaces` : undefined} /> },
    { id: "items", header: "Items", accessor: c => c.item_count, cell: c => c.item_count },
    { id: "due", header: "Due", accessor: c => c.due_at, cell: c => <span className={s.secondary}>{new Date(c.due_at).toLocaleDateString()}</span> },
    { id: "state", header: "Status", accessor: c => campaignState(c), cell: c => <StatusBadge tone={campaignStateTone[campaignState(c)]}>{campaignStateLabels[campaignState(c)]}</StatusBadge> },
  ];
  return <Stack gap={6} className={s.page}>
    <Heading title="Access reviews" description="One-off reviews of who has access to Teams and Projects, and of Admin and Auditor roles. Workspace owners and admins review their workspaces; Platform Admins review platform roles." actions={writable && <Button onClick={start}><Plus aria-hidden />Start review</Button>} />
    <DirectoryTable<ReviewCampaign> path={reviewsPath} label="Access reviews" storageKey="access-reviews" columns={columns} rowKey={c => c.id} rowLabel={c => c.name}
      empty={{ icon: <ClipboardCheck />, title: "No access reviews yet.", description: writable ? "Start one to have owners and admins confirm who should keep access." : "Platform Admins start access reviews." }} />
  </Stack>;
}

/** One campaign: counts, items with their decision and reviewer. */
export function AccessReviewDetail({ session, id }: { session: Session; id: string }) {
  const ask = useAction(), writable = session.capabilities.platform_write;
  const q = useApi<ReviewCampaignDetail>(reviewPath(id), session.capabilities.platform_read);
  useResourceName(q.data?.name);
  if (q.isPending) return <p role="status">Loading access review…</p>;
  if (q.isError) return <ErrorNotice error={q.error} retry={() => void q.refetch()} />;
  if (q.data.id !== id) return <ErrorNotice error={new Error("The returned review does not match the requested record.")} />;
  const c = q.data, state = campaignState(c), open = state !== "closed", n = c.counts;
  const close = () => ask({ title: `Close ${c.name}?`, description: "Reviewers can no longer decide its items. Decisions already made stay.", submitLabel: "Close review", successNotice: "Access review closed.", run: (_, signal) => api(reviewClosePath(id), { method: "POST", signal }) });
  const assign = (i: ReviewItem) => ask({
    title: `Choose who reviews ${personText(i)} (${accessText(i)})`, description: i.subject_kind === "platform_role" ? "A Platform Admin, other than the person under review." : `An owner or admin of ${i.workspace_name ?? "the workspace"}, or a Platform Admin, other than the person under review.`,
    fields: [{ name: "user_id", label: "Reviewer", type: "search", required: true, placeholder: "name@example.com", search: { path: `${platformPath}/users?status=active&role=${i.subject_kind === "platform_role" ? "admin" : "any"}`, option: ((u: DirectoryUser) => u.platform_role && u.email && u.id !== i.user_id ? { value: u.id, label: u.email } : undefined) as never } }],
    submitLabel: "Assign", successNotice: "Reviewer assigned.",
    run: (v, signal) => api<ReviewItem>(reviewerPath(id, i.id), { method: "POST", body: { user_id: v.user_id }, signal }),
  });
  const unassign = (i: ReviewItem) => ask({ title: "Back to the automatic reviewers?", description: "The workspace's owners and admins (or, for platform roles, the Platform Admins) review it again.", submitLabel: "Reset", successNotice: "Reviewer reset.", run: (_, signal) => api<ReviewItem>(reviewerPath(id, i.id), { method: "POST", body: { user_id: null }, signal }) });
  const columns: DataTableColumn<ReviewItem>[] = [
    personColumn<ReviewItem>(),
    accessColumn<ReviewItem>(),
    { id: "decision", header: "Decision", accessor: i => i.decision ?? "", cell: i => <DecisionCell item={i} /> },
    { id: "reviewer", header: "Reviewer", accessor: i => i.active && !i.reviewable ? "none" : i.reviewer_user_id ?? "", cell: i => i.active && !i.reviewable ? <StatusBadge tone="warning">No reviewer</StatusBadge> : i.reviewer_user_id ? <span title="An assignee who can no longer review it is ignored: the automatic reviewers decide again.">Assigned</span> : <span className={s.muted}>Automatic</span> },
  ];
  const summary = `${n.by_decision.pending} undecided · ${n.by_decision.keep} kept · ${n.by_decision.remove} removed · ${n.by_decision.flag_idp} flagged${n.without_reviewer ? ` · ${n.without_reviewer} without a reviewer` : ""}${n.inactive ? ` · ${n.inactive} no longer in effect` : ""}`;
  return <Stack gap={6} className={s.page}>
    <Heading title={c.name} description={`${c.subjects.map(x => subjectLabels[x]).join(" and ")} · ${c.item_count} items · due ${new Date(c.due_at).toLocaleDateString()} · ${summary}`}
      actions={<Inline gap={2}><StatusBadge tone={campaignStateTone[state]}>{campaignStateLabels[state]}</StatusBadge><TextLink href={reviewExportPath(id)} download>Export CSV</TextLink>{writable && open && <Button variant="secondary" onClick={close}>Close review</Button>}</Inline>} />
    <DirectoryTable<ReviewItem> path={reviewItemsPath(id)} label="Items" storageKey="access-review-items" columns={columns} rowKey={i => i.id} rowLabel={itemLabel}
      rowActions={i => !writable || !open || i.user_id === session.user.id ? [] : [
        { label: "Assign reviewer…", hidden: !i.active, onSelect: () => assign(i) },
        { label: "Back to automatic reviewers…", hidden: !i.reviewer_user_id, onSelect: () => unassign(i) },
      ]}
      empty={{ icon: <ClipboardCheck />, title: "No items.", description: "" }} />
  </Stack>;
}

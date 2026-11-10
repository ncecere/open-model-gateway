/*
 * Access review campaigns (#90). A Platform Admin starts a one-shot campaign
 * over shared Team/Project memberships, Admin/Auditor platform roles and
 * the keys of Teams and Projects; reviewers (each workspace's owners and
 * admins, Platform Admins for platform roles, or a reviewer a Platform Admin
 * assigned) keep, remove or flag each item. Keys are reviewed by the
 * workspace's owners and admins only, never their holder, and are never
 * assigned; the platform views carry no key name or holder. The server decides who may review what and applies every
 * removal through the same paths as the direct routes; these types mirror
 * docs/management-api.md#access-reviews-90.
 */
import { platformPath } from "./api";

export type ReviewSubject = "workspace_memberships" | "platform_roles" | "workspace_keys";
export type ReviewDecision = "keep" | "flag_idp" | "remove";
export type ReviewCampaign = {
  id: string; name: string; subjects: ReviewSubject[]; workspace_ids: string[]; created_by: string; created_at: string;
  due_at: string; closed_at: string | null; item_count: number;
};
export type ReviewCounts = {
  by_kind: { workspace_membership: number; platform_role: number; workspace_key: number };
  by_source: { manual: number; group: number; bootstrap: number };
  by_decision: { pending: number; keep: number; flag_idp: number; remove: number };
  inactive: number; without_reviewer: number;
};
export type ReviewCampaignDetail = ReviewCampaign & { counts: ReviewCounts };
export type ReviewItem = {
  id: string; campaign_id: string; subject_kind: "workspace_membership" | "platform_role" | "workspace_key";
  /** Key items: `user_id` is the human holder for reviewers, null in the platform views; no role or source. */
  workspace_id: string | null; workspace_name: string | null; user_id: string | null; email: string | null; display_name: string | null;
  role: "owner" | "admin" | "member" | "auditor" | null; source: "manual" | "group" | "bootstrap" | null; granted_at: string; active: boolean;
  /** Set on key items. The name, service account and holder are null in the platform views. */
  api_key_id: string | null; key_name: string | null; holder_kind: "member" | "service_account" | null;
  service_account_id: string | null; service_account_name: string | null; expires_at: string | null; last_used_at: string | null;
  decision: ReviewDecision | null; decided_by: string | null; decided_at: string | null; reviewer_user_id: string | null;
  /** Whether anyone may decide it now (#469). */
  reviewable: boolean;
};
export type InboxItem = ReviewItem & { campaign_name: string; due_at: string };

export const reviewsPath = `${platformPath}/access-reviews`;
export const reviewPath = (id: string) => `${reviewsPath}/${encodeURIComponent(id)}`;
export const reviewItemsPath = (id: string) => `${reviewPath(id)}/items`;
export const reviewClosePath = (id: string) => `${reviewPath(id)}/close`;
export const reviewExportPath = (id: string) => `${reviewPath(id)}/export`;
export const reviewerPath = (id: string, item: string) => `${reviewPath(id)}/items/${encodeURIComponent(item)}/reviewer`;
export const inboxPath = "/api/v1/me/access-reviews";
export const decisionPath = (item: string) => `${inboxPath}/${encodeURIComponent(item)}/decision`;

export const subjectLabels: Record<ReviewSubject, string> = { workspace_memberships: "Team and Project memberships", platform_roles: "Admin and Auditor roles", workspace_keys: "Team and Project keys" };
/** "A", "A and B", "A, B and C". */
export const subjectsText = (subjects: ReviewSubject[]) => { const l = subjects.map(x => subjectLabels[x]); return l.length < 2 ? l.join("") : `${l.slice(0, -1).join(", ")} and ${l[l.length - 1]}`; };
export const decisionLabels: Record<ReviewDecision, string> = { keep: "Kept", flag_idp: "Flagged for the identity provider", remove: "Removed" };
export const decisionTone: Record<ReviewDecision, "success" | "warning" | "danger"> = { keep: "success", flag_idp: "warning", remove: "danger" };
export const sourceLabels: Record<NonNullable<ReviewItem["source"]>, string> = { manual: "Manual", group: "Identity provider group", bootstrap: "Bootstrap" };
const roleLabels: Record<NonNullable<ReviewItem["role"]>, string> = { owner: "Owner", admin: "Admin", member: "Member", auditor: "Auditor" };

/** What the access is: "Owner of Product" or "Platform Admin". */
const roleText = (i: ReviewItem) => i.role ? roleLabels[i.role] : "Access";
export const isKey = (i: ReviewItem) => i.subject_kind === "workspace_key";
/**
 * A key's name, as reviewers see it. The platform views carry no name, so
 * there a key is told apart by its kind and the start of its id.
 */
export const keyText = (i: ReviewItem) => i.key_name || `${i.holder_kind === "service_account" ? "Service account key" : "Member's key"} ${(i.api_key_id ?? "").slice(0, 8)}`.trim();
/** Who holds a key, when the viewer may know: the service account, or a holder who still belongs to the workspace. */
export function keyHolderText(i: ReviewItem): string | undefined {
  if (i.service_account_name) return `Service account ${i.service_account_name}`;
  const holder = i.display_name || i.email;
  if (holder) return `Held by ${holder}`;
  return i.key_name ? (i.holder_kind === "service_account" ? "Service account key" : "Member's key") : undefined;
}
/** A key's expiry and last use: what a reviewer weighs. */
export function keyUseText(i: ReviewItem): string {
  const day = (v: string) => new Date(v).toLocaleDateString();
  return `${i.expires_at ? `Expires ${day(i.expires_at)}` : "No expiry"} · ${i.last_used_at ? `Last used ${day(i.last_used_at)}` : "No recorded use"}`;
}
export function accessText(i: ReviewItem): string {
  if (isKey(i)) return `Key in ${i.workspace_name ?? "a workspace"}`;
  return i.subject_kind === "platform_role" ? `Platform ${roleText(i)}` : `${roleText(i)} of ${i.workspace_name ?? "a workspace"}`;
}
/** The row's subject: the person, or the key. */
export const personText = (i: ReviewItem) => isKey(i) ? keyText(i) : i.display_name || i.email || "Removed person";
/** What a decision reads as: a removed key is revoked. */
export const decisionText = (i: ReviewItem, d: ReviewDecision) => isKey(i) && d === "remove" ? "Revoked" : decisionLabels[d];
/** A row's accessible name: the person and the access; a key adds its holder, since key names repeat. */
export const itemLabel = (i: ReviewItem) => [personText(i), isKey(i) ? keyHolderText(i) : undefined, accessText(i)].filter(Boolean).join(", ");
/** Which key a confirmation is about: key names are not unique, so its holder and the start of its id (the key as it was when the review started: a rotation gives its replacement a new id). */
export const keyIdentity = (i: ReviewItem) => `${keyHolderText(i) ?? "Key"}, ID ${(i.api_key_id ?? "").slice(0, 8)}.`;
/** "Blair from Product" or "Aud's Platform Auditor role". */
export const removalTarget = (i: ReviewItem) => isKey(i) ? `${keyText(i)} in ${i.workspace_name ?? "this workspace"}` : i.subject_kind === "platform_role" ? `${personText(i)}'s ${accessText(i)} role` : `${personText(i)} from ${i.workspace_name ?? "this workspace"}`;
export type CampaignState = "open" | "overdue" | "closed";
export function campaignState(c: ReviewCampaign, now = Date.now()): CampaignState {
  return c.closed_at ? "closed" : Date.parse(c.due_at) <= now ? "overdue" : "open";
}
export const campaignStateLabels: Record<CampaignState, string> = { open: "Open", overdue: "Overdue", closed: "Closed" };
export const campaignStateTone: Record<CampaignState, "info" | "danger" | "neutral"> = { open: "info", overdue: "danger", closed: "neutral" };

/**
 * What a removal does, said before it is confirmed: the server removes the
 * access at once through the same path as removing it directly.
 */
export function removalConsequence(i: ReviewItem): string {
  if (isKey(i)) return keyIdentity(i) + " Revokes this key now; if it was rotated after the review started, the key that replaced it is revoked. New requests that use it are refused at once. A revoked key never returns: whoever needs access creates a new key.";
  return i.subject_kind === "platform_role"
    ? `Removes this ${roleText(i)} grant (manual or bootstrap) now. The same role from an identity provider group, if they have one, stays. If they then hold no other platform role, they lose access to the gateway at once: they are suspended, their sessions and keys are revoked (revoked keys never return), and their personal data is removed after 30 days unless they are reinstated first.`
    : `Removes their membership in ${i.workspace_name ?? "this workspace"} now. Their keys there are revoked, permanently, if they no longer belong to it through an identity provider group.`;
}

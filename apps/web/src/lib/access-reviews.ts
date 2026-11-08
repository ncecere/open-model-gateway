/*
 * Access review campaigns (#90). A Platform Admin starts a one-shot campaign
 * over shared Team/Project memberships and Admin/Auditor platform roles;
 * reviewers (each workspace's owners and admins, Platform Admins for
 * platform roles, or a reviewer a Platform Admin assigned) keep, remove or
 * flag each item. The server decides who may review what and applies every
 * removal through the same paths as the direct routes; these types mirror
 * docs/management-api.md#access-reviews-90.
 */
import { platformPath } from "./api";

export type ReviewSubject = "workspace_memberships" | "platform_roles";
export type ReviewDecision = "keep" | "flag_idp" | "remove";
export type ReviewCampaign = {
  id: string; name: string; subjects: ReviewSubject[]; workspace_ids: string[]; created_by: string; created_at: string;
  due_at: string; closed_at: string | null; item_count: number;
};
export type ReviewCounts = {
  by_kind: { workspace_membership: number; platform_role: number };
  by_source: { manual: number; group: number; bootstrap: number };
  by_decision: { pending: number; keep: number; flag_idp: number; remove: number };
  inactive: number; without_reviewer: number;
};
export type ReviewCampaignDetail = ReviewCampaign & { counts: ReviewCounts };
export type ReviewItem = {
  id: string; campaign_id: string; subject_kind: "workspace_membership" | "platform_role";
  workspace_id: string | null; workspace_name: string | null; user_id: string; email: string | null; display_name: string | null;
  role: "owner" | "admin" | "member" | "auditor"; source: "manual" | "group" | "bootstrap"; granted_at: string; active: boolean;
  decision: ReviewDecision | null; decided_by: string | null; decided_at: string | null; reviewer_user_id: string | null;
};
export type InboxItem = ReviewItem & { campaign_name: string; due_at: string };

export const reviewsPath = `${platformPath}/access-reviews`;
export const reviewPath = (id: string) => `${reviewsPath}/${encodeURIComponent(id)}`;
export const reviewItemsPath = (id: string) => `${reviewPath(id)}/items`;
export const reviewClosePath = (id: string) => `${reviewPath(id)}/close`;
export const reviewerPath = (id: string, item: string) => `${reviewPath(id)}/items/${encodeURIComponent(item)}/reviewer`;
export const inboxPath = "/api/v1/me/access-reviews";
export const decisionPath = (item: string) => `${inboxPath}/${encodeURIComponent(item)}/decision`;

export const subjectLabels: Record<ReviewSubject, string> = { workspace_memberships: "Team and Project memberships", platform_roles: "Admin and Auditor roles" };
export const decisionLabels: Record<ReviewDecision, string> = { keep: "Kept", flag_idp: "Flagged for the identity provider", remove: "Removed" };
export const decisionTone: Record<ReviewDecision, "success" | "warning" | "danger"> = { keep: "success", flag_idp: "warning", remove: "danger" };
export const sourceLabels: Record<ReviewItem["source"], string> = { manual: "Manual", group: "Identity provider group", bootstrap: "Bootstrap" };
const roleLabels: Record<ReviewItem["role"], string> = { owner: "Owner", admin: "Admin", member: "Member", auditor: "Auditor" };

/** What the access is: "Owner of Product" or "Platform Admin". */
export function accessText(i: ReviewItem): string {
  return i.subject_kind === "platform_role" ? `Platform ${roleLabels[i.role]}` : `${roleLabels[i.role]} of ${i.workspace_name ?? "a workspace"}`;
}
export const personText = (i: ReviewItem) => i.display_name || i.email || "Removed person";
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
  return i.subject_kind === "platform_role"
    ? `Removes this ${roleLabels[i.role]} role now. If it is their only platform role, they lose access to the gateway: they are suspended and their sessions and keys are revoked.`
    : `Removes their membership in ${i.workspace_name ?? "this workspace"} now. Their keys there are revoked if they no longer belong to it through an identity provider group.`;
}

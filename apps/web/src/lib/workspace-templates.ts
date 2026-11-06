/*
 * Reviewed onboarding templates for Teams and Projects (#84). Platform Admins
 * author templates, another Platform Admin reviews a version before it can
 * be published, and a published version creates a workspace in one
 * transaction. The server validates everything and stays authoritative;
 * these types mirror docs/management-api.md#onboarding-templates-84.
 */
import { platformPath } from "./api";

export type TemplateSlot = { name: string; role: "admin" | "member"; required: boolean; multiple: boolean };
export type TemplateContent = {
  kind: "team" | "project";
  catalogs: { mode: "inherit" } | { mode: "replace"; catalog_ids: string[] };
  model_ids: string[];
  limits: Record<string, unknown> | null;
  owner: { name: string };
  slots: TemplateSlot[];
};
export type TemplateVersionState = "pending_review" | "reviewed" | "published" | "retired";
export type TemplateVersion = {
  version: number; state: TemplateVersionState; content: TemplateContent; content_hash: string;
  author: string; created_at: string; reviewed_by: string | null; reviewed_at: string | null; published_at: string | null; retired_at: string | null;
};
export type WorkspaceTemplate = { id: string; name: string; created_by: string; created_at: string; versions: TemplateVersion[] };
export type WorkspaceTemplateSummary = { id: string; name: string; created_at: string; latest_version: number; published_version: number | null };
export type TemplateApplication = { workspace_id: string; template_id: string; version: number; replayed: boolean };

export const templatesPath = `${platformPath}/workspace-templates`;
export const templatePath = (id: string) => `${templatesPath}/${encodeURIComponent(id)}`;
export const versionsPath = (id: string) => `${templatePath(id)}/versions`;
export const versionStepPath = (id: string, version: number, step: "review" | "publish" | "retire") => `${versionsPath(id)}/${version}/${step}`;
export const applyPath = (id: string) => `${templatePath(id)}/apply`;

export const versionStateLabels: Record<TemplateVersionState, string> = { pending_review: "Waiting for review", reviewed: "Reviewed", published: "Published", retired: "Retired" };
export const versionStateTone: Record<TemplateVersionState, "warning" | "info" | "success" | "neutral"> = { pending_review: "warning", reviewed: "info", published: "success", retired: "neutral" };

/** The starting content for a new template: inherit catalogs, one owner slot. */
export const exampleContent: TemplateContent = { kind: "team", catalogs: { mode: "inherit" }, model_ids: [], limits: null, owner: { name: "owner" }, slots: [{ name: "members", role: "member", required: false, multiple: true }] };

/** Parses the content field; the server validates the references and rules. */
export function parseContent(text: string): TemplateContent {
  let value: unknown;
  try { value = JSON.parse(text); } catch { throw new Error("The content is not valid JSON."); }
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("The content must be a JSON object.");
  return value as TemplateContent;
}
export function contentError(text: string): string | undefined {
  try { parseContent(text); return undefined; } catch (e) { return (e as Error).message; }
}

/** A plain summary of what a version creates, from its content. */
export function describeContent(c: TemplateContent): string[] {
  const kind = c.kind === "project" ? "Project" : "Team";
  const catalogs = c.catalogs.mode === "inherit" ? `${kind} default catalogs` : `${c.catalogs.catalog_ids.length} catalog${c.catalogs.catalog_ids.length === 1 ? "" : "s"} (replacing the defaults)`;
  const models = c.model_ids.length ? `${c.model_ids.length} model${c.model_ids.length === 1 ? "" : "s"} added` : "No models added";
  const limits = c.limits ? "Local limits set" : "No local limits";
  const slots = [`${c.owner.name} (owner)`, ...c.slots.map(s => `${s.name} (${s.role}${s.required ? ", required" : ""}${s.multiple ? ", several" : ""})`)].join(", ");
  return [kind, catalogs, models, limits, `Slots: ${slots}`];
}

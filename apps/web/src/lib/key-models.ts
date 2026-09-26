import type { Grant, Key } from "./api";
import { checkboxValues, type Field, type Option, type Values } from "./forms";

export const keyModelRestrictionHelp = "Model access is fixed at creation. To change it, create a replacement key and revoke the old one. Parent grants always apply; disabled models or deployments cannot serve inference.";
export const keyRotationDescription = "The old key will be revoked immediately. Update all clients to use the new key. Rotation retains model restrictions and budget consumption, including across repeated rotations. To change model access, create a replacement key and revoke the old one instead.";

export function keyModelOptions(grants: Grant[]): Option[] {
  // Use only effective grants, including authorized owner-personal grants. Do
  // not filter disabled models: inference, not this restriction, gates serving.
  return [...new Map(grants.map((grant) => [grant.model_id, { value: grant.model_id, label: `${grant.display_name} (${grant.public_name})` }])).values()];
}
export function keyModelFields(options: Option[]): Field[] {
  return [
    { name: "model_access", label: "Model access", type: "select", required: true, value: "inherit", help: keyModelRestrictionHelp, options: [
      { value: "inherit", label: "Inherit workspace access" },
      { value: "selected", label: "Selected models" },
      { value: "none", label: "No models" },
    ] },
    { name: "model_ids", label: "Models", type: "checkboxes", required: true, value: "[]", maxSelections: 200, options, visibleWhen: (values) => values.model_access === "selected", help: options.length ? "Select 1–200 granted models. This list reflects current effective grants, not deployment availability." : "No effective model grants are available. Choose Inherit workspace access or No models instead." },
  ];
}
export function keyModelBody(values: Values, options: Option[]): { model_ids: string[] | null } {
  // Hidden stale selections must never override the explicit mode.
  if (values.model_access === "inherit") return { model_ids: null };
  if (values.model_access === "none") return { model_ids: [] };
  if (values.model_access !== "selected") throw new Error("Choose an available model access mode.");
  return { model_ids: checkboxValues(keyModelFields(options)[1], values.model_ids) };
}
export function keyModelSummary(key: Pick<Key, "model_ids">, options: Option[] = []): { label: string; models: string[] } {
  if (key.model_ids == null) return { label: "Inherited", models: [] };
  if (!key.model_ids.length) return { label: "No models", models: [] };
  const labels = new Map(options.map((option) => [option.value, option.label]));
  return { label: `${key.model_ids.length} selected`, models: key.model_ids.map((id) => labels.get(id) ?? id) };
}

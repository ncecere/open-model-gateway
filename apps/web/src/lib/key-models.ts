import type { Grant, Key } from "./api";
import { checkboxValues, type Field, type Option, type Values } from "./forms";

export const keyModelRestrictionHelp = "You can't change a key's models later. Create a new key instead.";
export const keyRotationDescription = "A new secret replaces this one, and the old secret stops working immediately. Models, limits and usage carry over.";

export function keyModelOptions(grants: Grant[]): Option[] {
  // Use only effective grants, including authorized owner-personal grants. Do
  // not filter disabled models: inference, not this restriction, gates serving.
  return [...new Map(grants.map((grant) => [grant.model_id, { value: grant.model_id, label: `${grant.display_name} (${grant.public_name})` }])).values()];
}
export function keyModelFields(options: Option[]): Field[] {
  return [
    { name: "model_access", label: "Models", type: "select", required: true, value: "inherit", help: keyModelRestrictionHelp, options: [
      { value: "inherit", label: "All models in this workspace (updates as models change)" },
      { value: "selected", label: "Only selected models" },
      { value: "none", label: "No models" },
    ] },
    { name: "model_ids", label: "Models", type: "checkboxes", required: true, value: "[]", maxSelections: 200, options, visibleWhen: (values) => values.model_access === "selected", help: options.length ? "Select 1–200 models. This list reflects the workspace's current models, not provider availability." : "This workspace has no models yet. Add models first." },
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
  if (key.model_ids == null) return { label: "All workspace models", models: [] };
  if (!key.model_ids.length) return { label: "No models", models: [] };
  const labels = new Map(options.map((option) => [option.value, option.label]));
  return { label: `${key.model_ids.length} selected`, models: key.model_ids.map((id) => labels.get(id) ?? id) };
}
/** Create-key model selection: everything in the workspace (null) or 1–200 chosen models. */
export function selectedModelIds(mode: "inherit" | "selected", selected: string[], options: Option[]): { model_ids: string[] | null } | { error: string } {
  if (mode === "inherit") return { model_ids: null };
  const allowed = new Set(options.map(o => o.value)), ids = [...new Set(selected)].filter(id => allowed.has(id));
  if (!ids.length) return { error: "Choose at least one model." };
  if (ids.length > 200) return { error: "Choose at most 200 models." };
  return { model_ids: ids };
}

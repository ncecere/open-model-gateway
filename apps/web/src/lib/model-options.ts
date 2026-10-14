/*
 * Choosing platform models (R06, #68): searched on the server, never a whole
 * list of every model. A selected model's label comes from its own record.
 */
import { platformPath, type Model } from "./api";
import type { Field } from "./forms";

/** A platform model's choice label: "Display · api-name", marked when disabled. */
export const modelOptionLabel = (m: Pick<Model, "display_name" | "public_name" | "enabled">) => `${m.display_name} · ${m.public_name}${m.enabled ? "" : " · disabled"}`;
/**
 * Several platform models in an ActionDialog (a JSON array of ids). `exclude` is a server filter
 * (`not_in_catalog=…` or `not_direct_in=…`) so the 20 matches are never all models already added;
 * `skip` also drops them in the browser while that list refreshes.
 */
export const modelSearchField = (name: string, label: string, skip: ReadonlySet<string>, exclude: string): Field => ({ name, label, type: "search", required: true, maxSelections: 200, placeholder: "Search models", search: { path: `${platformPath}/models?${exclude}`, multiple: true, emptyText: "No models match.", option: ((m: Model) => skip.has(m.id) ? undefined : { value: m.id, label: modelOptionLabel(m) }) as never } });
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/** One platform model in an ActionDialog (its id is the value; anything but a model id is refused before sending). */
export const modelPickField = (name: string, label: string): Field => ({ name, label, type: "search", required: true, placeholder: "Search models", validate: v => uuid.test(v) ? undefined : "Choose a model from the search.", search: { path: `${platformPath}/models`, emptyText: "No models match.", option: ((m: Model) => ({ value: m.id, label: modelOptionLabel(m) })) as never } });

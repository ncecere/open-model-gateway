import { describe, expect, it } from "vitest";
import { keyModelBody, keyModelFields, keyModelOptions, keyModelSummary, keyRotationDescription, selectedModelIds } from "./key-models";
import { parseCheckboxValues, validateFields } from "./forms";

const id = (n: number) => `00000000-0000-0000-0000-${String(n).padStart(12, "0")}`;
const options = Array.from({ length: 201 }, (_, n) => ({ value: id(n), label: `Model ${n}` }));
const fields = keyModelFields(options);
const values = (selected: unknown, mode = "selected") => ({ model_access: mode, model_ids: JSON.stringify(selected) });

describe("key model restriction creation", () => {
  it("defaults to inherited workspace access", () => {
    expect(fields[0].value).toBe("inherit");
    expect(fields[0].options?.map((option) => option.label)).toEqual(["All models in this workspace (updates as models change)", "Only selected models", "No models"]);
    expect(keyModelBody(values([], "inherit"), options)).toEqual({ model_ids: null });
  });
  it("ignores hidden stale or malformed selections for inherit and explicit deny-all", () => {
    for (const mode of ["inherit", "none"]) {
      for (const model_ids of ['["foreign"]', "invalid json", JSON.stringify(options.map((o) => o.value))]) {
        const input = { model_access: mode, model_ids };
        expect(validateFields(fields, input)).toEqual({});
        expect(keyModelBody(input, options)).toEqual({ model_ids: mode === "inherit" ? null : [] });
      }
    }
  });
  it("submits selected grant UUIDs, not names or aliases", () => {
    expect(keyModelBody(values([id(1), id(2)]), options)).toEqual({ model_ids: [id(1), id(2)] });
    expect(validateFields(fields, values([id(1)]))).toEqual({});
    expect(() => keyModelBody(values(["Model 1"]), options)).toThrow("available");
  });
  it.each(["", "undefined", "null", "{}", "1", '"abc"', '[1]', '[true]', '[null]', '[[]]', '["a",]', '["a","a"]'])("strictly rejects malformed/non-string/duplicate selections: %s", (model_ids) => {
    expect(() => parseCheckboxValues(model_ids)).toThrow();
    expect(validateFields(fields, { model_access: "selected", model_ids })).toHaveProperty("model_ids");
    expect(() => keyModelBody({ model_access: "selected", model_ids }, options)).toThrow();
  });
  it("requires nonempty offered options, rejects duplicates, and enforces the 200 bound", () => {
    for (const selected of [[], [id(1), id(1)], ["foreign"], options.map((o) => o.value)]) {
      expect(validateFields(fields, values(selected))).toHaveProperty("model_ids");
      expect(() => keyModelBody(values(selected), options)).toThrow();
    }
    const twoHundred = values(options.slice(0, 200).map((o) => o.value));
    expect(validateFields(fields, twoHundred)).toEqual({});
    expect(keyModelBody(twoHundred, options).model_ids).toHaveLength(200);
    expect(() => keyModelBody(values([id(1)], "unexpected"), options)).toThrow();
  });
  it("rejects absent/unknown modes and selections when there are no effective grants", () => {
    for (const model_access of ["", "unexpected"]) {
      const input = { model_access, model_ids: "[]" };
      expect(validateFields(fields, input)).toHaveProperty("model_access");
      expect(() => keyModelBody(input, options)).toThrow();
    }
    expect(validateFields(fields, { model_access: "selected" })).toHaveProperty("model_ids");
    expect(() => keyModelBody({ model_access: "selected" }, options)).toThrow();
    for (const selection of [[], [id(1)]]) {
      expect(validateFields(keyModelFields([]), values(selection))).toHaveProperty("model_ids");
      expect(() => keyModelBody(values(selection), [])).toThrow();
    }
    expect(keyModelFields([])[1].help).toContain("no models yet");
  });
  it("retains all effective granted choices irrespective of model availability and deduplicates IDs", () => {
    const grant = { model_id: id(1), display_name: "Disabled model", public_name: "test/disabled", enabled: false, selected: true, catalog_granted: false, direct_granted: true, available_from_catalog: false, supported_protocols: ["chat_completions" as const] };
    expect(keyModelOptions([grant, grant])).toEqual([{ value: id(1), label: "Disabled model (test/disabled)" }]);
    expect(keyModelBody(values([id(1)]), keyModelOptions([grant]))).toEqual({ model_ids: [id(1)] });
  });
  it("explains immutable restrictions and budget consumption across rotations", () => {
    expect(keyRotationDescription).toContain("old secret stops working immediately");
    expect(keyRotationDescription).toContain("Models, limits and usage carry over");
  });
});
describe("key model restriction display", () => {
  it("treats missing fixture fields as inherited, without conflating deny-all", () => {
    expect(keyModelSummary({}).label).toBe("All workspace models");
    expect(keyModelSummary({ model_ids: null }).label).toBe("All workspace models");
    expect(keyModelSummary({ model_ids: [] }).label).toBe("No models");
  });
  it("create dialog selection: all models (null) or 1-200 offered models, never an empty or foreign list", () => {
    expect(selectedModelIds("inherit", ["foreign"], options)).toEqual({ model_ids: null });
    expect(selectedModelIds("selected", [], options)).toHaveProperty("error");
    expect(selectedModelIds("selected", ["foreign"], options)).toHaveProperty("error");
    expect(selectedModelIds("selected", [id(1), id(1), "foreign"], options)).toEqual({ model_ids: [id(1)] });
    expect(selectedModelIds("selected", options.map(o => o.value), options)).toHaveProperty("error");
  });
  it("keeps removed grants visible by UUID when their labels are unavailable", () => {
    expect(keyModelSummary({ model_ids: [id(1), "removed-model"] }, options)).toEqual({ label: "2 selected", models: ["Model 1", "removed-model"] });
  });
});

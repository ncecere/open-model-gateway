/*
 * PresetChoice: pick a preset (No limit · $10 · $50 · $100, or 7 · 30 · 90
 * days) or "Custom", which reveals a labelled text input in place. Built on
 * Bitop RadioGroup (fieldset + legend, real radios, arrow keys) and Field +
 * Input. Controlled with an explicit union so a custom value equal to a
 * preset never flips the mode; the custom text is passed through as typed
 * (validate/convert it with the existing helpers, e.g. dollarsToMicroUsd).
 *
 *   <PresetChoice legend="Spending limit" presets={[{ value: "", label: "No limit" }, { value: "10", label: "$10" }]}
 *     value={limit} onChange={setLimit} custom={{ label: "Custom limit (USD)", prefix: "$", inputMode: "decimal" }} />
 */
import type { ReactNode } from "react";
import { Field } from "../ui/field/field";
import { Input } from "../ui/input/input";
import { RadioGroup } from "../ui/radio-group/radio-group";
import { cx } from "../../lib/bitop-utils";
import styles from "./preset-choice.module.css";

export type PresetOption = { value: string; label: ReactNode; description?: ReactNode; disabled?: boolean };
export type PresetValue = { kind: "preset"; preset: string } | { kind: "custom"; text: string };

export type PresetChoiceProps = {
  legend: ReactNode;
  description?: ReactNode;
  presets: PresetOption[];
  value: PresetValue;
  onChange: (value: PresetValue) => void;
  /** The Custom option and its input. Omit to offer presets only. */
  custom?: {
    /** Option text (default "Custom"). */
    optionLabel?: ReactNode;
    /** Input label, e.g. "Custom limit (USD)". */
    label: string;
    /** Decorative text before the value, e.g. "$". */
    prefix?: string;
    placeholder?: string;
    inputMode?: "decimal" | "numeric" | "text";
    maxLength?: number;
    description?: ReactNode;
    error?: ReactNode;
  };
  name?: string;
  disabled?: boolean;
  error?: ReactNode;
  className?: string;
};

const CUSTOM = "__custom__";

/** The chosen preset value or the custom text. */
export function presetChoiceText(value: PresetValue): string {
  return value.kind === "preset" ? value.preset : value.text;
}

export function PresetChoice({ legend, description, presets, value, onChange, custom, name, disabled, error, className }: PresetChoiceProps) {
  const options = [...presets.map(p => ({ ...p, disabled: disabled || p.disabled })), ...(custom ? [{ value: CUSTOM, label: custom.optionLabel ?? "Custom", disabled }] : [])];
  const selected = value.kind === "custom" ? CUSTOM : value.preset;
  return (
    <div className={cx(styles.root, className)}>
      <RadioGroup
        legend={legend}
        description={description}
        name={name}
        options={options}
        orientation="horizontal"
        value={selected}
        error={error}
        onValueChange={next => onChange(next === CUSTOM ? { kind: "custom", text: value.kind === "custom" ? value.text : "" } : { kind: "preset", preset: next })}
      />
      {custom && value.kind === "custom" && (
        <Field label={custom.label} description={custom.description} error={custom.error} className={styles.custom}>
          <Input
            value={value.text}
            disabled={disabled}
            inputMode={custom.inputMode}
            maxLength={custom.maxLength}
            placeholder={custom.placeholder}
            startIcon={custom.prefix ? <span className={styles.prefix}>{custom.prefix}</span> : undefined}
            autoComplete="off"
            onValueChange={text => onChange({ kind: "custom", text })}
          />
        </Field>
      )}
    </div>
  );
}

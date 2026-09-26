export type Values = Record<string, string>;
export type Option = { value: string; label: string };
export type Field = { name: string; label: string; type?: "text" | "email" | "number" | "select" | "password" | "textarea"; inputMode?: "decimal" | "numeric"; required?: boolean; value?: string; options?: Option[]; help?: string; placeholder?: string; min?: number; max?: number; maxLength?: number; visibleWhen?: (values: Values) => boolean; validate?: (value: string, values: Values) => string | undefined };
export const uuidError = (value: string) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value) ? undefined : "Enter a valid user UUID.";
export const slugError = (value: string) => /^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(value) ? undefined : "Use lowercase letters, numbers, and single hyphens.";
export const expiryField: Field = { name: "expires_in_days", label: "Expires in (days)", type: "number", value: "30", required: true, min: 1, max: 365, help: "Keys must expire within 1–365 days." };
export const nameField: Field = { name: "name", label: "Name", required: true, maxLength: 120 };
export const roleOptions: Option[] = ["member", "admin", "owner"].map((role) => ({ value: role, label: role[0].toUpperCase() + role.slice(1) }));
export const roleField: Field = { name: "role", label: "Role", type: "select", value: "member", required: true, options: roleOptions };
export const enabledField: Field = { name: "enabled", label: "Status", type: "select", value: "true", required: true, options: [{ value: "true", label: "Enabled" }, { value: "false", label: "Disabled" }] };
export const disabledField: Field = { name: "disabled", label: "Membership status", type: "select", required: true, options: [{ value: "false", label: "Active" }, { value: "true", label: "Disabled" }] };
export function validateFields(fields: Field[], values: Values): Record<string, string> {
  const errors: Record<string, string> = {};
  for (const field of fields) {
    if (field.visibleWhen && !field.visibleWhen(values)) continue;
    const value = (values[field.name] ?? "").trim();
    if (!value) { if (field.required) errors[field.name] = `${field.label} is required.`; continue; }
    if (field.type !== "textarea" && /[\u0000-\u001f\u007f]/.test(value)) errors[field.name] = "Control characters are not allowed.";
    if (field.maxLength && new TextEncoder().encode(value).length > field.maxLength) errors[field.name] = `Use at most ${field.maxLength} UTF-8 bytes (ASCII characters use one byte).`;
    if (field.type === "email" && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value)) errors[field.name] = "Enter a valid email address.";
    if (field.type === "number" && (!Number.isInteger(Number(value)) || (field.min !== undefined && Number(value) < field.min) || (field.max !== undefined && Number(value) > field.max))) errors[field.name] = `Enter a whole number from ${field.min ?? 0} to ${field.max ?? "the maximum"}.`;
    if (field.type === "select" && !field.options?.some((option) => option.value === value)) errors[field.name] = "Choose an available option.";
    const custom = field.validate?.(value, values);
    if (custom) errors[field.name] = custom;
  }
  return errors;
}

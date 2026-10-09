/*
 * Admin › Settings: API shapes and client-side checks that mirror the server's
 * strict validation (the server stays authoritative). See docs/settings.md.
 */
import { API } from "./api";

export const settingsPath = `${API}/platform/settings`;

export type GeneralSettings = { display_name: string; support_url: string | null; logo_url: string | null; human_key_max_lifetime_days: number; timezone: "UTC"; updated_at: string; updated_by: string | null };
export type LockedValue<T> = { value: T; stored: T; locked: boolean; source: "installation" | "environment"; variable: string };
export type PrivacySettings = {
  openrouter_data_collection: LockedValue<"deny" | "allow">;
  request_log_retention_days: LockedValue<number | null> & { minimum: number; maximum: number };
  prompt_response_storage: "never_stored";
  updated_at: string;
};
export type TlsMode = "starttls" | "implicit" | "none";
export type DeliveryError = "credential" | "address" | "connection" | "tls" | "authentication" | "rejected" | "timeout";
export type EmailSettings = {
  configured: boolean; status: "not_configured" | "credential_unavailable" | "ready";
  host: string | null; port: number | null; tls: TlsMode | null; username: string | null; password_ref: string | null; password_ref_allowed: boolean | null;
  from_address: string | null; from_name: string | null; public_url_configured: boolean;
  last_test: { at: string; ok: boolean; error: DeliveryError | null } | null; updated_at: string;
};
export type EmailTestResult = { ok: boolean; error: DeliveryError | null; recipient: string };
/** Issuer signing keys (JWKS) cached by the server: refreshed on expiry or unknown key id. */
export type JwksStatus = { keys: number; refreshed_at: string; fresh_until: string; last_failure_at: string | null; state: "fresh" | "stale" | "unavailable" };
/** SCIM provisioning status; never the token. */
export type ScimStatus = { enabled: false } | { enabled: true; base_url: string; users: number; active_users: number; groups: number; memberships: number; last_sync_at: string | null };
export type SignInSettings = { enabled: boolean; issuer?: string; client_id?: string; client_type?: "confidential" | "public"; groups_claim?: string; public_url?: string; callback_url?: string; secure_cookies?: boolean; jwks?: JwksStatus; enabled_group_mappings: number; scim?: ScimStatus };

/** Data & privacy › Storage (docs/file-storage.md). Location is host-only; never paths or credentials. */
export type StorageGroupId = "batch" | "video" | "user_files" | "export" | "branding";
export type StorageGroup = {
  group: StorageGroupId; label: string; purposes: string[]; holds_customer_content: boolean; toggle: boolean;
  enabled: boolean; active: boolean; retention_days: number | null; retention_editable: boolean; default_retention_days: number | null;
  minimum: number; maximum: number; objects: number; bytes: number;
};
export type StorageLocation =
  | { kind: "local" | "memory" }
  | { kind: "s3"; bucket: string; region: string; endpoint_host: string | null; endpoint_tls: boolean; path_style: boolean; prefix_set: boolean; auth: "aws_default" | "aws_profile" | "aws_role" | "static" };
export type StorageError = "not_found" | "too_large" | "integrity" | "key_unavailable" | "invalid_key" | "source" | "unavailable" | "timeout" | "denied" | "unsafe_path" | "disabled";
export type StorageSettings = {
  backend: "off" | "local" | "s3" | "memory"; location: StorageLocation | null;
  encryption: { key_id: string; decrypt_only_keys: number } | null;
  health: { checked_at: string; ok: boolean; error: StorageError | null; current: boolean } | null;
  groups: StorageGroup[]; updated_at: string;
};
export type StorageTestResult = { ok: boolean; error: StorageError | null; backend: string; round_trip_ms: number | null };
export const storageErrors: Record<StorageError, string> = {
  not_found: "The test object disappeared.", too_large: "The test object was too large.", integrity: "The stored test object didn't match.",
  key_unavailable: "The encryption key isn't configured.", invalid_key: "Invalid object key.", source: "The upload failed.",
  unavailable: "Couldn't reach the store. Check the bucket, endpoint and network.", timeout: "The store didn't answer in time.",
  denied: "The store refused the credentials or permissions.", unsafe_path: "The storage directory contains a symlink or unsafe path.", disabled: "No file store is configured.",
};
export function storageBackendLabel(s: StorageSettings): string {
  if (s.backend === "off") return "Off";
  if (s.backend === "local") return "Local disk";
  if (s.backend === "memory") return "Memory (tests)";
  return s.location?.kind === "s3" && s.location.endpoint_host ? "S3-compatible" : "Amazon S3";
}
export type StorageDraft = Record<StorageGroupId, { enabled: boolean; days: string }>;
export const storageDraft = (s: StorageSettings): StorageDraft => Object.fromEntries(s.groups.map(g => [g.group, { enabled: g.enabled, days: g.retention_days === null ? "" : String(g.retention_days) }])) as StorageDraft;
export function storageErrorsOf(s: StorageSettings, d: StorageDraft): Partial<Record<StorageGroupId, string>> {
  const out: Partial<Record<StorageGroupId, string>> = {};
  for (const g of s.groups) if (g.retention_editable) { const e = daysError(d[g.group].days, g.minimum, g.maximum, "the retention"); if (e) out[g.group] = e; }
  return out;
}
export const storageBody = (d: StorageDraft) => ({
  batch: { enabled: d.batch.enabled, retention_days: Number(d.batch.days) }, video: { enabled: d.video.enabled, retention_days: Number(d.video.days) },
  user_files: { enabled: d.user_files.enabled, retention_days: Number(d.user_files.days) }, export: { retention_days: Number(d.export.days) },
});

export const KEY_DAYS = { min: 1, max: 365 } as const;
export const tlsLabels: Record<TlsMode, string> = { starttls: "STARTTLS", implicit: "Implicit TLS", none: "None (this machine only)" };
export const tlsPorts: Record<TlsMode, number> = { starttls: 587, implicit: 465, none: 25 };
export const deliveryErrors: Record<DeliveryError, string> = {
  credential: "The password reference can't be used: its variable isn't set or isn't on the server allowlist.",
  address: "The From address or your email address isn't valid.",
  connection: "Couldn't connect to the relay. Check the host and port.",
  tls: "The secure connection failed. Check the TLS mode and port.",
  authentication: "The relay rejected the username or password.",
  rejected: "The relay refused the message.",
  timeout: "The relay didn't answer in time.",
};

const control = /[\u0000-\u001f\u007f]/;
/** Absolute https URL without credentials or fragment; empty is allowed (none). */
export function httpsUrlError(value: string): string | undefined {
  const v = value.trim(); if (!v) return;
  let url: URL; try { url = new URL(v); } catch { return "Enter a full address starting with https://."; }
  if (url.protocol !== "https:" || !url.hostname) return "Use an https:// address.";
  if (url.username || url.password || url.hash || v.length > 2048 || /\s/.test(v) || control.test(v)) return "Use an https:// address without credentials or a #fragment.";
  return;
}
export function nameError(value: string): string | undefined {
  const v = value.trim();
  if (!v) return "Enter a name.";
  if (v.length > 120 || control.test(v)) return "Use at most 120 characters.";
  return;
}
export function daysError(value: string, min: number, max: number, label: string): string | undefined {
  if (!/^\d{1,4}$/.test(value.trim())) return `Enter ${label} as a whole number of days.`;
  const n = Number(value.trim());
  return n < min || n > max ? `Enter ${min}–${max} days.` : undefined;
}
export const isLoopback = (host: string) => /^(localhost|127(?:\.\d{1,3}){3}|\[?::1\]?)$/i.test(host.trim());
export function hostError(value: string): string | undefined {
  const v = value.trim();
  if (!v) return "Enter the relay's host name.";
  if (v.length > 253 || /[\s/:@]/.test(v) && !/^\[?[0-9a-f:]+\]?$/i.test(v)) return "Enter a host name or IP address only, without a port or scheme.";
  return;
}
export const passwordRefPattern = /^env:[A-Z_][A-Z0-9_]{0,127}$/;
export const emailPattern = /^[^\s@]+@[^\s@]+$/;

export type EmailDraft = { host: string; port: string; tls: TlsMode; username: string; password_ref: string; from_address: string; from_name: string };
export const emailDraft = (e: EmailSettings): EmailDraft => ({ host: e.host ?? "", port: e.port ? String(e.port) : "587", tls: e.tls ?? "starttls", username: e.username ?? "", password_ref: e.password_ref ?? "", from_address: e.from_address ?? "", from_name: e.from_name ?? "" });
export function emailErrors(d: EmailDraft): Partial<Record<keyof EmailDraft, string>> {
  const out: Partial<Record<keyof EmailDraft, string>> = {};
  const host = hostError(d.host); if (host) out.host = host;
  const port = daysError(d.port, 1, 65535, "the port"); if (port) out.port = "Enter a port from 1 to 65535.";
  if (!host && d.tls === "none" && !isLoopback(d.host)) out.tls = "Unencrypted delivery only works with a relay on this machine (localhost). Choose STARTTLS or implicit TLS.";
  const user = d.username.trim(), ref = d.password_ref.trim();
  if (user && !ref) out.password_ref = "Enter the password reference for this username.";
  if (!user && ref) out.username = "Enter the username this password belongs to.";
  if (ref && !passwordRefPattern.test(ref)) out.password_ref = "Enter env: followed by the variable name, for example env:SMTP_PASSWORD.";
  if (user.length > 256 || control.test(user)) out.username = "Use at most 256 characters.";
  if (!emailPattern.test(d.from_address.trim()) || d.from_address.trim().length > 320) out.from_address = "Enter an email address, such as gateway@example.com.";
  if (d.from_name.trim().length > 120 || control.test(d.from_name)) out.from_name = "Use at most 120 characters.";
  return out;
}
export const emailBody = (d: EmailDraft) => ({ host: d.host.trim(), port: Number(d.port.trim()), tls: d.tls, username: d.username.trim() || null, password_ref: d.password_ref.trim() || null, from_address: d.from_address.trim(), from_name: d.from_name.trim() || null });

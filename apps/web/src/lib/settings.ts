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

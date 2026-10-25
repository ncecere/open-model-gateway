import { ApiError, platformPath } from "./api";

export const webhookPath = `${platformPath}/alerts/webhooks`;
export const webhookItemPath = (id: string) => `${webhookPath}/${encodeURIComponent(id)}`;
export type AlertWebhook = {
  id: string; label: string; enabled: boolean; created_at: string; updated_at: string;
  signing_configured: boolean; overlap_active: boolean; previous_secret_until: string | null;
  waiting: number; exhausted: number; last_success_at: string | null; last_error: string | null;
  // Absent, not null placeholders, for Auditors/read-only tokens. Never signing bytes.
  url?: string; current_secret_ref?: string; previous_secret_ref?: string | null;
};
export const deliveryStates = ["waiting", "leased", "sent", "failed", "exhausted", "ineligible"] as const;
export type DeliveryState = typeof deliveryStates[number];
export type WebhookDelivery = { id: string; state: DeliveryState; test: boolean; attempt_count: number; created_at: string; completed_at: string | null; next_attempt_at: string | null; last_error: string | null };
export type WebhookDeliveries = { destination: AlertWebhook; counts: Record<DeliveryState, number>; data: WebhookDelivery[] };
export const deliveryLabels: Record<DeliveryState, string> = { waiting: "Waiting", leased: "Sending", sent: "Sent", failed: "Failed", exhausted: "Exhausted", ineligible: "Ineligible" };
export const webhookTestLimitMessage = "Too many test sends. Webhook tests are limited to 2 per minute per person and per destination, and 20 per hour in total. Try again in a minute.";
export function webhookError(error: unknown): Error {
  if (error instanceof ApiError) {
    let message: string | undefined;
    if (error.status === 429) message = webhookTestLimitMessage;
    else if (error.status === 404) message = "This webhook was deleted or is no longer available. Refresh the list.";
    else if (error.status === 400 || error.status === 422) message = "Check the name, operator-approved URL and signing references. Overlap needs a different previous reference and an RFC3339 end time within the next 24 hours. The URL cannot be changed.";
    else if (error.status === 409) message = error.message === "Maximum ten webhook destinations" ? "Ten webhooks already exist. Delete an unused destination before creating another." : error.message === "An enabled saved destination is required" ? "Enable the saved webhook before sending a test. Refresh if it was changed or deleted." : "This webhook conflicts with the saved configuration. Refresh and check its settings.";
    else if (error.status === 403) message = "Your access has changed. Refreshing permissions; Platform Admin access is required to make changes.";
    if (message) return new ApiError(error.status, error.code, message, error.reason);
  }
  // Do not echo unexpected server text that could contain configuration or receiver output.
  return new Error("Could not complete the webhook request. Check your connection and refresh before trying again.");
}
const errors: Record<string, string> = {
  connection: "Connection failed", timeout: "Request timed out", tls: "TLS verification failed", credential: "Signing material unavailable",
  destination_policy: "Destination approval refused", host_not_allowed: "Egress policy refused", rejected: "Receiver rejected delivery", interrupted: "Delivery interrupted",
};
export const deliveryErrorText = (value: string | null) => value ? errors[value] ?? "Delivery failed (sanitized category unavailable)" : "None recorded";
export type WebhookDraft = { label: string; url: string; current: string; previous: string; until: string };
export const webhookDraft = (saved?: AlertWebhook): WebhookDraft => ({ label: saved?.label ?? "", url: saved?.url ?? "", current: saved?.current_secret_ref ?? "", previous: saved?.previous_secret_ref ?? "", until: saved?.previous_secret_until ?? "" });
const referenceValid = (value: string) => /^env:[A-Z_][A-Z_0-9]{0,127}$/.test(value);
export function webhookDraftErrors(draft: WebhookDraft, saved?: AlertWebhook): Partial<Record<keyof WebhookDraft, string>> {
  const result: Partial<Record<keyof WebhookDraft, string>> = {};
  const label = draft.label.trim();
  if (!label || [...label].length > 120 || /[\u0000-\u001f\u007f-\u009f]/.test(label)) result.label = "Enter a name of 1–120 characters without control characters.";
  if (!saved) {
    try {
      const url = new URL(draft.url.trim());
      if (!["https:", "http:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || /[?#]/.test(draft.url)) throw new Error();
    } catch { result.url = "Enter an operator-approved HTTP(S) URL without credentials, query or fragment. HTTP is development-loopback only."; }
  }
  if (!referenceValid(draft.current.trim())) result.current = "Enter an operator-approved env:NAME reference, never a signing secret.";
  if (saved && (draft.previous !== (saved.previous_secret_ref ?? "") || draft.until !== (saved.previous_secret_until ?? ""))) {
    if (!!draft.previous.trim() !== !!draft.until.trim()) result.until = "Set both a previous signing reference and an overlap end, or clear both.";
    if (draft.previous.trim() && (!referenceValid(draft.previous.trim()) || draft.previous.trim() === draft.current.trim())) result.previous = "Use a different, operator-approved env:NAME reference.";
    if (draft.until.trim()) {
      const timestamp = Date.parse(draft.until.trim()), now = Date.now();
      if (!/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)$/i.test(draft.until.trim()) || !Number.isFinite(timestamp) || timestamp <= now || timestamp > now + 86400000) result.until = "Enter an RFC3339 time within the next 24 hours, for example 2026-11-01T12:00:00Z.";
    }
  }
  if (saved && draft.previous.trim() && draft.previous.trim() === draft.current.trim()) result.previous = "The previous and current signing references must differ.";
  return result;
}
/** POST accepts only three fields. PATCH preserves untouched overlaps (including expired ones). */
export function webhookBody(draft: WebhookDraft, saved?: AlertWebhook): Record<string, unknown> {
  if (!saved) return { label: draft.label.trim(), url: draft.url.trim(), current_secret_ref: draft.current.trim() };
  return {
    ...(draft.label.trim() !== saved.label ? { label: draft.label.trim() } : {}),
    ...(draft.current.trim() !== saved.current_secret_ref ? { current_secret_ref: draft.current.trim() } : {}),
    ...(draft.previous !== (saved.previous_secret_ref ?? "") || draft.until !== (saved.previous_secret_until ?? "") ? { previous_secret_ref: draft.previous.trim() || null, previous_secret_until: draft.until.trim() || null } : {}),
  };
}

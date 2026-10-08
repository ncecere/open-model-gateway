/*
 * StatusPill: the lifecycle state of a key, member or route as a Bitop
 * StatusBadge (dot + word, never colour alone). Disabled and Revoked are
 * deliberately distinct: Disabled can be re-enabled by an admin (warning);
 * Revoked is permanent (danger). Unknown is an outline pill, never "Active";
 * Pending pulses (no pulse under reduced motion, handled by Bitop Badge).
 * Pass `label` to reword, e.g. "Expired" with status="revoked".
 *
 *   <StatusPill status={key.revoked_at ? "revoked" : key.enabled ? "active" : "disabled"} />
 */
import { StatusBadge } from "../ui/badge/badge";
import type { Tone } from "../../lib/bitop-utils";

export type LifecycleStatus = "active" | "disabled" | "revoked" | "unknown" | "pending";

export const statusPresentation: Record<LifecycleStatus, { label: string; tone: Tone; description: string }> = {
  active: { label: "Active", tone: "success", description: "In use" },
  disabled: { label: "Disabled", tone: "warning", description: "Turned off; an admin can enable it again" },
  revoked: { label: "Revoked", tone: "danger", description: "Permanently revoked; it can never be re-enabled" },
  unknown: { label: "Unknown", tone: "neutral", description: "State not reported" },
  pending: { label: "Pending", tone: "info", description: "Waiting to take effect" },
};

/** Normalises a server string ("ACTIVE", "enabled", null) to a status; anything unrecognised is "unknown". */
export function toLifecycleStatus(value: string | null | undefined): LifecycleStatus {
  const v = value?.trim().toLowerCase();
  if (v === "active" || v === "enabled") return "active";
  if (v === "disabled" || v === "paused") return "disabled";
  if (v === "revoked") return "revoked";
  if (v === "pending") return "pending";
  return "unknown";
}

/** Whether rows in this state are shown struck through/muted in lists (disabled and revoked). */
export const isInactiveStatus = (status: LifecycleStatus) => status === "disabled" || status === "revoked";

export type StatusPillProps = {
  status: LifecycleStatus;
  /** Visible text override (the tone still follows `status`). */
  label?: string;
  size?: "sm" | "md";
  /** Show the explanation as the pill's title (hover) too. */
  explain?: boolean;
  className?: string;
};

export function StatusPill({ status, label, size = "sm", explain = false, className }: StatusPillProps) {
  const p = statusPresentation[status] ?? statusPresentation.unknown;
  return (
    <StatusBadge tone={p.tone} size={size} variant={status === "unknown" ? "outline" : "soft"} pulse={status === "pending"} title={explain ? p.description : undefined} className={className} data-status={status}>
      {label ?? p.label}
    </StatusBadge>
  );
}

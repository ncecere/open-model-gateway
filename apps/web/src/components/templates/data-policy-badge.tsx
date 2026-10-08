/*
 * DataPolicyBadge: what an upstream route does with prompts and responses —
 * "Doesn't keep data" (success), "Keeps data" (neutral, with the retention
 * detail, e.g. "30 days") or "Data policy unknown" (amber warning). Unknown is
 * never shown as "doesn't keep". Icon + words, never colour alone; `detail`
 * is visible text, not a tooltip.
 *
 *   <DataPolicyBadge policy="keeps" detail="30-day retention" />
 *   <DataPolicyBadge policy={route.data_policy ?? "unknown"} />
 */
import { ShieldAlert, ShieldCheck, ShieldQuestion } from "lucide-react";
import { Badge } from "../ui/badge/badge";
import { cx } from "../../lib/bitop-utils";
import styles from "./data-policy-badge.module.css";

export type DataPolicy = "keeps" | "no_keep" | "unknown";

const presentation = {
  no_keep: { label: "Doesn't keep data", tone: "success", icon: ShieldCheck },
  keeps: { label: "Keeps data", tone: "neutral", icon: ShieldAlert },
  unknown: { label: "Data policy unknown", tone: "warning", icon: ShieldQuestion },
} as const;

/** Normalises a server value; anything unrecognised (including null) is "unknown". */
export function toDataPolicy(value: string | null | undefined): DataPolicy {
  const v = value?.trim().toLowerCase();
  if (v === "keeps" || v === "retains" || v === "retained") return "keeps";
  if (v === "no_keep" || v === "zero_retention" || v === "zdr" || v === "none") return "no_keep";
  return "unknown";
}

export type DataPolicyBadgeProps = {
  policy: DataPolicy;
  /** Extra visible detail, e.g. "30-day retention" or "No training". */
  detail?: string;
  /** Override the label. */
  label?: string;
  size?: "sm" | "md";
  className?: string;
};

export function DataPolicyBadge({ policy, detail, label, size = "sm", className }: DataPolicyBadgeProps) {
  const p = presentation[policy] ?? presentation.unknown;
  const Icon = p.icon;
  return (
    <Badge tone={p.tone} size={size} className={cx(styles.badge, className)} data-policy={policy}>
      <Icon aria-hidden className={styles.icon} />
      {label ?? p.label}
      {detail && <span className={styles.detail}>· {detail}</span>}
    </Badge>
  );
}

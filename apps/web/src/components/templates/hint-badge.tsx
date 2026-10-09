/*
 * HintBadge: a status badge whose "why" is a tooltip instead of a sentence in
 * the row ("Not serving" → "No enabled route…"). Focusable only when it has a
 * hint, and the hint is its accessible description (always in the page, so
 * screen-reader and touch users get it too).
 *
 *   <HintBadge tone="warning" hint="No enabled route to a provider.">Not serving</HintBadge>
 */
import { useId, type ReactNode } from "react";
import { Badge, type BadgeProps } from "../ui/badge/badge";
import { Tooltip } from "../ui/tooltip/tooltip";
import { cx } from "../../lib/bitop-utils";
import styles from "./templates.module.css";

export function HintBadge({ hint, children, dot = true, size = "sm", ...props }: Omit<BadgeProps, "children"> & { hint?: ReactNode; children: ReactNode }) {
  const id = useId();
  if (!hint) return <Badge size={size} dot={dot} {...props}>{children}</Badge>;
  return <>
    <Tooltip content={hint}><Badge size={size} dot={dot} {...props} tabIndex={0} aria-describedby={id} className={cx(styles.hinted, props.className)}>{children}</Badge></Tooltip>
    <span id={id} hidden>{hint}</span>
  </>;
}

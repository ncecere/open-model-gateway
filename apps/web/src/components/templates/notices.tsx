/*
 * InfoBanner and DangerZone, reusing Bitop Alert and Card.
 *
 * - InfoBanner: a page-level explanation ("Limits on Personal are set by the
 *   platform", "Costs are estimates") — Bitop Alert, info tone by default
 *   (role="status"), optional actions and dismiss.
 * - DangerZone / DangerAction: Grounded's settings pattern (copied shape, not
 *   code): a red-bordered "Danger zone" card with one row per destructive
 *   action (title, consequence copy, its own button). Actions are not part of
 *   the page's save; each opens its own confirmation dialog. `disabledReason`
 *   says why an action can't run (e.g. "Revoked keys can't be re-enabled").
 *
 *   <InfoBanner title="Read-only">Personal limits are set by the platform.</InfoBanner>
 *   <DangerZone><DangerAction title="Revoke key" description="Requests using it fail immediately. This can't be undone."
 *     action={<Button variant="danger" onClick={confirm}>Revoke key</Button>} /></DangerZone>
 */
import type { ReactNode } from "react";
import { Alert } from "../ui/alert/alert";
import { Card } from "../ui/card/card";
import { cx } from "../../lib/bitop-utils";
import styles from "./notices.module.css";

export type InfoBannerProps = {
  title?: ReactNode;
  children?: ReactNode;
  /** "info" (default), "warning" for consequences, "success" for confirmations. */
  tone?: "info" | "warning" | "success";
  actions?: ReactNode;
  onDismiss?: () => void;
  className?: string;
};

export function InfoBanner({ title, children, tone = "info", actions, onDismiss, className }: InfoBannerProps) {
  return <Alert tone={tone} title={title} actions={actions} onDismiss={onDismiss} className={className}>{children}</Alert>;
}

export type DangerZoneProps = { children: ReactNode; title?: string; description?: ReactNode; className?: string };

export function DangerZone({ children, title = "Danger zone", description, className }: DangerZoneProps) {
  return (
    <Card title={title} titleAs="h2" description={description} className={cx(styles.dangerZone, className)}>
      <div className={styles.dangerList}>{children}</div>
    </Card>
  );
}

export type DangerActionProps = {
  title: ReactNode;
  /** Consequence copy: what happens, and whether it can be undone. */
  description: ReactNode;
  /** The button, e.g. <Button variant="danger">Delete</Button>; give it `disabled` with a `disabledReason`. */
  action: ReactNode;
  disabledReason?: ReactNode;
};

export function DangerAction({ title, description, action, disabledReason }: DangerActionProps) {
  return (
    <div className={styles.dangerRow}>
      <div className={styles.dangerText}>
        <h3 className={styles.dangerTitle}>{title}</h3>
        <p className={styles.dangerDescription}>{description}</p>
        {disabledReason && <p className={styles.dangerReason}>{disabledReason}</p>}
      </div>
      <div className={styles.dangerButton}>{action}</div>
    </div>
  );
}

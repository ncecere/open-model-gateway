/*
 * UpstreamModel: the Logs "Upstream model" of an attempt or request. The model
 * the provider reported serving is shown as is; when the provider reported
 * none, the route's configured upstream id is shown with a visible
 * "configured" marker (words, never colour alone). With `showRoute`, a
 * reported model that differs from the configured id also names the route.
 *
 *   <UpstreamModel row={generation} compact />
 *   <UpstreamModel row={attempt} showRoute />
 */
import { servedModel, servedModelText } from "../../lib/requests";
import { cx } from "../../lib/bitop-utils";
import styles from "./upstream-model.module.css";

type Row = { upstream_model?: string | null; reported_upstream_model?: string | null };

/** `compact` keeps one line in table cells: the id is ellipsized, the marker stays. */
export function UpstreamModel({ row, showRoute = false, compact = false }: { row: Row; showRoute?: boolean; compact?: boolean }) {
  const m = servedModel(row);
  if (!m) return <span className={styles.note}>Unknown</span>;
  return <span className={cx(styles.model, compact && styles.compact)} title={servedModelText(m)}>
    <span className={styles.id}>{m.id}</span>
    {m.configured && <>{" "}<span className={styles.note}>(configured)</span></>}
    {showRoute && m.route && <>{" "}<span className={styles.note}>· route {m.route}</span></>}
  </span>;
}

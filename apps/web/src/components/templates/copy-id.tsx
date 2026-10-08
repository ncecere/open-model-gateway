/*
 * CopyId: a long identifier (request, key, attempt, price id) shown short —
 * "req_8f3a…91c2" in monospace — with Bitop's CopyButton. The full value is
 * only in the `title` and the clipboard, so tables stay narrow; the copy
 * button is named after what it copies ("Copy request ID") and the result is
 * announced politely. A missing value shows "Unknown" with no button.
 *
 * Never use this for secrets (inference tokens): those belong in transient
 * one-time dialogs, not in titles.
 *
 *   <CopyId value={request.id} label="request ID" />
 */
import { CopyButton } from "../ui/copy-button/copy-button";
import { cx } from "../../lib/bitop-utils";
import styles from "./copy-id.module.css";

export type CopyIdProps = {
  value: string | null | undefined;
  /** What it is, lower-case, used in the button name: "request ID". */
  label: string;
  /** Characters kept at the start and end (default 8 and 4); values up to start+end+1 are shown in full. */
  head?: number;
  tail?: number;
  className?: string;
};

/** "abcdefgh…wxyz" for long values; the value itself when short enough. */
export function shortId(value: string, head = 8, tail = 4): string {
  return value.length <= head + tail + 1 ? value : `${value.slice(0, head)}…${value.slice(-tail)}`;
}

export function CopyId({ value, label, head = 8, tail = 4, className }: CopyIdProps) {
  if (!value) return <span className={cx(styles.root, styles.unknown, className)}>Unknown</span>;
  const short = shortId(value, head, tail);
  return (
    <span className={cx(styles.root, className)}>
      <code className={styles.code} title={value}>
        {short}
      </code>
      <CopyButton value={value} label={label} size="sm" variant="ghost" className={styles.button} />
    </span>
  );
}

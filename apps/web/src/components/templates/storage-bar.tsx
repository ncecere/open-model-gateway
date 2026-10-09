/*
 * StorageBar: a workspace's stored bytes against its storage quota (file
 * store: Files API uploads, batch inputs and outputs) over a Bitop Meter,
 * "120 MB of 1 GB". No quota reads "120 MB · No limit" without a bar; a
 * hidden or unknown used amount shows the quota only (unknown is not zero).
 *
 *   <StorageBar used={storage.used_bytes} quota={storage.quota_bytes} />
 */
import type { ReactNode } from "react";
import { Meter } from "../ui/meter/meter";
import { formatBytes } from "../../lib/bitop-format";
import s from "../../pages/shared.module.css";

/** "1.2 GB" with binary units shown in plain words (1 GB = 2^30 bytes). */
export const bytesText = (bytes: number) => formatBytes(bytes, { binary: true }).replace(/([KMGTP])iB\b/, "$1B");

export function StorageBar({ used, quota, label = "Storage", size = "md", description }: { used: number | null | undefined; quota: number | null | undefined; label?: string; size?: "sm" | "md"; description?: ReactNode }) {
  if (used === null || used === undefined) {
    return <p className={s.note}>{label}: {quota == null ? "no limit" : `${bytesText(quota)} quota`}</p>;
  }
  return <Meter label={label} size={size} value={used} max={quota ?? null} formatValue={bytesText} description={description} />;
}

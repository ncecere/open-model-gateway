/*
 * LayerTable: effective policy by layer — Platform → Team/Project/Personal →
 * workspace-local → Key — one row per layer in order, with where the value
 * comes from (type default, replacement override, local restriction), the
 * layer's value, optional Available/Partial/Unavailable model counts, and an
 * in-place expandable detail row ("Why can't I use this model?") instead of a
 * drawer. An optional final "Effective" row shows the composed result.
 * Unknown counts read "Unknown", never 0. Built on Bitop Table + ExpandableRow.
 *
 *   <LayerTable caption="Effective limits" valueHeader="Monthly budget" layers={[
 *     { id: "platform", kind: "platform", name: "Platform", source: <Badge>Type default</Badge>, value: "$500.00 / month" },
 *     { id: "key", kind: "key", name: "Key ci-runner", value: "Inherited", state: "inherited", details: <Reasons /> },
 *   ]} effective={{ value: "$500.00 / month" }} />
 */
import { Building2, FolderKanban, Globe, KeyRound, type LucideIcon, User, Users } from "lucide-react";
import type { ReactNode } from "react";
import { Table, Td, Th, Tr, type TableColumn } from "../ui/table/table";
import { ExpandableRow, expandColumn } from "./table-rows";
import styles from "./layer-table.module.css";

export type LayerKind = "platform" | "team" | "project" | "personal" | "workspace" | "key";
export type LayerCounts = { available: number | null; partial: number | null; unavailable: number | null };

export type Layer = {
  id: string;
  kind: LayerKind;
  /** Plain-text name, e.g. "Platform", "Team Research", "Key ci-runner". */
  name: string;
  /** Where the value comes from (a Badge), e.g. "Type default" / "Replaced for this workspace" / "Local restriction". */
  source?: ReactNode;
  /** The layer's own value, e.g. "$50.00 / month" or "Not set". */
  value: ReactNode;
  /** "inherited"/"none" mute the value (no own restriction at this layer). */
  state?: "set" | "inherited" | "none";
  counts?: LayerCounts;
  /** In-place detail (reasons per model, history). */
  details?: ReactNode;
};

const kindIcon: Record<LayerKind, LucideIcon> = { platform: Globe, team: Users, project: FolderKanban, personal: User, workspace: Building2, key: KeyRound };

export type LayerTableProps = {
  caption: string;
  showCaption?: boolean;
  layers: Layer[];
  /** Header of the value column (default "Value"). */
  valueHeader?: string;
  /** Show the Available / Partial / Unavailable model count columns. */
  showCounts?: boolean;
  /** The composed result as a final row. */
  effective?: { label?: string; value: ReactNode; counts?: LayerCounts };
};

const countText = (n: number | null | undefined) => (n === null || n === undefined || !Number.isFinite(n) ? "Unknown" : n.toLocaleString());

function CountCells({ counts, show }: { counts?: LayerCounts; show: boolean }) {
  if (!show) return null;
  return <>{(["available", "partial", "unavailable"] as const).map(k => {
    const text = counts ? countText(counts[k]) : "—";
    return <Td key={k} numeric data-unknown={text === "Unknown" ? "" : undefined} className={styles.count}>{text === "—" ? <><span aria-hidden>—</span><span className="sr-only">Not applicable</span></> : text}</Td>;
  })}</>;
}

export function LayerTable({ caption, showCaption, layers, valueHeader = "Value", showCounts = false, effective }: LayerTableProps) {
  const expandable = layers.some(l => l.details);
  const columns: TableColumn[] = [...(expandable ? [expandColumn] : []), "Layer", "Source", valueHeader, ...(showCounts ? [{ label: "Available", numeric: true }, { label: "Partial", numeric: true }, { label: "Unavailable", numeric: true }] : [])];
  const span = columns.length;
  return (
    <Table caption={caption} showCaption={showCaption} columns={columns} className={styles.table}>
      {layers.map((layer, index) => {
        const Icon = kindIcon[layer.kind];
        const cells = <>
          <Th>
            <span className={styles.layer}>
              <Icon aria-hidden className={styles.icon} />
              <span><span className="sr-only">{`Layer ${index + 1} of ${layers.length}: `}</span>{layer.name}</span>
            </span>
          </Th>
          <Td>{layer.source ?? <span className={styles.muted}>—</span>}</Td>
          <Td data-state={layer.state ?? "set"} className={styles.value}>{layer.value}</Td>
          <CountCells counts={layer.counts} show={showCounts} />
        </>;
        if (layer.details) return <ExpandableRow key={layer.id} label={layer.name} colSpan={span} details={layer.details}>{cells}</ExpandableRow>;
        return <Tr key={layer.id}>{expandable && <Td />}{cells}</Tr>;
      })}
      {effective && (
        <Tr className={styles.effective}>
          {expandable && <Td />}
          <Th>{effective.label ?? "Effective"}</Th>
          <Td><span className={styles.muted}>All layers apply</span></Td>
          <Td className={styles.value}>{effective.value}</Td>
          <CountCells counts={effective.counts} show={showCounts} />
        </Tr>
      )}
    </Table>
  );
}

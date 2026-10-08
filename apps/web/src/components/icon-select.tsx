/*
 * IconSelect: a compact single choice with an icon beside every option and the
 * chosen option's icon in the trigger (Add model › Type). An option that can't
 * be chosen right now stays listed but disabled, its reason in a tooltip on
 * hover and in the option's accessible name for screen readers.
 *
 * Gateway-owned composition of Base UI Select using the vendored Bitop Select
 * and popup styles (components/ui/select, unchanged): the vendored Select shows
 * only the label in its trigger and has no per-option reason.
 *
 *   <IconSelect label="Type" value={workload} onChange={setWorkload}
 *     items={[{ value: "generation", label: "Text", icon: <MessageSquareText /> }, { value: "rerank", label: "Rerank", icon: <ListOrdered />, disabledReason: "Not available on Anthropic" }]} />
 */
import { Select as BaseSelect } from "@base-ui/react/select";
import { Check, ChevronsUpDown } from "lucide-react";
import type { ReactNode } from "react";
import popup from "./ui/styles/popup.module.css";
import select from "./ui/select/select.module.css";
import { Tooltip } from "./ui/tooltip/tooltip";
import styles from "./icon-select.module.css";

export type IconSelectItem<V extends string> = { value: V; label: string; icon: ReactNode; /** Shown as a tooltip; the option is disabled when set. */ disabledReason?: string };

export function IconSelect<V extends string>({ label, items, value, onChange, disabled, id }: { label: ReactNode; items: IconSelectItem<V>[]; value: V; onChange: (value: V) => void; disabled?: boolean; id?: string }) {
  const chosen = items.find(i => i.value === value);
  return (
    <BaseSelect.Root<V> items={items.map(i => ({ value: i.value, label: i.label }))} value={value} onValueChange={next => { if (next !== null && next !== value) onChange(next as V); }} disabled={disabled}>
      <div className={select.root}>
        <BaseSelect.Label className={select.label}>{label}</BaseSelect.Label>
        <BaseSelect.Trigger id={id} className={select.trigger} data-size="md">
          <BaseSelect.Value className={`${select.value} ${styles.value}`}>{() => chosen ? <><span aria-hidden className={styles.icon}>{chosen.icon}</span>{chosen.label}</> : null}</BaseSelect.Value>
          <BaseSelect.Icon className={select.icon}><ChevronsUpDown aria-hidden /></BaseSelect.Icon>
        </BaseSelect.Trigger>
      </div>
      <BaseSelect.Portal>
        <BaseSelect.Positioner className={popup.positioner} sideOffset={6} alignItemWithTrigger={false}>
          <BaseSelect.Popup className={`${popup.popup} ${select.popup}`}>
            <BaseSelect.List>
              {items.map(item => {
                const content = <span className={styles.option}><span aria-hidden className={styles.icon}>{item.icon}</span><BaseSelect.ItemText className={select.itemText}>{item.label}</BaseSelect.ItemText>{item.disabledReason && <span className="sr-only">({item.disabledReason})</span>}</span>;
                return <BaseSelect.Item key={item.value} value={item.value} label={item.label} disabled={!!item.disabledReason} className={`${popup.item} ${select.item}`}>
                  {item.disabledReason ? <Tooltip content={item.disabledReason} side="right">{content}</Tooltip> : content}
                  <BaseSelect.ItemIndicator className={select.indicator}><Check aria-hidden /></BaseSelect.ItemIndicator>
                </BaseSelect.Item>;
              })}
            </BaseSelect.List>
          </BaseSelect.Popup>
        </BaseSelect.Positioner>
      </BaseSelect.Portal>
    </BaseSelect.Root>
  );
}

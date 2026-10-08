"use client";

import { Autocomplete as BaseAutocomplete } from "@base-ui/react/autocomplete";
import { Combobox as BaseCombobox } from "@base-ui/react/combobox";
import { Check, ChevronsUpDown, X } from "lucide-react";
import { type FocusEvent, type KeyboardEvent, type MouseEvent, type ReactNode, useMemo, useRef } from "react";
import popup from "@/components/ui/styles/popup.module.css";
import { cx } from "@/lib/bitop-utils";
import styles from "./combobox.module.css";

/*
 * Combobox: a filterable select (Base UI Combobox). Type to filter, ↑/↓ to
 * move, Enter to pick, Esc to close. Single or multiple selection; in
 * multiple mode the picks show as chips inside the input (← from the start
 * of the input moves onto them, Backspace/Delete removes one).
 *
 *   <Field label="Country" description="Where the account is billed.">
 *     <Combobox items={countries} value={code} onValueChange={setCode} clearable />
 *   </Field>
 *
 *   <Field label="Reviewers">
 *     <Combobox multiple items={people} defaultValue={["ana"]} />
 *   </Field>
 *
 * The input is a Base UI Field control: inside <Field> it is labelled,
 * described and marked invalid. Standalone, pass `aria-label`. Items that
 * share a `group` are shown under a heading. Values are the items' string
 * `value`s; the input shows and filters by `label`.
 *
 * Items the server already filtered (a search as you type): pass
 * `filter={null}` so the list shows `items` as they are, and fetch them from
 * `onInputValueChange`. `filter` can also be a function, e.g. to match a
 * hint as well as the label.
 *
 *   <Combobox items={results} filter={null} onInputValueChange={setQuery} value={id} onValueChange={setId} />
 *
 * `freeText` accepts any text, with the items as suggestions (Base UI
 * Autocomplete): the value is the input's text, and picking a suggestion
 * fills in its `value`. Use it for names that may not be listed yet.
 *
 *   <Combobox freeText items={knownGroups} value={group} onValueChange={setGroup} />
 *
 * Enter in the input picks the highlighted option and never submits a
 * surrounding form or dialog, open list or not: after Esc closes the list,
 * the next Enter used to send the form half filled in. `submitOnEnter`
 * lets Enter submit when the list has nothing highlighted (the `freeText`
 * default, where the text is the value, like a plain input).
 *
 * Use Select when there are only a handful of options, and NativeSelect in
 * plain forms.
 */

export type ComboboxOption<V extends string = string> = {
  value: V;
  label: string;
  /** Decorative icon shown before the label. */
  icon?: ReactNode;
  /** Muted secondary text shown at the end of the row. */
  hint?: ReactNode;
  /** Group heading; options with the same group are listed together. */
  group?: string;
  disabled?: boolean;
};

type ComboboxBaseProps<V extends string> = {
  items: ComboboxOption<V>[];
  /**
   * How items match the typed text. Default: the label contains the text (case- and
   * accent-insensitive). `null` turns filtering off, for items the server already filtered.
   */
  filter?: ((option: ComboboxOption<V>, query: string) => boolean) | null;
  placeholder?: string;
  /** Shown (and announced) when nothing matches the typed text. */
  emptyText?: ReactNode;
  /** Show a button that clears the selection. */
  clearable?: boolean;
  /** Accessible name of the clear button. */
  clearLabel?: string;
  /** Accessible name of the button that opens the list. */
  triggerLabel?: string;
  /** Accessible name when the combobox is not inside a labelled Field. */
  "aria-label"?: string;
  /** Called as the user types. */
  onInputValueChange?: (text: string) => void;
  /** Highlight the first match while typing, so Enter picks it. */
  autoHighlight?: boolean;
  /** Most options to render at once (long lists). */
  limit?: number;
  /**
   * Let Enter submit a surrounding form when no option is highlighted (default: false, and true with
   * `freeText`). Enter on a highlighted option always picks it.
   */
  submitOnEnter?: boolean;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  name?: string;
  id?: string;
  disabled?: boolean;
  readOnly?: boolean;
  required?: boolean;
  size?: "sm" | "md";
  className?: string;
};

export type ComboboxSingleProps<V extends string = string> = ComboboxBaseProps<V> & {
  multiple?: false;
  freeText?: false;
  value?: V | null;
  defaultValue?: V | null;
  onValueChange?: (value: V | null, option: ComboboxOption<V> | null) => void;
};

export type ComboboxMultipleProps<V extends string = string> = ComboboxBaseProps<V> & {
  multiple: true;
  freeText?: false;
  value?: V[];
  defaultValue?: V[];
  onValueChange?: (value: V[], options: ComboboxOption<V>[]) => void;
  /** Accessible name of the chip list, e.g. "Selected reviewers". */
  chipsLabel?: string;
};

/** Any text, with `items` as suggestions: `value` is the text, and a picked suggestion fills in its `value`. */
export type ComboboxFreeTextProps = Omit<ComboboxBaseProps<string>, "onInputValueChange"> & {
  freeText: true;
  multiple?: false;
  value?: string;
  defaultValue?: string;
  onValueChange?: (text: string) => void;
};

export type ComboboxProps<V extends string = string> = ComboboxSingleProps<V> | ComboboxMultipleProps<V> | ComboboxFreeTextProps;

type OptionGroup<V extends string> = { value: string; items: ComboboxOption<V>[] };

function groupOptions<V extends string>(items: ComboboxOption<V>[]): OptionGroup<V>[] {
  const groups = new Map<string, ComboboxOption<V>[]>();
  for (const item of items) {
    const key = item.group ?? "";
    const list = groups.get(key);
    if (list) list.push(item);
    else groups.set(key, [item]);
  }
  return [...groups].map(([value, list]) => ({ value, items: list }));
}

export function Combobox<V extends string = string>(props: ComboboxProps<V>) {
  return props.freeText ? <FreeTextCombobox {...props} /> : <SelectCombobox {...props} />;
}

/**
 * Keeps Enter in the input from submitting the surrounding form. It runs as
 * the key event bubbles out of the input, after Base UI has picked the
 * highlighted option, so only the form's implicit submission is stopped.
 */
function keepEnter(submitOnEnter: boolean) {
  return (event: KeyboardEvent<HTMLDivElement>) => {
    if (submitOnEnter || event.key !== "Enter" || !(event.target instanceof HTMLInputElement)) return;
    event.preventDefault();
  };
}

function toBaseFilter<V extends string>(filter: ComboboxBaseProps<V>["filter"]) {
  if (filter === undefined || filter === null) return filter;
  return (item: unknown, query: string) => filter(item as ComboboxOption<V>, query);
}

function SelectCombobox<V extends string>(props: ComboboxSingleProps<V> | ComboboxMultipleProps<V>) {
  const {
    items,
    filter,
    placeholder,
    emptyText = "No results.",
    clearable = false,
    clearLabel = "Clear selection",
    triggerLabel = "Show options",
    "aria-label": ariaLabel,
    onInputValueChange,
    autoHighlight,
    limit,
    submitOnEnter = false,
    open,
    defaultOpen,
    onOpenChange,
    name,
    id,
    disabled,
    readOnly,
    required,
    size = "md",
    className,
  } = props;

  const anchorRef = useRef<HTMLDivElement | null>(null);
  const grouped = items.some((item) => item.group !== undefined);
  const byValue = useMemo(() => new Map(items.map((item) => [item.value, item])), [items]);
  const collection = useMemo(() => {
    const getters = { getValue: (o: ComboboxOption<V>) => o.value, getLabel: (o: ComboboxOption<V>) => o.label };
    return grouped ? BaseCombobox.createItems(groupOptions(items), getters) : BaseCombobox.createItems(items, getters);
  }, [items, grouped]);

  const labelOf = (value: V) => byValue.get(value)?.label ?? value;

  const handleValueChange = (next: unknown) => {
    if (props.multiple) {
      const values = (next as V[] | null) ?? [];
      props.onValueChange?.(values, values.flatMap((v) => byValue.get(v) ?? []));
    } else {
      const value = (next as V | null) ?? null;
      props.onValueChange?.(value, value === null ? null : (byValue.get(value) ?? null));
    }
  };

  const renderItem = (item: ComboboxOption<V>) => (
    <BaseCombobox.Item key={item.value} value={item.value} disabled={item.disabled} className={cx(popup.item, styles.item)}>
      {item.icon && (
        <span aria-hidden className={styles.itemIcon}>
          {item.icon}
        </span>
      )}
      <span className={styles.itemText}>{item.label}</span>
      {item.hint && <span className={styles.hint}>{item.hint}</span>}
      <BaseCombobox.ItemIndicator className={styles.indicator}>
        <Check aria-hidden />
      </BaseCombobox.ItemIndicator>
    </BaseCombobox.Item>
  );

  // A single pick's input shows the chosen label: focusing it selects that text, so typing searches afresh
  // instead of appending to it ("America/New_Yorkberl" found nothing). The mouseup after a click's focus
  // would put the caret back, so it's skipped once.
  const justFocused = useRef(false);
  const selectOnFocus = props.multiple
    ? {}
    : {
        onFocus: (e: FocusEvent<HTMLInputElement>) => {
          e.currentTarget.select();
          justFocused.current = true;
        },
        onMouseUp: (e: MouseEvent<HTMLInputElement>) => {
          if (justFocused.current) e.preventDefault();
          justFocused.current = false;
        },
      };
  const input = (hasChips: boolean) => (
    <BaseCombobox.Input
      id={id}
      aria-label={ariaLabel}
      placeholder={hasChips ? undefined : placeholder}
      className={cx(styles.input, props.multiple && styles.chipInput)}
      {...selectOnFocus}
    />
  );

  return (
    <BaseCombobox.Root
      items={collection as never}
      filter={toBaseFilter(filter) as never}
      multiple={props.multiple ?? false}
      value={props.value as never}
      defaultValue={props.defaultValue as never}
      onValueChange={handleValueChange}
      onInputValueChange={onInputValueChange ? (text) => onInputValueChange(text) : undefined}
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={onOpenChange ? (next) => onOpenChange(next) : undefined}
      autoHighlight={autoHighlight}
      limit={limit}
      name={name}
      disabled={disabled}
      readOnly={readOnly}
      required={required}
    >
      <BaseCombobox.InputGroup ref={anchorRef} data-size={size} className={cx(styles.control, className)} onKeyDown={keepEnter(submitOnEnter)}>
        {props.multiple ? (
          <BaseCombobox.Value>
            {(selected: V[]) => (
              <BaseCombobox.Chips className={styles.chips} aria-label={selected.length > 0 ? (props.chipsLabel ?? "Selected") : undefined}>
                {selected.map((value) => (
                  <BaseCombobox.Chip key={value} className={styles.chip}>
                    <span className={styles.chipText}>{labelOf(value)}</span>
                    <BaseCombobox.ChipRemove className={styles.chipRemove} aria-label={`Remove ${labelOf(value)}`}>
                      <X aria-hidden />
                    </BaseCombobox.ChipRemove>
                  </BaseCombobox.Chip>
                ))}
                {input(selected.length > 0)}
              </BaseCombobox.Chips>
            )}
          </BaseCombobox.Value>
        ) : (
          input(false)
        )}
        <div className={styles.actions}>
          {clearable && (
            <BaseCombobox.Clear className={styles.action} aria-label={clearLabel}>
              <X aria-hidden />
            </BaseCombobox.Clear>
          )}
          <BaseCombobox.Trigger className={styles.action} aria-label={triggerLabel}>
            <ChevronsUpDown aria-hidden />
          </BaseCombobox.Trigger>
        </div>
      </BaseCombobox.InputGroup>
      <BaseCombobox.Portal>
        <BaseCombobox.Positioner className={popup.positioner} anchor={anchorRef} align="start" sideOffset={6}>
          <BaseCombobox.Popup className={cx(popup.popup, styles.popup)}>
            <BaseCombobox.Empty className={styles.empty}>{emptyText}</BaseCombobox.Empty>
            <BaseCombobox.List className={styles.list}>
              {grouped
                ? (group: OptionGroup<V>) => (
                    <BaseCombobox.Group key={group.value} items={group.items} className={styles.group}>
                      {group.value && <BaseCombobox.GroupLabel className={popup.groupLabel}>{group.value}</BaseCombobox.GroupLabel>}
                      <BaseCombobox.Collection>{renderItem}</BaseCombobox.Collection>
                    </BaseCombobox.Group>
                  )
                : renderItem}
            </BaseCombobox.List>
          </BaseCombobox.Popup>
        </BaseCombobox.Positioner>
      </BaseCombobox.Portal>
    </BaseCombobox.Root>
  );
}

/** The `freeText` mode: Base UI Autocomplete, whose value is the input's text. */
function FreeTextCombobox(props: ComboboxFreeTextProps) {
  const {
    items,
    filter,
    placeholder,
    emptyText,
    clearable = false,
    clearLabel = "Clear text",
    triggerLabel = "Show suggestions",
    "aria-label": ariaLabel,
    autoHighlight,
    limit,
    submitOnEnter = true,
    open,
    defaultOpen,
    onOpenChange,
    name,
    id,
    disabled,
    readOnly,
    required,
    size = "md",
    className,
    value,
    defaultValue,
    onValueChange,
  } = props;

  const anchorRef = useRef<HTMLDivElement | null>(null);
  const grouped = items.some((item) => item.group !== undefined);
  const list = useMemo(() => (grouped ? groupOptions(items) : items), [items, grouped]);

  const renderItem = (item: ComboboxOption) => (
    <BaseAutocomplete.Item key={item.value} value={item} disabled={item.disabled} className={cx(popup.item, styles.item)}>
      {item.icon && (
        <span aria-hidden className={styles.itemIcon}>
          {item.icon}
        </span>
      )}
      <span className={styles.itemText}>{item.label}</span>
      {item.hint && <span className={styles.hint}>{item.hint}</span>}
    </BaseAutocomplete.Item>
  );

  return (
    <BaseAutocomplete.Root
      items={list as never}
      itemToStringValue={(item: unknown) => (item as ComboboxOption).value}
      filter={toBaseFilter(filter) as never}
      value={value}
      defaultValue={defaultValue}
      onValueChange={onValueChange ? (text) => onValueChange(text) : undefined}
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={onOpenChange ? (next) => onOpenChange(next) : undefined}
      autoHighlight={autoHighlight}
      limit={limit}
      name={name}
      disabled={disabled}
      readOnly={readOnly}
      required={required}
    >
      <BaseAutocomplete.InputGroup ref={anchorRef} data-size={size} className={cx(styles.control, className)} onKeyDown={keepEnter(submitOnEnter)}>
        <BaseAutocomplete.Input id={id} aria-label={ariaLabel} placeholder={placeholder} className={styles.input} />
        <div className={styles.actions}>
          {clearable && (
            <BaseAutocomplete.Clear className={styles.action} aria-label={clearLabel}>
              <X aria-hidden />
            </BaseAutocomplete.Clear>
          )}
          <BaseAutocomplete.Trigger className={styles.action} aria-label={triggerLabel}>
            <ChevronsUpDown aria-hidden />
          </BaseAutocomplete.Trigger>
        </div>
      </BaseAutocomplete.InputGroup>
      <BaseAutocomplete.Portal>
        <BaseAutocomplete.Positioner className={popup.positioner} anchor={anchorRef} align="start" sideOffset={6}>
          <BaseAutocomplete.Popup className={cx(popup.popup, styles.popup)}>
            {emptyText && <BaseAutocomplete.Empty className={styles.empty}>{emptyText}</BaseAutocomplete.Empty>}
            <BaseAutocomplete.List className={styles.list}>
              {grouped
                ? (group: OptionGroup<string>) => (
                    <BaseAutocomplete.Group key={group.value} items={group.items} className={styles.group}>
                      {group.value && <BaseAutocomplete.GroupLabel className={popup.groupLabel}>{group.value}</BaseAutocomplete.GroupLabel>}
                      <BaseAutocomplete.Collection>{renderItem}</BaseAutocomplete.Collection>
                    </BaseAutocomplete.Group>
                  )
                : renderItem}
            </BaseAutocomplete.List>
          </BaseAutocomplete.Popup>
        </BaseAutocomplete.Positioner>
      </BaseAutocomplete.Portal>
    </BaseAutocomplete.Root>
  );
}

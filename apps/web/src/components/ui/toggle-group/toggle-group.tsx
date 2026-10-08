"use client";

import { ToggleGroup as BaseToggleGroup } from "@base-ui/react/toggle-group";
import { type FocusEvent, type Ref, createContext, useContext, useMemo, useRef, useState } from "react";
import { Toggle, type ToggleProps, type ToggleSize, type ToggleVariant } from "@/components/ui/toggle/toggle";
import { cx, dataFlag, mergeRefs, scrollEdgeAttrs, useScrollEdges } from "@/lib/bitop-utils";
import styles from "./toggle-group.module.css";

/*
 * ToggleGroup: shared pressed state for a row (or column) of toggles, built
 * on Base UI ToggleGroup. One item at a time by default; `multiple` allows
 * several. Arrow keys move focus between items (roving tabindex), and the
 * group is a role="group" that must be named.
 *
 *   <ToggleGroup aria-label="Text alignment" defaultValue={["left"]}>
 *     <ToggleGroupItem value="left" iconOnly aria-label="Align left"><AlignLeft aria-hidden /></ToggleGroupItem>
 *     ...
 *   </ToggleGroup>
 *
 * Items are Toggles: variant and size come from the group.
 *
 * Tabbing into the group lands on the pressed item (the first pressed one
 * with `multiple`), not on the first item, also after the value changed
 * from outside (a URL, a reset). A joined group too wide for its container
 * (a phone) wraps onto more lines, its items then spaced apart, so every
 * choice stays readable; with `overflow="scroll"` it scrolls sideways
 * instead and fades the edge where more items are hidden.
 */

type GroupContextValue = { variant: ToggleVariant; size: ToggleSize; pressed: readonly string[] };
const GroupContext = createContext<GroupContextValue>({ variant: "ghost", size: "md", pressed: [] });

/**
 * Tab / Shift+Tab into the group: focus comes from outside, lands on the roving tab stop
 * (tabindex 0 until Base UI re-renders), and no pointer press started it.
 */
function tabbedIn(event: FocusEvent<HTMLDivElement>, pointerAt: number) {
  if (pointerAt && performance.now() - pointerAt < 1000) return false;
  const from = event.relatedTarget;
  if (from instanceof Node && event.currentTarget.contains(from)) return false;
  return (event.target as HTMLElement).getAttribute("tabindex") === "0";
}

type ToggleGroupBaseProps = Omit<BaseToggleGroup.Props, "className"> & {
  variant?: ToggleVariant;
  size?: ToggleSize;
  /** Join items into one segmented control (shared borders, no gaps). */
  joined?: boolean;
  /** A joined group too wide for its container: "wrap" onto more lines (default) or "scroll" sideways with a fade. */
  overflow?: "wrap" | "scroll";
  className?: string;
  ref?: Ref<HTMLDivElement>;
};

/** The group needs an accessible name: pass `aria-label` or `aria-labelledby`. */
export type ToggleGroupProps = ToggleGroupBaseProps & ({ "aria-label": string } | { "aria-labelledby": string });

export function ToggleGroup({
  variant = "ghost",
  size = "md",
  joined = false,
  overflow = "wrap",
  orientation = "horizontal",
  className,
  children,
  value,
  defaultValue,
  onValueChange,
  onFocus,
  onPointerDown,
  onClickCapture,
  ref,
  ...props
}: ToggleGroupProps) {
  // The pressed values, also when uncontrolled, so the pressed item can hold the tab stop.
  const [inner, setInner] = useState<readonly string[]>(defaultValue ?? []);
  const pressed = value ?? inner;
  // When a pointer press last started in the group (a click or tap focuses the item it hits).
  const pointerAt = useRef(0);
  const [edgesRef, edges] = useScrollEdges<HTMLDivElement>();
  const groupRef = useMemo(() => (ref ? mergeRefs(edgesRef, ref) : edgesRef), [edgesRef, ref]);
  const context = useMemo(() => ({ variant, size, pressed }), [variant, size, pressed]);
  return (
    <BaseToggleGroup
      {...props}
      ref={groupRef}
      value={value}
      defaultValue={defaultValue}
      onValueChange={(next, details) => {
        onValueChange?.(next, details);
        if (value === undefined && !details.isCanceled) setInner(next);
      }}
      onPointerDown={(event) => {
        pointerAt.current = performance.now();
        onPointerDown?.(event);
      }}
      onClickCapture={(event) => {
        pointerAt.current = 0;
        onClickCapture?.(event);
      }}
      onFocus={(event) => {
        onFocus?.(event);
        const entry = tabbedIn(event, pointerAt.current);
        pointerAt.current = 0;
        if (!entry) return;
        // Base UI's roving tabindex remembers the last focused item; move to the pressed one.
        const target = event.currentTarget.querySelector<HTMLElement>('[aria-pressed="true"]:not([disabled]):not([aria-disabled="true"])');
        if (target && target !== event.target) target.focus();
      }}
      orientation={orientation}
      {...scrollEdgeAttrs(edges)}
      data-variant={variant}
      data-size={size}
      data-joined={dataFlag(joined)}
      data-overflow-mode={joined ? overflow : undefined}
      className={cx(styles.group, className)}
    >
      <GroupContext.Provider value={context}>{children}</GroupContext.Provider>
    </BaseToggleGroup>
  );
}

/** A Toggle inside a ToggleGroup. `value` identifies it in the group's value array. */
export type ToggleGroupItemProps = Omit<ToggleProps, "variant" | "size" | "value"> & { value: string };

export function ToggleGroupItem({ className, ...props }: ToggleGroupItemProps) {
  const { variant, size, pressed } = useContext(GroupContext);
  // Base UI gives the first tab stop to the first item marked active (else to the first item).
  const active = pressed.includes(props.value) ? { "data-composite-item-active": "" } : undefined;
  return <Toggle {...(props as ToggleProps)} {...active} variant={variant} size={size} className={cx(styles.item, className)} />;
}

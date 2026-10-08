/*
 * StickySaveBar: Bitop's SaveBar (unchanged) that also reserves its height at
 * the bottom of the page while it is open, so it never hides the controls it
 * floats over. The bar's height (plus its offset from the viewport edge) is
 * published as `--save-bar-space` on <html>, which the shell uses as
 * `scroll-padding-block-end`: focusing a field, following an anchor or
 * scrollIntoView stop above the bar instead of under it. (At the end of the
 * form the sticky bar rests in its own place in the flow, after the last
 * control.) Closed or unmounted, the space goes back to 0.
 *
 *   <StickySaveBar open={dirty} message="Unsaved changes"><Button …>Save</Button></StickySaveBar>
 */
import { useEffect, useRef, type ComponentProps } from "react";
import { SaveBar } from "../ui/save-bar/save-bar";

const VAR = "--save-bar-space";
/** Open bars, so two forms on one page don't clear each other's space. */
const open = new Map<object, number>();
function publish() {
  if (typeof document === "undefined") return;
  const space = Math.max(0, ...open.values());
  if (space > 0) document.documentElement.style.setProperty(VAR, `${Math.ceil(space)}px`);
  else document.documentElement.style.removeProperty(VAR);
}

export function StickySaveBar(props: ComponentProps<typeof SaveBar>) {
  const ref = useRef<HTMLDivElement>(null), token = useRef({});
  useEffect(() => {
    const el = ref.current, id = token.current;
    if (!props.open || !el) { open.delete(id); publish(); return; }
    const measure = () => {
      // The bar's height plus the gap it keeps from the bottom edge (its sticky `bottom` offset).
      const offset = Number.parseFloat(getComputedStyle(el).bottom) || 0;
      open.set(id, el.getBoundingClientRect().height + offset * 2);
      publish();
    };
    measure();
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(measure);
    observer?.observe(el);
    return () => { observer?.disconnect(); open.delete(id); publish(); };
  }, [props.open]);
  return <SaveBar {...props} ref={ref} />;
}

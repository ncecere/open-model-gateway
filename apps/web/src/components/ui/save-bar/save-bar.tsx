import type { ComponentPropsWithRef, ReactNode } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./save-bar.module.css";

/*
 * SaveBar: the actions of a long form (Save, Discard), stuck to the bottom of
 * the scrolling area while the form has unsaved changes, so they stay in reach
 * without scrolling to the end.
 *
 *   <form>
 *     …fields…
 *     <SaveBar open={dirty}>
 *       <Button variant="ghost" onClick={discard}>Discard</Button>
 *       <Button type="submit">Save</Button>
 *     </SaveBar>
 *   </form>
 *
 * Put it last inside the form (or the element that scrolls with it): it uses
 * position: sticky, so it rests at the end of the form once that is in view.
 * The message is a polite live region, so "Unsaved changes" is announced when
 * the bar appears. When closed, the bar and its actions are hidden.
 */

export type SaveBarProps = Omit<ComponentPropsWithRef<"div">, "children"> & {
  /** Show the bar (usually: the form has unsaved changes). */
  open: boolean;
  /** Status text; announced when the bar opens. Default "Unsaved changes". */
  message?: ReactNode;
  /** The actions, e.g. Discard and Save buttons. */
  children: ReactNode;
};

export function SaveBar({ open, message = "Unsaved changes", className, children, ...props }: SaveBarProps) {
  return (
    <div {...props} className={cx(styles.bar, className)} data-open={dataFlag(open)}>
      <p role="status" className={styles.message}>
        {open ? message : null}
      </p>
      <div className={styles.actions} hidden={!open}>
        {children}
      </div>
    </div>
  );
}

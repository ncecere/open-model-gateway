"use client";

import { useRender } from "@base-ui/react/use-render";
import { ArrowUpRight } from "lucide-react";
import type { ComponentPropsWithRef } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./text-link.module.css";

export type TextLinkProps = Omit<ComponentPropsWithRef<"a">, "className"> & {
  className?: string;
  /** `muted` for secondary links in dense UI (still underlined on hover). */
  tone?: "default" | "muted";
  /** Opens in a new tab, adds an icon and announces "(opens in a new tab)". */
  external?: boolean;
  /**
   * Render through another link component, e.g. TanStack Router:
   * `<TextLink render={<Link to="/teams/$team" params={{ team }} />}>Team</TextLink>`.
   */
  render?: useRender.RenderProp;
};

/** An inline text link. */
export function TextLink({ tone = "default", external = false, className, render, ref, children, ...props }: TextLinkProps) {
  return useRender({
    render,
    defaultTagName: "a",
    ref,
    props: {
      ...(external ? { target: "_blank", rel: "noreferrer noopener" } : {}),
      ...props,
      className: cx(styles.link, className),
      "data-tone": tone,
      "data-external": dataFlag(external),
      children: (
        <>
          {children}
          {external && (
            <>
              <ArrowUpRight aria-hidden className={styles.icon} />
              <span className="sr-only"> (opens in a new tab)</span>
            </>
          )}
        </>
      ),
    },
  });
}

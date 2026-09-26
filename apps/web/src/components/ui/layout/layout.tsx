"use client";

import { useRender } from "@base-ui/react/use-render";
import type { ComponentPropsWithRef, CSSProperties } from "react";
import { cx, dataFlag } from "@/lib/bitop-utils";
import styles from "./layout.module.css";

/*
 * Flex layout primitives with gaps from the spacing scale.
 *   <Stack gap={4}>…</Stack>            vertical
 *   <Inline gap={2} align="center">…    horizontal, wraps
 *   <Container>…</Container>            centred max-width page column (1200px)
 */

export type Space = 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12;

type FlexProps = Omit<ComponentPropsWithRef<"div">, "className"> & {
  gap?: Space;
  align?: "start" | "center" | "end" | "stretch" | "baseline";
  justify?: "start" | "center" | "end" | "between";
  className?: string;
  /** Render as another element, e.g. `render={<ul />}`. */
  render?: useRender.RenderProp;
};

function useFlex(kind: "stack" | "inline", { gap, align, justify, className, style, render, ref, ...props }: FlexProps, extra: object) {
  const gapStyle = gap !== undefined ? ({ "--flex-gap": `var(--space-${gap})` } as CSSProperties) : undefined;
  return useRender({
    render,
    defaultTagName: "div",
    ref,
    props: {
      ...props,
      ...extra,
      className: cx(styles[kind], className),
      "data-align": align,
      "data-justify": justify,
      style: { ...gapStyle, ...style },
    },
  });
}

export type StackProps = FlexProps;

export function Stack(props: StackProps) {
  return useFlex("stack", props, {});
}

export type InlineProps = FlexProps & { wrap?: boolean };

export function Inline({ wrap = true, ...props }: InlineProps) {
  return useFlex("inline", props, { "data-nowrap": dataFlag(!wrap) });
}

export type ContainerProps = ComponentPropsWithRef<"div"> & { size?: "md" | "lg" | "full" };

/** Centred page column (max 1200px) with responsive side padding. */
export function Container({ size = "lg", className, ...props }: ContainerProps) {
  return <div data-size={size} className={cx(styles.container, className)} {...props} />;
}

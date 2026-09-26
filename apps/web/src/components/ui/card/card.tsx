"use client";

import { type ComponentPropsWithRef, type ReactNode, useId } from "react";
import { cx } from "@/lib/bitop-utils";
import styles from "./card.module.css";

/*
 * Card: a white surface with a hairline ring and soft shadow.
 *
 * Shorthand:   <Card title="Members" actions={<Button/>}>body</Card>
 * Composable:  <Card><CardHeader title=… /><CardBody flush>…</CardBody><CardFooter>…</CardFooter></Card>
 */

type HeadingLevel = "h2" | "h3" | "h4";

export type CardProps = Omit<ComponentPropsWithRef<"section">, "title"> & {
  /** Shorthand: renders a CardHeader and wraps children in CardBody. */
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  titleAs?: HeadingLevel;
  /** Shorthand: footer content. */
  footer?: ReactNode;
  /** Remove body padding (e.g. for a full-bleed table). */
  flush?: boolean;
  /** Interactive cards lift slightly on hover. */
  interactive?: boolean;
};

export function Card({
  title,
  description,
  actions,
  titleAs = "h2",
  footer,
  flush,
  interactive,
  className,
  children,
  ...props
}: CardProps) {
  const id = useId();
  const shorthand = title !== undefined || actions !== undefined || footer !== undefined;
  return (
    <section
      aria-labelledby={title !== undefined ? id : undefined}
      data-interactive={interactive ? "" : undefined}
      className={cx(styles.card, className)}
      {...props}
    >
      {shorthand ? (
        <>
          {(title !== undefined || actions !== undefined) && (
            <CardHeader title={title} titleId={id} description={description} actions={actions} titleAs={titleAs} />
          )}
          <CardBody flush={flush}>{children}</CardBody>
          {footer && <CardFooter>{footer}</CardFooter>}
        </>
      ) : (
        children
      )}
    </section>
  );
}

export type CardHeaderProps = {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  titleAs?: HeadingLevel;
  titleId?: string;
  className?: string;
};

export function CardHeader({ title, description, actions, titleAs: Heading = "h2", titleId, className }: CardHeaderProps) {
  return (
    <header className={cx(styles.header, className)}>
      <div className={styles.heading}>
        {title !== undefined && (
          <Heading id={titleId} className={styles.title}>
            {title}
          </Heading>
        )}
        {description && <p className={styles.description}>{description}</p>}
      </div>
      {actions && <div className={styles.actions}>{actions}</div>}
    </header>
  );
}

export type CardBodyProps = ComponentPropsWithRef<"div"> & { flush?: boolean };

export function CardBody({ flush, className, ...props }: CardBodyProps) {
  return <div data-flush={flush ? "" : undefined} className={cx(styles.body, className)} {...props} />;
}

export function CardFooter({ className, ...props }: ComponentPropsWithRef<"div">) {
  return <div className={cx(styles.footer, className)} {...props} />;
}

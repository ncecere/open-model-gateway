/*
 * Checklist: setup steps with done states, a progress bar ("2 of 6 done") and
 * an action per open step. The first open step is current; its action is the
 * primary button. Done state is an icon plus screen-reader text, not colour.
 *
 * Provenance: adapted from Grounded web/src/components/ui/checklist/checklist.tsx
 * (read-only reference). OMG's vendored Bitop set has no Checklist or Progress
 * primitive, so this custom template draws the bar itself (role="progressbar").
 */
import { Check, X } from "lucide-react";
import { useId, type ReactElement, type ReactNode } from "react";
import { Button, IconButton } from "../ui/button/button";
import styles from "./checklist.module.css";

export type ChecklistStep = {
  id: string;
  title: ReactNode;
  description?: ReactNode;
  done: boolean;
  /** A link (render) or button (onClick) doing the step; hidden once done or when absent (read-only viewers). */
  action?: { label: string; render?: ReactElement; onClick?: () => void };
};

/** `embedded`: inside a card that already has a title; no own surface, the title becomes the accessible name only. */
/** `actionVariant="secondary"` when the page header already holds the next step as its one primary button. */
/** `onDismiss` adds a × button (named `dismissLabel`, default "Dismiss <title>"). */
export function Checklist({ title, description, steps, onDismiss, dismissLabel, headingLevel = 2, complete, embedded = false, actionVariant = "auto" }: { title: string; description?: ReactNode; steps: ChecklistStep[]; onDismiss?: () => void; dismissLabel?: string; headingLevel?: 2 | 3; complete?: ReactNode; embedded?: boolean; actionVariant?: "auto" | "secondary" }) {
  const titleId = useId(), Heading = `h${headingLevel}` as const;
  const done = steps.filter(step => step.done).length, total = steps.length, current = steps.find(step => !step.done)?.id;
  const summary = `${done} of ${total} done`, percent = total ? Math.round(done / total * 100) : 0;
  return <section aria-labelledby={titleId} className={embedded ? styles.embedded : styles.root} data-complete={done === total ? "" : undefined}>
    {embedded ? <span id={titleId} className={styles.srOnly}>{title}</span> : <div className={styles.header}><div className={styles.heading}><Heading id={titleId} className={styles.title}>{title}</Heading>{description && <p className={styles.description}>{description}</p>}</div>{onDismiss && <IconButton size="sm" variant="ghost" icon={<X aria-hidden />} label={dismissLabel ?? `Dismiss ${title}`} onClick={onDismiss} />}</div>}
    {done === total && complete ? <p className={styles.summary}>{complete}</p> : <div className={styles.progress}><span className={styles.summary} id={`${titleId}-summary`}>{summary}</span><div role="progressbar" aria-labelledby={`${titleId}-summary`} aria-valuemin={0} aria-valuemax={total} aria-valuenow={done} aria-valuetext={summary} className={styles.track}><span className={styles.bar} style={{ width: `${percent}%` }} /></div></div>}
    <ol className={styles.steps}>{steps.map((step, index) => <li key={step.id} className={styles.step} data-done={step.done ? "" : undefined} data-current={step.id === current ? "" : undefined}>
      <span aria-hidden className={styles.marker}>{step.done ? <Check /> : index + 1}</span>
      <div className={styles.body}><p className={styles.stepTitle}><span className={styles.srOnly}>{step.done ? "Done" : "To do"}: </span>{step.title}</p>{step.description && <p className={styles.stepDescription}>{step.description}</p>}</div>
      {!step.done && step.action && <div className={styles.action}><Button size="sm" variant={step.id === current && actionVariant === "auto" ? "primary" : "secondary"} onClick={step.action.onClick} render={step.action.render}>{step.action.label}</Button></div>}
    </li>)}</ol>
  </section>;
}

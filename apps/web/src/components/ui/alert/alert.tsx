"use client";

import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from "lucide-react";
import type { ComponentPropsWithRef, ReactNode } from "react";
import { Button } from "@/components/ui/button/button";
import { cx } from "@/lib/bitop-utils";
import styles from "./alert.module.css";

export type AlertTone = "info" | "success" | "warning" | "danger";

export type AlertProps = Omit<ComponentPropsWithRef<"div">, "title"> & {
  tone?: AlertTone;
  title?: ReactNode;
  /** Trailing actions (buttons/links). */
  actions?: ReactNode;
  /** Shows a dismiss button. */
  onDismiss?: () => void;
  /** Hide the leading icon. */
  hideIcon?: boolean;
};

const icons = { info: Info, success: CircleCheck, warning: TriangleAlert, danger: CircleAlert } as const;

/**
 * An inline message. Danger alerts use role="alert" (assertive); the other
 * tones use role="status" (polite). Mount the alert when the message
 * appears so it is announced.
 */
export function Alert({ tone = "info", title, actions, onDismiss, hideIcon, className, children, ...props }: AlertProps) {
  const Icon = icons[tone];
  return (
    <div role={tone === "danger" ? "alert" : "status"} data-tone={tone} className={cx(styles.alert, className)} {...props}>
      {!hideIcon && <Icon aria-hidden className={styles.icon} />}
      <div className={styles.body}>
        {title && <p className={styles.title}>{title}</p>}
        {children && <div className={styles.content}>{children}</div>}
      </div>
      {actions && <div className={styles.actions}>{actions}</div>}
      {onDismiss && (
        <button type="button" className={styles.dismiss} onClick={onDismiss} aria-label="Dismiss">
          <X aria-hidden />
        </button>
      )}
    </div>
  );
}

/** Extracts a human-readable message from an unknown error value. */
export function errorMessage(error: unknown): string {
  if (!error) return "";
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && typeof (error as { message?: unknown }).message === "string") {
    return (error as { message: string }).message;
  }
  const text = String(error);
  return text === "[object Object]" ? "" : text;
}

/** How an error is presented: what ErrorAlert shows by default. */
export type ErrorDescription = {
  title?: string;
  message: string;
  tone: AlertTone;
  /** The HTTP status found on the error, if any. */
  status?: number;
};

/** Reads a numeric HTTP status from `error.status` or `error.response.status` (fetch wrappers, axios, ky…). */
export function errorStatus(error: unknown): number | undefined {
  if (!error || typeof error !== "object") return undefined;
  const e = error as { status?: unknown; response?: { status?: unknown } | null };
  if (typeof e.status === "number") return e.status;
  const nested = e.response && typeof e.response === "object" ? e.response.status : undefined;
  return typeof nested === "number" ? nested : undefined;
}

const STATUS_TEXT: Record<number, { title: string; message: string; tone?: AlertTone }> = {
  401: { title: "You're signed out", message: "Sign in again to continue." },
  403: { title: "You don't have access", message: "Ask an administrator for access if you need it." },
  404: { title: "Not found", message: "It may have been moved or deleted." },
  408: { title: "The request timed out", message: "Check your connection and try again." },
  429: { title: "Too many requests", message: "Wait a moment and try again.", tone: "warning" },
  504: { title: "The request timed out", message: "The server took too long to respond. Try again." },
};

/**
 * Maps an unknown error to a title, message and tone. Errors with an HTTP
 * status get a plain-language title (401, 403, 404, 408/504, 429 as a
 * warning, 5xx); the error's own message is kept, or a default is used when
 * it has none. Everything else is a danger alert with the message only.
 */
export function describeError(error: unknown): ErrorDescription {
  const message = errorMessage(error);
  const status = errorStatus(error);
  const fallback = "Something went wrong.";
  if (status === undefined) return { message: message || fallback, tone: "danger" };
  const known = STATUS_TEXT[status] ?? (status >= 500 && status <= 599 ? { title: "Something went wrong on our side", message: "Try again in a moment." } : undefined);
  if (!known) return { message: message || fallback, tone: "danger", status };
  return { title: known.title, message: message || known.message, tone: known.tone ?? "danger", status };
}

export type ErrorAlertProps = Omit<AlertProps, "tone" | "children"> & {
  error: unknown;
  /** Overrides the tone from `describe` / `describeError`. */
  tone?: AlertTone;
  /** App-specific mapping merged over `describeError`, e.g. quota errors as warnings. */
  describe?: (error: unknown) => Partial<Omit<ErrorDescription, "status">> | undefined;
  /** Shows a retry button in the actions slot. */
  onRetry?: () => void;
  /** Default "Try again". */
  retryLabel?: ReactNode;
  /** Puts the retry button in its loading state (e.g. while a query refetches). */
  retrying?: boolean;
};

/**
 * Renders nothing when `error` is falsy; otherwise an Alert whose title,
 * message and tone come from `describeError`, then `describe`, then the
 * explicit `title` / `tone` props (last wins).
 */
export function ErrorAlert({ error, tone, title, describe, onRetry, retryLabel = "Try again", retrying, actions, ...props }: ErrorAlertProps) {
  if (!error) return null;
  const d = { ...describeError(error), ...stripUndefined(describe?.(error)) };
  const retry = onRetry && (
    <Button size="sm" variant="secondary" loading={retrying} onClick={onRetry}>
      {retryLabel}
    </Button>
  );
  return (
    <Alert
      tone={tone ?? d.tone}
      title={title ?? d.title}
      actions={
        retry || actions ? (
          <>
            {retry}
            {actions}
          </>
        ) : undefined
      }
      {...props}
    >
      {d.message}
    </Alert>
  );
}

function stripUndefined<T extends object>(value: T | undefined): Partial<T> {
  if (!value) return {};
  return Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined)) as Partial<T>;
}

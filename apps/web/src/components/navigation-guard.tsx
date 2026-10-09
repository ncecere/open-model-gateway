/*
 * Unsaved changes: ONE confirmation everywhere (route leave, dialog close, tab switch) with the same words,
 * "Discard unsaved changes?" / "Keep editing" / "Discard". Only the one-line description may name what is lost.
 */
import type { ReactNode } from "react";
import { useBlocker, useRouter } from "@tanstack/react-router";
import { AlertDialog } from "./ui/dialog/dialog";

export const discardCopy = { title: "Discard unsaved changes?", description: "Your changes haven't been saved.", confirm: "Discard", cancel: "Keep editing" } as const;

/** The shared "Discard unsaved changes?" confirmation. `onDiscard` runs on Discard; Keep editing just closes it. */
export function DiscardChangesDialog({ open, onOpenChange, onDiscard, description = discardCopy.description }: { open: boolean; onOpenChange: (open: boolean) => void; onDiscard: () => void; /** One short line naming what is lost (optional). */ description?: ReactNode }) {
  return <AlertDialog open={open} onOpenChange={onOpenChange} title={discardCopy.title} description={description} confirmLabel={discardCopy.confirm} cancelLabel={discardCopy.cancel} onConfirm={onDiscard} />;
}

export function NavigationGuard({ dirty, allow }: { dirty: boolean; allow?: () => boolean }) { const router = useRouter({ warn: false }); return router ? <Blocking dirty={dirty} allow={allow} /> : null; }
function Blocking({ dirty, allow }: { dirty: boolean; allow?: () => boolean }) {
  const blocker = useBlocker({ shouldBlockFn: () => dirty && !allow?.(), enableBeforeUnload: dirty, withResolver: true });
  return <DiscardChangesDialog open={blocker.status === "blocked"} onOpenChange={open => { if (!open && blocker.status === "blocked") blocker.reset(); }} onDiscard={() => { if (blocker.status === "blocked") blocker.proceed(); }} />;
}

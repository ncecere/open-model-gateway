import { useBlocker, useRouter } from "@tanstack/react-router";
import { AlertDialog } from "./ui/dialog/dialog";
export function NavigationGuard({ dirty, allow }: { dirty: boolean; allow?: () => boolean }) { const router = useRouter({ warn: false }); return router ? <Blocking dirty={dirty} allow={allow} /> : null; }
function Blocking({ dirty, allow }: { dirty: boolean; allow?: () => boolean }) {
  const blocker = useBlocker({ shouldBlockFn: () => dirty && !allow?.(), enableBeforeUnload: dirty, withResolver: true });
  return <AlertDialog open={blocker.status === "blocked"} onOpenChange={open => { if (!open && blocker.status === "blocked") blocker.reset(); }} title="Leave without saving?" description="Unsaved changes will be discarded. Any in-flight request will be aborted when you leave." confirmLabel="Leave without saving" cancelLabel="Keep editing" onConfirm={() => { if (blocker.status === "blocked") blocker.proceed(); }} />;
}

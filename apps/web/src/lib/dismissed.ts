/*
 * Optional explanatory notes the reader can dismiss, remembered per user in
 * localStorage (UI principle: everything optional is dismissable). Storage is
 * optional: when it is unavailable the note is simply shown again next time.
 */
import { useState } from "react";

export const dismissedKey = (note: string, user: string) => `omg.enterprise.dismissed.${note}:${user}`;

export function readDismissed(storage: Pick<Storage, "getItem"> | undefined, note: string, user: string): boolean {
  try { return storage?.getItem(dismissedKey(note, user)) === "1"; } catch { return false; }
}

/** `[dismissed, dismiss]` for one note and user. */
export function useDismissed(note: string, user: string): [boolean, () => void] {
  const storage = typeof window === "undefined" ? undefined : window.localStorage;
  const [dismissed, setDismissed] = useState(() => readDismissed(storage, note, user));
  const dismiss = () => { try { storage?.setItem(dismissedKey(note, user), "1"); } catch { /* Storage is optional. */ } setDismissed(true); };
  return [dismissed, dismiss];
}

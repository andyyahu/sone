import { useCallback, useEffect, useLayoutEffect, useRef } from "react";
import { registerDismissable, DISMISS_PRIORITY } from "../lib/dismissStack";

// Registers while `active` — not merely while mounted — so a layer that is
// present but hidden (a minimized video) doesn't swallow Escape. The callback
// lives in a ref so an inline arrow doesn't re-register on every render.
export function useEscapeDismiss(
  active: boolean,
  onClose: () => void,
  priority: number = DISMISS_PRIORITY.modal,
): () => boolean {
  const onCloseRef = useRef(onClose);
  const registration = useRef<ReturnType<typeof registerDismissable> | null>(
    null,
  );
  // Layout-phase so an Escape between render and passive flush can't run a stale closure.
  useLayoutEffect(() => {
    onCloseRef.current = onClose;
  });

  useEffect(() => {
    if (!active) return;
    const entry = registerDismissable(priority, () => onCloseRef.current());
    registration.current = entry;
    return () => {
      entry();
      if (registration.current === entry) registration.current = null;
    };
  }, [active, priority]);
  return useCallback(() => registration.current?.isTop() ?? false, []);
}

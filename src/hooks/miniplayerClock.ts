import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import { useDocumentVisible } from "./useDocumentVisible";

/** Position lives outside the player tree; only its progress leaf subscribes. */
export function createMiniplayerClock() {
  let anchor = { position: 0, time: performance.now(), playing: false };
  const listeners = new Set<() => void>();
  const getPosition = () =>
    anchor.position +
    (anchor.playing ? Math.max(0, performance.now() - anchor.time) / 1000 : 0);
  const setAnchor = (position: number, playing: boolean) => {
    if (!playing && !anchor.playing && position === anchor.position) return;
    anchor = { position, playing, time: performance.now() };
    listeners.forEach((listener) => listener());
  };

  return {
    getPosition,
    getSnapshot: () => anchor,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    setAnchor,
    setPlaying: (playing: boolean) => {
      if (playing !== anchor.playing) setAnchor(getPosition(), playing);
    },
    seek: (position: number) => setAnchor(position, anchor.playing),
  };
}

export type MiniplayerClock = ReturnType<typeof createMiniplayerClock>;

/** A small progress bar needs four updates/second, only while it can be seen. */
export function useMiniplayerPosition(clock: MiniplayerClock, active: boolean) {
  const visible = useDocumentVisible();
  const enabled = active && visible;
  const subscribe = useCallback(
    (listener: () => void) => (enabled ? clock.subscribe(listener) : () => {}),
    [clock, enabled],
  );
  const anchor = useSyncExternalStore(subscribe, clock.getSnapshot);
  const [, tick] = useState(0);
  useEffect(() => {
    if (!enabled || !anchor.playing) return;
    const timer = setInterval(() => tick((value) => value + 1), 250);
    return () => clearInterval(timer);
  }, [enabled, anchor.playing]);
  return clock.getPosition();
}

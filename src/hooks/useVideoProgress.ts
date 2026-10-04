import { useCallback, useEffect, useState, type RefObject } from "react";
import { useDocumentVisible } from "./useDocumentVisible";

const sourceListeners = new Set<() => void>();

/** The player bar can mount before VideoPlayer publishes its shared element. */
export function notifyVideoProgressSourceChanged() {
  sourceListeners.forEach((listener) => listener());
}

/** Read native media events immediately; poll only a visible, playing scrubber. */
export function useVideoProgress(
  videoRef: RefObject<HTMLVideoElement | null>,
  active: boolean,
  isDraggingRef: RefObject<boolean>,
) {
  const documentVisible = useDocumentVisible();
  const enabled = active && documentVisible;
  const [progress, setProgress] = useState({ position: 0, duration: 0 });
  const setPosition = useCallback((position: number) => {
    setProgress((previous) =>
      previous.position === position ? previous : { ...previous, position },
    );
  }, []);

  useEffect(() => {
    if (!enabled) return;
    let video: HTMLVideoElement | null = null;
    let timer: ReturnType<typeof setInterval> | undefined;
    let disconnect = () => {};

    const stop = () => {
      clearInterval(timer);
      timer = undefined;
    };
    const sync = () => {
      if (!video || isDraggingRef.current) return;
      const currentTime = video.currentTime;
      const mediaDuration = video.duration;
      const position = Number.isFinite(currentTime) ? currentTime : 0;
      const duration = Number.isFinite(mediaDuration) ? mediaDuration : 0;
      setProgress((previous) =>
        previous.position === position && previous.duration === duration
          ? previous
          : { position, duration },
      );
    };
    const refresh = () => {
      stop();
      sync();
      if (video && !video.paused && !video.ended) {
        timer = setInterval(sync, 150);
      }
    };
    const suspend = () => {
      stop();
      sync();
    };
    const connect = () => {
      if (videoRef.current === video) return;
      disconnect();
      video = videoRef.current;
      if (!video) {
        setProgress({ position: 0, duration: 0 });
        return;
      }
      const element = video;
      const refreshEvents = [
        "play",
        "playing",
        "pause",
        "ended",
        "loadedmetadata",
        "durationchange",
        "seeking",
        "seeked",
      ];
      const suspendEvents = ["waiting", "emptied", "error"];
      refreshEvents.forEach((event) =>
        element.addEventListener(event, refresh),
      );
      suspendEvents.forEach((event) =>
        element.addEventListener(event, suspend),
      );
      disconnect = () => {
        stop();
        refreshEvents.forEach((event) =>
          element.removeEventListener(event, refresh),
        );
        suspendEvents.forEach((event) =>
          element.removeEventListener(event, suspend),
        );
      };
      refresh();
    };

    sourceListeners.add(connect);
    connect();
    return () => {
      sourceListeners.delete(connect);
      disconnect();
    };
  }, [enabled, videoRef, isDraggingRef]);

  return { ...progress, setPosition };
}

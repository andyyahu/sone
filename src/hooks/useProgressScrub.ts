import { useState, useEffect, useRef, useCallback } from "react";
import { useAtomValue } from "jotai";
import { currentTrackAtom, isPlayingAtom } from "../atoms/playback";
import { usePlaybackActions } from "./usePlaybackActions";
import { getInterpolatedPosition, notifySeek } from "../lib/playbackPosition";
import { useDocumentVisible } from "./useDocumentVisible";

interface UseProgressScrubOptions {
  /** False while this scrubber is covered or its controls are hidden. */
  active?: boolean;
  /** Notify the parent when dragging changes (for auto-hide). */
  onDraggingChange?: (dragging: boolean) => void;
  /** Callback when drag ends (for resetting auto-hide timer) */
  onDragEnd?: () => void;
}

export function useProgressScrub(options?: UseProgressScrubOptions) {
  const currentTrack = useAtomValue(currentTrackAtom);
  const isPlaying = useAtomValue(isPlayingAtom);
  const { seekTo } = usePlaybackActions();
  const documentVisible = useDocumentVisible();
  const active = (options?.active ?? true) && documentVisible;
  const trackId = currentTrack?.id;

  // Destructure options so callback dependencies remain stable.
  const onDraggingChange = options?.onDraggingChange;
  const onDragEnd = options?.onDragEnd;

  const [currentTime, setCurrentTime] = useState(0);
  const [isDragging, setIsDragging] = useState(false);
  const [dragTime, setDragTime] = useState(0);
  const [isHoveringProgress, setIsHoveringProgress] = useState(false);
  const progressRef = useRef<HTMLDivElement>(null);

  // Sync progress with interpolated position (no IPC per tick)
  useEffect(() => {
    if (!active || isDragging) return;
    if (trackId === undefined) {
      setCurrentTime(0);
      return;
    }

    const syncPosition = () => {
      setCurrentTime(getInterpolatedPosition());
    };

    syncPosition();
    window.addEventListener("playback-seeked", syncPosition);
    const interval = isPlaying ? setInterval(syncPosition, 500) : undefined;
    return () => {
      clearInterval(interval);
      window.removeEventListener("playback-seeked", syncPosition);
    };
  }, [active, isPlaying, trackId, isDragging]);

  const duration = currentTrack?.duration ?? 0;
  const rawTime = isDragging ? dragTime : currentTime;
  // Clamp to duration: interpolation can briefly overshoot near the track end
  // (before the track-finished event lands), which would show e.g. "3:01" on a
  // 3:00 track.
  const displayTime = duration > 0 ? Math.min(rawTime, duration) : rawTime;
  const progress = duration > 0 ? (displayTime / duration) * 100 : 0;
  const clampedProgress = Math.min(100, Math.max(0, progress));

  const getTimeFromClientX = useCallback(
    (clientX: number) => {
      if (!progressRef.current || !currentTrack) return 0;
      const el = progressRef.current;
      const rect = el.getBoundingClientRect();

      let adjustedX = clientX;
      const cssWidth = el.offsetWidth;
      if (cssWidth > 0 && Math.abs(rect.width / cssWidth - 1) < 0.01) {
        const zoom = parseFloat(document.documentElement.style.zoom || "1");
        if (zoom !== 1) {
          adjustedX = clientX / zoom;
        }
      }

      const pct = Math.max(
        0,
        Math.min(1, (adjustedX - rect.left) / rect.width),
      );
      return pct * currentTrack.duration;
    },
    [currentTrack],
  );

  const handleProgressMouseDown = useCallback(
    (e: React.MouseEvent) => {
      if (!currentTrack) return;
      e.preventDefault();
      onDraggingChange?.(true);
      const startTime = getTimeFromClientX(e.clientX);
      setIsDragging(true);
      setDragTime(startTime);

      const onMove = (ev: MouseEvent) => {
        setDragTime(getTimeFromClientX(ev.clientX));
      };

      const onUp = async (ev: MouseEvent) => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        const finalTime = getTimeFromClientX(ev.clientX);
        setCurrentTime(finalTime);
        setIsDragging(false);
        onDraggingChange?.(false);
        onDragEnd?.();
        notifySeek(finalTime);
        await seekTo(finalTime);
      };

      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    },
    [currentTrack, getTimeFromClientX, seekTo, onDraggingChange, onDragEnd],
  );

  return {
    progressRef,
    currentTrack,
    displayTime,
    duration,
    clampedProgress,
    isDragging,
    isHoveringProgress,
    setIsHoveringProgress,
    handleProgressMouseDown,
  };
}

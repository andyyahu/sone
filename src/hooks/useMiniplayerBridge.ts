import { useState, useEffect, useCallback, useRef } from "react";
import { emitTo } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { MiniplayerState } from "./useMiniplayerEmitter";
import { createMiniplayerClock } from "./miniplayerClock";

type DisplayState = Omit<MiniplayerState, "position">;

// IPC creates fresh track/source objects on every heartbeat. Position-only
// updates should re-anchor the progress leaf without rendering the player tree.
function sameDisplayState(a: DisplayState, b: DisplayState) {
  const ta = a.track;
  const tb = b.track;
  return (
    a.isPlaying === b.isPlaying &&
    a.duration === b.duration &&
    a.isFavorite === b.isFavorite &&
    a.shuffle === b.shuffle &&
    a.repeat === b.repeat &&
    a.volume === b.volume &&
    a.bitPerfect === b.bitPerfect &&
    a.volumeLock === b.volumeLock &&
    a.accentColor === b.accentColor &&
    a.error === b.error &&
    a.playbackSourceLabel?.type === b.playbackSourceLabel?.type &&
    a.playbackSourceLabel?.id === b.playbackSourceLabel?.id &&
    a.playbackSourceLabel?.name === b.playbackSourceLabel?.name &&
    ta?.id === tb?.id &&
    ta?.title === tb?.title &&
    ta?.version === tb?.version &&
    ta?.artist.id === tb?.artist.id &&
    ta?.artist.name === tb?.artist.name &&
    ta?.album.id === tb?.album.id &&
    ta?.album.cover === tb?.album.cover &&
    ta?.album.vibrantColor === tb?.album.vibrantColor &&
    ta?.artists?.length === tb?.artists?.length &&
    (ta?.artists?.every(
      (artist, index) =>
        artist.id === tb?.artists?.[index]?.id &&
        artist.name === tb?.artists?.[index]?.name,
    ) ??
      true)
  );
}

export function useMiniplayerBridge() {
  const [state, setState] = useState<DisplayState>({
    track: null,
    isPlaying: false,
    duration: 0,
    isFavorite: false,
    shuffle: false,
    repeat: 0,
    volume: 1,
    playbackSourceLabel: null,
    bitPerfect: false,
    accentColor: "#A855F7",
  });
  const [positionClock] = useState(createMiniplayerClock);
  const displayStateRef = useRef(state);
  const seekUntil = useRef(0);
  const lastAnchoredTrackId = useRef<number | null>(null);
  const authoritativeAnchor = useRef({
    position: 0,
    time: 0,
    playing: false,
  });
  const optimisticPlayRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const volumeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [optimisticPlaying, setOptimisticPlaying] = useState<boolean | null>(
    null,
  );
  const optimisticRef = useRef<boolean | null>(null);
  const isPlayingRef = useRef(false);

  useEffect(() => {
    let active = true;
    const unlisten = getCurrentWindow().listen<MiniplayerState>(
      "miniplayer-state-update",
      (event) => {
        if (!active) return;
        const { position, ...displayState } = event.payload;
        const now = performance.now();
        authoritativeAnchor.current = {
          position,
          time: now,
          playing: displayState.isPlaying,
        };
        isPlayingRef.current = displayState.isPlaying;

        // A new track must always reset a recent optimistic seek target.
        const trackChanged =
          (displayState.track?.id ?? null) !== lastAnchoredTrackId.current;
        lastAnchoredTrackId.current = displayState.track?.id ?? null;
        if (!trackChanged && now < seekUntil.current) {
          positionClock.setPlaying(displayState.isPlaying);
        } else {
          if (trackChanged) seekUntil.current = 0;
          positionClock.setAnchor(position, displayState.isPlaying);
        }

        // Any authoritative response (including a heartbeat after a rejected
        // toggle) reconciles both the transport icon and the position clock.
        if (optimisticPlayRef.current) clearTimeout(optimisticPlayRef.current);
        optimisticPlayRef.current = null;
        if (optimisticRef.current !== null) {
          optimisticRef.current = null;
          setOptimisticPlaying(null);
        }
        if (!sameDisplayState(displayStateRef.current, displayState)) {
          displayStateRef.current = displayState;
          setState(displayState);
        }
      },
    );
    emitTo("main", "miniplayer-ready", {}).catch(() => {});

    return () => {
      active = false;
      unlisten.then((fn) => fn());
      if (optimisticPlayRef.current) clearTimeout(optimisticPlayRef.current);
      if (volumeTimerRef.current) clearTimeout(volumeTimerRef.current);
    };
  }, [positionClock]);

  const sendCommand = useCallback(
    (action: string, value?: number) => {
      if (action === "toggle-play") {
        const playing = !(optimisticRef.current ?? isPlayingRef.current);
        optimisticRef.current = playing;
        setOptimisticPlaying(playing);
        positionClock.setPlaying(playing);
        if (optimisticPlayRef.current) clearTimeout(optimisticPlayRef.current);
        optimisticPlayRef.current = setTimeout(() => {
          optimisticPlayRef.current = null;
          optimisticRef.current = null;
          setOptimisticPlaying(null);
          const anchor = authoritativeAnchor.current;
          positionClock.setAnchor(
            anchor.position +
              (anchor.playing ? (performance.now() - anchor.time) / 1000 : 0),
            anchor.playing,
          );
        }, 2000);
      }
      if (action === "seek" && value !== undefined) {
        positionClock.seek(value);
        seekUntil.current = performance.now() + 500;
      }
      emitTo("main", "miniplayer-command", { action, value }).catch(() => {});
    },
    [positionClock],
  );

  const sendVolume = useCallback(
    (vol: number) => {
      if (volumeTimerRef.current) clearTimeout(volumeTimerRef.current);
      volumeTimerRef.current = setTimeout(() => {
        volumeTimerRef.current = null;
        sendCommand("set-volume", vol);
      }, 50);
    },
    [sendCommand],
  );

  return {
    state,
    positionClock,
    isPlaying: optimisticPlaying ?? state.isPlaying,
    sendCommand,
    sendVolume,
  };
}

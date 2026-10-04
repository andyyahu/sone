import { useEffect } from "react";
import { useStore } from "jotai";
import { isAuthenticatedAtom } from "../atoms/auth";
import {
  currentTrackAtom,
  queueAtom,
  historyAtom,
  manualQueueAtom,
  originalQueueAtom,
  playbackSourceAtom,
  contextSourceAtom,
} from "../atoms/playback";
import { loadPlaybackQueue, savePlaybackQueue } from "../api/tidal";
import type { PlaybackSnapshot, QueuedTrack, Track } from "../types";
import { advanceCounterPast, ensureQid } from "../lib/qid";
import {
  createPlaybackPersistence,
  MAX_HISTORY_TRACKS,
  PLAYBACK_STATE_KEY,
  readPendingPlaybackSnapshot,
} from "../lib/playbackPersistence";

const playbackAtoms = [
  currentTrackAtom,
  queueAtom,
  historyAtom,
  manualQueueAtom,
  originalQueueAtom,
  playbackSourceAtom,
  contextSourceAtom,
] as const;

/** Subscribe without React renders; capture large queues after interaction. */
export function usePlaybackPersistence() {
  const store = useStore();

  useEffect(() => {
    let cancelled = false;
    let signedOut = false;
    let wasAuthenticated = store.get(isAuthenticatedAtom);
    let restoring = false;
    // Once superseded, a pending startup read must never cross a logout or
    // overwrite a queue the user edited, even if they sign in again meanwhile.
    let restoreObsolete = false;
    const persistence = createPlaybackPersistence({
      readSnapshot: () => ({
        currentTrack: store.get(currentTrackAtom),
        queue: store.get(queueAtom),
        history: store.get(historyAtom),
        manualQueue: store.get(manualQueueAtom),
        originalQueue: store.get(originalQueueAtom),
        playbackSource: store.get(playbackSourceAtom),
        contextSource: store.get(contextSourceAtom),
      }),
      storage: localStorage,
      saveBackend: savePlaybackQueue,
      onError: (message, error) => console.error(message, error),
    });

    const subscriptions = playbackAtoms.map((atom) =>
      store.sub(atom, () => {
        if (restoring || signedOut) return;
        restoreObsolete = true;
        persistence.markDirty();
      }),
    );
    subscriptions.push(
      store.sub(isAuthenticatedAtom, () => {
        const authenticated = store.get(isAuthenticatedAtom);
        if (wasAuthenticated && !authenticated) {
          signedOut = true;
          restoreObsolete = true;
          // Also discard hidden original/source queues left behind by logout.
          store.set(currentTrackAtom, null);
          store.set(queueAtom, []);
          store.set(historyAtom, []);
          store.set(manualQueueAtom, []);
          store.set(originalQueueAtom, null);
          store.set(playbackSourceAtom, null);
          store.set(contextSourceAtom, null);
          void persistence.clear();
        } else if (authenticated) {
          signedOut = false;
        }
        wasAuthenticated = authenticated;
      }),
    );

    const flush = () => {
      void persistence.flush();
    };
    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") flush();
    };
    window.addEventListener("pagehide", flush);
    document.addEventListener("visibilitychange", onVisibilityChange);

    const restoreSnapshot = (raw: string) => {
      const parsed = JSON.parse(raw) as Partial<PlaybackSnapshot>;
      const validTracks = (items: unknown): Track[] =>
        Array.isArray(items)
          ? items.filter((t): t is Track => !!t && typeof t.id === "number")
          : [];
      const queue = validTracks(parsed.queue);
      const history = validTracks(parsed.history).slice(-MAX_HISTORY_TRACKS);
      const originalQueue = validTracks(parsed.originalQueue);
      const manualQueue = validTracks(parsed.manualQueue);
      const sourceTracks = validTracks(parsed.playbackSource?.tracks);
      const contextTracks = validTracks(parsed.contextSource?.tracks);
      const currentTrack = validTracks([parsed.currentTrack])[0] ?? null;
      // Reserve existing IDs before assigning IDs to older snapshots.
      advanceCounterPast([
        ...queue,
        ...history,
        ...originalQueue,
        ...manualQueue,
        ...sourceTracks,
        ...contextTracks,
        ...(currentTrack ? [currentTrack] : []),
      ] as QueuedTrack[]);
      const restoreTrack = (track: Track) => {
        const copy = { ...track } as Track & {
          _playingFrom?: unknown;
          _contextFrom?: unknown;
        };
        delete copy._playingFrom;
        delete copy._contextFrom;
        return ensureQid(copy);
      };
      restoring = true;
      try {
        store.set(
          currentTrackAtom,
          currentTrack ? restoreTrack(currentTrack) : null,
        );
        store.set(queueAtom, queue.map(restoreTrack));
        store.set(historyAtom, history.map(restoreTrack));
        store.set(manualQueueAtom, manualQueue.map(restoreTrack));
        store.set(
          originalQueueAtom,
          Array.isArray(parsed.originalQueue)
            ? originalQueue.map(restoreTrack)
            : null,
        );
        store.set(
          playbackSourceAtom,
          parsed.playbackSource
            ? {
                ...parsed.playbackSource,
                tracks: sourceTracks.map(restoreTrack),
              }
            : null,
        );
        store.set(
          contextSourceAtom,
          parsed.contextSource
            ? {
                ...parsed.contextSource,
                tracks: contextTracks.map(restoreTrack),
              }
            : null,
        );
      } finally {
        restoring = false;
      }
    };

    const restore = async () => {
      try {
        const pending = readPendingPlaybackSnapshot(localStorage);
        if (pending) {
          restoreSnapshot(pending.json);
          if (pending.cleared) void persistence.clear();
          else persistence.markDirty();
          return;
        }
      } catch (error) {
        console.error("Failed to recover pending playback state:", error);
      }
      try {
        const raw = await loadPlaybackQueue();
        if (cancelled || restoreObsolete) return;
        if (raw) {
          restoreSnapshot(raw);
          return;
        }
      } catch {
        // Backend unavailable or invalid — fall through to the local copy.
      }
      if (cancelled || restoreObsolete) return;
      try {
        const raw = localStorage.getItem(PLAYBACK_STATE_KEY);
        if (raw) restoreSnapshot(raw);
      } catch (error) {
        console.error("Failed to restore playback state:", error);
      }
    };
    void restore();

    return () => {
      cancelled = true;
      subscriptions.forEach((unsubscribe) => unsubscribe());
      window.removeEventListener("pagehide", flush);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      void persistence.dispose();
    };
  }, [store]);
}

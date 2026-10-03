import type { PlaybackSnapshot, Track } from "../types";

export const PLAYBACK_STATE_KEY = "sone.playback-state.v1";
const PENDING_STATE_KEY = "sone.playback-state-pending.v1";
export const MAX_HISTORY_TRACKS = 500;

const EMPTY_SNAPSHOT: PlaybackSnapshot = {
  currentTrack: null,
  queue: [],
  history: [],
  manualQueue: [],
  originalQueue: null,
  playbackSource: null,
  contextSource: null,
};
const EMPTY_JSON = JSON.stringify(EMPTY_SNAPSHOT);

type PersistenceStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** Recover the latest local snapshot if the WebView exited before disk caught up. */
export function readPendingPlaybackSnapshot(storage: PersistenceStorage) {
  const pending = storage.getItem(PENDING_STATE_KEY);
  if (pending === "cleared") return { json: EMPTY_JSON, cleared: true };
  const json =
    pending === "snapshot" ? storage.getItem(PLAYBACK_STATE_KEY) : null;
  return json ? { json, cleared: false } : null;
}

function serializeSnapshot(snapshot: PlaybackSnapshot) {
  // A track can occur in history, originalQueue and both source lists. Clone
  // it once per capture; do not cache between captures because metadata and
  // playback runtime fields can be mutated in place.
  const sanitized = new WeakMap<Track, Track>();
  const sanitize = (track: Track): Track => {
    const cached = sanitized.get(track);
    if (cached) return cached;
    const copy = { ...track } as Track & {
      _playingFrom?: unknown;
      _contextFrom?: unknown;
    };
    delete copy._playingFrom;
    delete copy._contextFrom;
    sanitized.set(track, copy);
    return copy;
  };
  const tracks = (items: Track[] = []) => items.map(sanitize);
  return JSON.stringify({
    currentTrack: snapshot.currentTrack
      ? sanitize(snapshot.currentTrack)
      : null,
    queue: tracks(snapshot.queue),
    history: tracks(snapshot.history.slice(-MAX_HISTORY_TRACKS)),
    manualQueue: tracks(snapshot.manualQueue),
    originalQueue: snapshot.originalQueue
      ? tracks(snapshot.originalQueue)
      : null,
    playbackSource: snapshot.playbackSource
      ? {
          ...snapshot.playbackSource,
          tracks: tracks(snapshot.playbackSource.tracks),
        }
      : null,
    contextSource: snapshot.contextSource
      ? {
          ...snapshot.contextSource,
          tracks: tracks(snapshot.contextSource.tracks),
        }
      : null,
  } satisfies PlaybackSnapshot);
}

interface PersistenceOptions {
  readSnapshot: () => PlaybackSnapshot;
  storage: PersistenceStorage;
  saveBackend: (json: string) => Promise<void>;
  onError: (message: string, error: unknown) => void;
}

/** Coalesce interactions before capturing, then write disk snapshots in order. */
export function createPlaybackPersistence({
  readSnapshot,
  storage,
  saveBackend,
  onError,
}: PersistenceOptions) {
  let dirty = false;
  let disposed = false;
  let captureTimer: ReturnType<typeof setTimeout> | null = null;
  let idleId: number | null = null;
  let backendTimer: ReturnType<typeof setTimeout> | null = null;
  let latestJson: string | null = null;
  let localJson: string | null = null;
  let savedJson: string | null = null;
  let pendingBackend: string | null = null;
  let writing: Promise<void> | null = null;

  const cancelCapture = () => {
    if (captureTimer !== null) clearTimeout(captureTimer);
    if (idleId !== null) window.cancelIdleCallback(idleId);
    captureTimer = null;
    idleId = null;
  };

  const cancelBackendTimer = () => {
    if (backendTimer !== null) clearTimeout(backendTimer);
    backendTimer = null;
  };

  const acknowledge = (json: string) => {
    // A newer snapshot or logout may have been recorded while this write ran.
    if (latestJson !== json) return;
    try {
      const pending = readPendingPlaybackSnapshot(storage);
      if (pending?.json !== json) return;
      storage.removeItem(PENDING_STATE_KEY);
    } catch (error) {
      onError("Failed to acknowledge playback queue save:", error);
    }
  };

  const writePending = (): Promise<void> => {
    if (writing) return writing;
    writing = (async () => {
      while (pendingBackend !== null) {
        const json = pendingBackend;
        pendingBackend = null;
        if (savedJson === json) {
          acknowledge(json);
          continue;
        }
        try {
          await saveBackend(json);
          savedJson = json;
          acknowledge(json);
        } catch (error) {
          // Retain latestJson and the recovery marker. The next change or
          // final flush retries; never busy-loop on an unavailable backend.
          onError("Failed to save playback queue to backend:", error);
        }
      }
    })().then(() => {
      writing = null;
      if (pendingBackend !== null) return writePending();
    });
    return writing;
  };

  const capture = () => {
    cancelCapture();
    if (!dirty) return;
    dirty = false;
    let json: string;
    try {
      json = serializeSnapshot(readSnapshot());
    } catch (error) {
      dirty = true;
      onError("Failed to serialize playback state:", error);
      return;
    }
    latestJson = json;
    if (localJson !== json) {
      try {
        storage.setItem(PLAYBACK_STATE_KEY, json);
        storage.setItem(PENDING_STATE_KEY, "snapshot");
        localJson = json;
      } catch (error) {
        onError("Failed to persist playback state:", error);
      }
    }
    pendingBackend = json;
    if (savedJson === json && !writing) {
      pendingBackend = null;
      cancelBackendTimer();
      acknowledge(json);
      return;
    }
    cancelBackendTimer();
    backendTimer = setTimeout(() => {
      backendTimer = null;
      void writePending();
    }, 2000);
  };

  const markDirty = () => {
    if (disposed) return;
    dirty = true;
    if (captureTimer !== null || idleId !== null) return;
    // Give input and the next paint time to finish. Do not reset this timer
    // on every edit, so a busy queue still gets periodic recovery snapshots.
    captureTimer = setTimeout(() => {
      captureTimer = null;
      if (typeof window.requestIdleCallback === "function") {
        idleId = window.requestIdleCallback(capture, { timeout: 1000 });
      } else {
        capture();
      }
    }, 200);
  };

  const flush = (): Promise<void> => {
    capture(); // synchronous local recovery copy, even if idle never ran
    cancelBackendTimer();
    if (latestJson !== null) pendingBackend = latestJson;
    return writePending();
  };

  const clear = (): Promise<void> => {
    cancelCapture();
    cancelBackendTimer();
    dirty = false;
    latestJson = EMPTY_JSON;
    localJson = null;
    try {
      // The tombstone also prevents a stale in-flight disk write from
      // resurrecting a signed-out user's queue on the next launch.
      storage.setItem(PENDING_STATE_KEY, "cleared");
      storage.removeItem(PLAYBACK_STATE_KEY);
    } catch (error) {
      onError("Failed to clear playback state:", error);
    }
    pendingBackend = EMPTY_JSON;
    return writePending();
  };

  return {
    markDirty,
    flush,
    clear,
    dispose: () => {
      disposed = true;
      return flush();
    },
  };
}

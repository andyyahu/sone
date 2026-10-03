import { act, cleanup, renderHook } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
import type { PlaybackSnapshot, PlaybackSource, Track } from "../types";
import {
  PLAYBACK_STATE_KEY,
  readPendingPlaybackSnapshot,
} from "../lib/playbackPersistence";
import { usePlaybackPersistence } from "./usePlaybackPersistence";

const { load, save } = vi.hoisted(() => ({ load: vi.fn(), save: vi.fn() }));
vi.mock("../api/tidal", () => ({
  loadPlaybackQueue: load,
  savePlaybackQueue: save,
}));

function snapshot(id = 1): PlaybackSnapshot {
  return {
    currentTrack: { id, title: `Track ${id}` } as Track,
    queue: [{ id: id + 1, title: "Next" } as Track],
    history: [],
    manualQueue: [],
    originalQueue: null,
    playbackSource: null,
    contextSource: null,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => {
    resolve = yes;
  });
  return { promise, resolve };
}

function setup(authenticated = false) {
  const store = createStore();
  store.set(isAuthenticatedAtom, authenticated);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  const hook = renderHook(() => usePlaybackPersistence(), { wrapper });
  return { store, ...hook };
}

beforeEach(() => {
  vi.useFakeTimers();
  localStorage.clear();
  load.mockReset().mockResolvedValue(null);
  save.mockReset().mockResolvedValue(undefined);
  vi.stubGlobal("requestIdleCallback", undefined);
  vi.stubGlobal("cancelIdleCallback", undefined);
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(async () => {
  cleanup();
  await Promise.resolve();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("playback persistence lifecycle", () => {
  it("restores while initial auth is false without clearing the saved queue", async () => {
    load.mockResolvedValueOnce(JSON.stringify(snapshot(4)));
    const { store } = setup();
    await act(async () => {});
    expect(store.get(currentTrackAtom)?.id).toBe(4);
    expect(store.get(queueAtom)[0].id).toBe(5);
    await vi.advanceTimersByTimeAsync(3000);
    expect(save).not.toHaveBeenCalled();
  });

  it("prefers the pending local snapshot and repairs an older disk copy", async () => {
    localStorage.setItem(PLAYBACK_STATE_KEY, JSON.stringify(snapshot(7)));
    localStorage.setItem("sone.playback-state-pending.v1", "snapshot");
    load.mockResolvedValueOnce(JSON.stringify(snapshot(1)));
    const { store } = setup();
    expect(store.get(currentTrackAtom)?.id).toBe(7);
    expect(load).not.toHaveBeenCalled();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2200);
    });
    expect(JSON.parse(save.mock.calls[0][0]).currentTrack.id).toBe(7);
    expect(readPendingPlaybackSnapshot(localStorage)).toBeNull();
  });

  it("honors a logout tombstone even if a stale disk queue exists", async () => {
    localStorage.setItem(PLAYBACK_STATE_KEY, JSON.stringify(snapshot(3)));
    localStorage.setItem("sone.playback-state-pending.v1", "cleared");
    load.mockResolvedValueOnce(JSON.stringify(snapshot(3)));
    const { store } = setup();
    await act(async () => {});
    expect(store.get(currentTrackAtom)).toBeNull();
    expect(store.get(queueAtom)).toEqual([]);
    expect(localStorage.getItem(PLAYBACK_STATE_KEY)).toBeNull();
    expect(load).not.toHaveBeenCalled();
    expect(JSON.parse(save.mock.calls[0][0]).currentTrack).toBeNull();
  });

  it.each([false, true])(
    "does not restore across logout (relogin: %s)",
    async (relogin) => {
      const pending = deferred<string>();
      load.mockReturnValueOnce(pending.promise);
      const { store } = setup(true);
      act(() => {
        store.set(isAuthenticatedAtom, false);
        if (relogin) store.set(isAuthenticatedAtom, true);
      });
      await act(async () => {
        pending.resolve(JSON.stringify(snapshot(42)));
      });
      expect(store.get(currentTrackAtom)).toBeNull();
      expect(store.get(queueAtom)).toEqual([]);
      await vi.advanceTimersByTimeAsync(3000);
      expect(save).toHaveBeenCalledTimes(1);
      expect(JSON.parse(save.mock.calls[0][0]).currentTrack).toBeNull();
    },
  );

  it("clears stale original/manual/source queues and pending captures on logout", async () => {
    const { store } = setup(true);
    await act(async () => {});
    const track = snapshot(5).currentTrack!;
    const source = {
      type: "playlist",
      id: "one",
      name: "One",
      tracks: [{ ...track, _qid: "q77" }],
    } satisfies PlaybackSource;
    act(() => {
      store.set(currentTrackAtom, track);
      store.set(queueAtom, [track]);
      store.set(historyAtom, [track]);
      store.set(manualQueueAtom, [track]);
      store.set(originalQueueAtom, [track]);
      store.set(playbackSourceAtom, source);
      store.set(contextSourceAtom, source);
      store.set(isAuthenticatedAtom, false);
    });
    await act(async () => {});
    expect(store.get(currentTrackAtom)).toBeNull();
    expect(store.get(queueAtom)).toEqual([]);
    expect(store.get(historyAtom)).toEqual([]);
    expect(store.get(manualQueueAtom)).toEqual([]);
    expect(store.get(originalQueueAtom)).toBeNull();
    expect(store.get(playbackSourceAtom)).toBeNull();
    expect(store.get(contextSourceAtom)).toBeNull();
    await vi.advanceTimersByTimeAsync(3000);
    expect(save).toHaveBeenCalledTimes(1);
    expect(localStorage.getItem(PLAYBACK_STATE_KEY)).toBeNull();
    // A fresh login is allowed to build and persist a new queue.
    act(() => {
      store.set(isAuthenticatedAtom, true);
      store.set(currentTrackAtom, snapshot(9).currentTrack);
    });
    await vi.advanceTimersByTimeAsync(2200);
    expect(JSON.parse(save.mock.calls[1][0]).currentTrack.id).toBe(9);
  });

  it("does not overwrite playback started before the initial disk read completes", async () => {
    const pending = deferred<string>();
    load.mockReturnValueOnce(pending.promise);
    const { store } = setup(true);
    act(() => {
      store.set(currentTrackAtom, snapshot(8).currentTrack);
    });
    await act(async () => {
      pending.resolve(JSON.stringify(snapshot(1)));
    });
    expect(store.get(currentTrackAtom)?.id).toBe(8);
    await vi.advanceTimersByTimeAsync(2200);
    expect(JSON.parse(save.mock.calls[0][0]).currentTrack.id).toBe(8);
  });

  it.each(["pagehide", "hidden", "unmount"])(
    "captures pending changes on %s without waiting for idle",
    async (event) => {
      const { store, unmount } = setup(true);
      await act(async () => {});
      act(() => {
        store.set(currentTrackAtom, snapshot(17).currentTrack);
      });
      expect(localStorage.getItem(PLAYBACK_STATE_KEY)).toBeNull();
      if (event === "pagehide") {
        window.dispatchEvent(new Event("pagehide"));
      } else if (event === "hidden") {
        vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
        document.dispatchEvent(new Event("visibilitychange"));
      } else {
        unmount();
      }
      expect(
        JSON.parse(localStorage.getItem(PLAYBACK_STATE_KEY)!).currentTrack.id,
      ).toBe(17);
      await act(async () => {});
      expect(JSON.parse(save.mock.calls[0][0]).currentTrack.id).toBe(17);
      expect(vi.getTimerCount()).toBe(0);
      if (event === "unmount") {
        act(() => {
          store.set(currentTrackAtom, snapshot(18).currentTrack);
        });
        window.dispatchEvent(new Event("pagehide"));
        await vi.advanceTimersByTimeAsync(3000);
        expect(save).toHaveBeenCalledTimes(1);
      }
    },
  );

  it("does not restore a pending backend response after unmount", async () => {
    const pending = deferred<string>();
    load.mockReturnValueOnce(pending.promise);
    const { store, unmount } = setup();
    unmount();
    await act(async () => {
      pending.resolve(JSON.stringify(snapshot(4)));
    });
    expect(store.get(currentTrackAtom)).toBeNull();
    expect(save).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("cleans up subscriptions and ignores the first restore under StrictMode", async () => {
    const pending = deferred<string>();
    load
      .mockReturnValueOnce(pending.promise)
      .mockResolvedValueOnce(JSON.stringify(snapshot(6)));
    const store = createStore();
    const wrapper = ({ children }: PropsWithChildren) => (
      <Provider store={store}>{children}</Provider>
    );
    renderHook(() => usePlaybackPersistence(), {
      wrapper,
      reactStrictMode: true,
    });
    await act(async () => {});
    await act(async () => {
      pending.resolve(JSON.stringify(snapshot(1)));
    });
    expect(store.get(currentTrackAtom)?.id).toBe(6);
    act(() => {
      store.set(currentTrackAtom, snapshot(8).currentTrack);
    });
    await vi.advanceTimersByTimeAsync(2200);
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("restores legacy local data, filters malformed tracks, caps history and strips runtime links", async () => {
    load.mockRejectedValueOnce(new Error("no backend"));
    localStorage.setItem(
      PLAYBACK_STATE_KEY,
      JSON.stringify({
        currentTrack: { id: 1, _playingFrom: {} },
        queue: [null, {}, { id: 2, _contextFrom: {} }],
        history: Array.from({ length: 510 }, (_, id) => ({ id })),
        playbackSource: { type: "playlist", id: "legacy", name: "Legacy" },
      }),
    );
    const { store } = setup();
    await act(async () => {});
    expect(store.get(currentTrackAtom)).not.toHaveProperty("_playingFrom");
    expect(store.get(queueAtom)).toHaveLength(1);
    expect(store.get(queueAtom)[0]).not.toHaveProperty("_contextFrom");
    expect(store.get(historyAtom)).toHaveLength(500);
    expect(store.get(historyAtom)[0].id).toBe(10);
    expect(store.get(playbackSourceAtom)?.tracks).toEqual([]);
    expect(save).not.toHaveBeenCalled();
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PlaybackSnapshot, Track } from "../types";
import {
  createPlaybackPersistence,
  MAX_HISTORY_TRACKS,
  PLAYBACK_STATE_KEY,
  readPendingPlaybackSnapshot,
} from "./playbackPersistence";

function snapshot(id = 1): PlaybackSnapshot {
  return {
    currentTrack: { id, title: `Track ${id}` } as Track,
    queue: [],
    history: [],
    manualQueue: [],
    originalQueue: null,
    playbackSource: null,
    contextSource: null,
  };
}

function deferred() {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<void>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

function setup() {
  let value = snapshot();
  const readSnapshot = vi.fn(() => value);
  const saveBackend = vi
    .fn<(json: string) => Promise<void>>()
    .mockResolvedValue(undefined);
  const onError = vi.fn();
  const persistence = createPlaybackPersistence({
    readSnapshot,
    saveBackend,
    onError,
    storage: localStorage,
  });
  return {
    persistence,
    readSnapshot,
    saveBackend,
    onError,
    update(next: PlaybackSnapshot) {
      value = next;
      persistence.markDirty();
    },
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  localStorage.clear();
  vi.stubGlobal("requestIdleCallback", undefined);
  vi.stubGlobal("cancelIdleCallback", undefined);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("playback snapshot persistence", () => {
  it("coalesces rapid edits without reading or serializing during input", async () => {
    const { persistence, update, readSnapshot, saveBackend } = setup();
    for (let id = 1; id <= 100; id++) update(snapshot(id));
    expect(readSnapshot).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(200);
    expect(readSnapshot).toHaveBeenCalledTimes(1);
    expect(
      JSON.parse(localStorage.getItem(PLAYBACK_STATE_KEY)!).currentTrack.id,
    ).toBe(100);
    expect(saveBackend).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(2000);
    expect(saveBackend).toHaveBeenCalledTimes(1);
    expect(readPendingPlaybackSnapshot(localStorage)).toBeNull();
    await persistence.dispose();
  });

  it("uses idle time with a deadline and captures pending work on disposal", async () => {
    const requestIdleCallback = vi.fn(() => 9);
    const cancelIdleCallback = vi.fn();
    vi.stubGlobal("requestIdleCallback", requestIdleCallback);
    vi.stubGlobal("cancelIdleCallback", cancelIdleCallback);
    const { persistence, update, readSnapshot, saveBackend } = setup();
    update(snapshot(17));
    await vi.advanceTimersByTimeAsync(200);
    expect(requestIdleCallback).toHaveBeenCalledWith(expect.any(Function), {
      timeout: 1000,
    });
    expect(readSnapshot).not.toHaveBeenCalled();
    await persistence.dispose();
    expect(cancelIdleCallback).toHaveBeenCalledWith(9);
    expect(JSON.parse(saveBackend.mock.calls[0][0]).currentTrack.id).toBe(17);
    expect(vi.getTimerCount()).toBe(0);
    update(snapshot(18));
    await vi.advanceTimersByTimeAsync(5000);
    expect(readSnapshot).toHaveBeenCalledTimes(1);
  });

  it("takes periodic recovery copies even while the queue keeps changing", async () => {
    const { persistence, update, readSnapshot } = setup();
    for (let id = 1; id <= 10; id++) {
      update(snapshot(id));
      await vi.advanceTimersByTimeAsync(100);
    }
    expect(readSnapshot).toHaveBeenCalledTimes(5);
    expect(
      JSON.parse(readPendingPlaybackSnapshot(localStorage)!.json).currentTrack
        .id,
    ).toBe(10);
    await persistence.dispose();
  });

  it("clones a repeated track once per capture and observes metadata mutations", async () => {
    const { persistence, update, saveBackend } = setup();
    let title = "Before";
    const readTitle = vi.fn(() => title);
    const track = {
      id: 1,
      get title() {
        return readTitle();
      },
      _playingFrom: { circular: null as unknown },
      _contextFrom: {},
    } as unknown as Track;
    const value = snapshot();
    value.currentTrack = track;
    value.queue = [track, track];
    value.history = Array.from(
      { length: MAX_HISTORY_TRACKS + 100 },
      () => track,
    );
    value.manualQueue = [track];
    value.originalQueue = [track];
    value.playbackSource = {
      type: "playlist",
      id: "one",
      name: "One",
      tracks: [track],
    };
    value.contextSource = value.playbackSource;
    update(value);
    await persistence.flush();
    expect(readTitle).toHaveBeenCalledTimes(1);
    const json = saveBackend.mock.calls[0][0];
    expect(json).not.toContain("_playingFrom");
    expect(json).not.toContain("_contextFrom");
    expect(JSON.parse(json).history).toHaveLength(MAX_HISTORY_TRACKS);
    expect(track).toHaveProperty("_playingFrom");
    title = "After";
    update(value);
    await persistence.flush();
    expect(readTitle).toHaveBeenCalledTimes(2);
    expect(JSON.parse(saveBackend.mock.calls[1][0]).currentTrack.title).toBe(
      "After",
    );
  });

  it("accepts legacy snapshots without optional queues", async () => {
    const { persistence, update, saveBackend } = setup();
    update({ currentTrack: null, queue: [], history: [] });
    await persistence.flush();
    expect(JSON.parse(saveBackend.mock.calls[0][0])).toMatchObject({
      manualQueue: [],
      originalQueue: null,
    });
  });

  it("serializes writes, drops superseded waiting snapshots and retains newer recovery state", async () => {
    const { persistence, update, saveBackend } = setup();
    const first = deferred();
    const last = deferred();
    saveBackend
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(last.promise);
    update(snapshot(1));
    const firstFlush = persistence.flush();
    update(snapshot(2));
    void persistence.flush();
    update(snapshot(3));
    const lastFlush = persistence.flush();
    expect(saveBackend).toHaveBeenCalledTimes(1);
    first.resolve();
    await Promise.resolve();
    expect(saveBackend).toHaveBeenCalledTimes(2);
    expect(
      saveBackend.mock.calls.map(([json]) => JSON.parse(json).currentTrack.id),
    ).toEqual([1, 3]);
    expect(
      JSON.parse(readPendingPlaybackSnapshot(localStorage)!.json).currentTrack
        .id,
    ).toBe(3);
    last.resolve();
    await Promise.all([firstFlush, lastFlush]);
    expect(readPendingPlaybackSnapshot(localStorage)).toBeNull();
  });

  it("keeps logout's tombstone until all older disk writes finish and the empty state saves", async () => {
    const { persistence, update, saveBackend } = setup();
    const old = deferred();
    const cleared = deferred();
    saveBackend
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(cleared.promise);
    update(snapshot(1));
    void persistence.flush();
    update(snapshot(2));
    const complete = persistence.clear();
    expect(localStorage.getItem(PLAYBACK_STATE_KEY)).toBeNull();
    expect(readPendingPlaybackSnapshot(localStorage)?.cleared).toBe(true);
    old.resolve();
    await Promise.resolve();
    expect(readPendingPlaybackSnapshot(localStorage)?.cleared).toBe(true);
    expect(JSON.parse(saveBackend.mock.calls[1][0]).currentTrack).toBeNull();
    cleared.resolve();
    await complete;
    expect(readPendingPlaybackSnapshot(localStorage)).toBeNull();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not let an older controller acknowledge a different local snapshot", async () => {
    const older = setup();
    const writing = deferred();
    older.saveBackend.mockReturnValueOnce(writing.promise);
    older.update(snapshot(1));
    const completion = older.persistence.flush();
    const newer = setup();
    newer.update(snapshot(2));
    await vi.advanceTimersByTimeAsync(200);
    writing.resolve();
    await completion;
    expect(
      JSON.parse(readPendingPlaybackSnapshot(localStorage)!.json).currentTrack
        .id,
    ).toBe(2);
    await newer.persistence.dispose();
  });

  it("retains recoverable state after failures and retries on final flush", async () => {
    const { persistence, update, saveBackend, onError } = setup();
    saveBackend.mockRejectedValueOnce(new Error("disk unavailable"));
    update(snapshot(6));
    await persistence.flush();
    expect(readPendingPlaybackSnapshot(localStorage)).not.toBeNull();
    expect(onError).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(10000);
    expect(saveBackend).toHaveBeenCalledTimes(1);
    await persistence.dispose();
    expect(saveBackend).toHaveBeenCalledTimes(2);
    expect(readPendingPlaybackSnapshot(localStorage)).toBeNull();
  });

  it("skips duplicate local and backend writes", async () => {
    const { persistence, update, saveBackend } = setup();
    update(snapshot(2));
    await persistence.flush();
    const writeLocal = vi.spyOn(localStorage, "setItem");
    update(snapshot(2));
    await persistence.flush();
    expect(saveBackend).toHaveBeenCalledTimes(1);
    expect(writeLocal).not.toHaveBeenCalled();
  });
});

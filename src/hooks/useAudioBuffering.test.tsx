import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useAudioBuffering } from "./useAudioBuffering";
import { usePlaybackActions } from "./usePlaybackActions";
import {
  acceptAudioOutputAtom,
  audioBufferingAtom,
  audioOutputStateAtom,
  configuredAudioOutputAtom,
} from "../atoms/audioOutput";
import {
  currentTrackAtom,
  isPlayingAtom,
  userPausedAtom,
} from "../atoms/playback";
import type { Track } from "../types";
import { markPlaybackLoading } from "../lib/playbackPosition";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => ({ showToast: vi.fn() }),
}));
vi.mock("../lib/playbackPosition", () => ({
  markPlaybackLoading: vi.fn(),
  getInterpolatedPosition: vi.fn(),
  notifySeek: vi.fn(),
}));
function setup() {
  const store = createStore();
  const config = {
    ...store.get(configuredAudioOutputAtom),
    route: "hqplayer" as const,
  };
  store.set(acceptAudioOutputAtom, {
    configured: config,
    active: config,
    playbackGeneration: 1,
    revision: 0,
    pending: false,
  });
  store.set(currentTrackAtom, { id: 12, title: "Current" } as Track);
  store.set(isPlayingAtom, true);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  const hook = renderHook(
    () => {
      useAudioBuffering();
      return usePlaybackActions();
    },
    { wrapper },
  );
  const receive = vi.mocked(listen).mock.calls[
    vi.mocked(listen).mock.calls.length - 1
  ][1] as (event: {
    payload: {
      buffering: boolean;
      trackId: number;
      playbackGeneration: number;
    };
  }) => void;
  const emit = (
    buffering: boolean,
    trackId = 12,
    playbackGeneration = store.get(audioOutputStateAtom)?.playbackGeneration ??
      1,
  ) =>
    act(() => receive({ payload: { buffering, trackId, playbackGeneration } }));
  return { ...hook, store, emit };
}
beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  vi.mocked(listen).mockResolvedValue(vi.fn());
  vi.mocked(invoke).mockResolvedValue(undefined);
});
afterEach(cleanup);

describe("HQPlayer reconnect bookkeeping", () => {
  it("freezes the clock and scrobbling once per disconnect and resumes on confirmed playback", () => {
    const { store, emit } = setup();
    emit(true);
    emit(true);
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(markPlaybackLoading).toHaveBeenCalledWith(true);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("notify_track_paused");
    emit(false);
    emit(false);
    expect(store.get(isPlayingAtom)).toBe(true);
    expect(store.get(audioBufferingAtom)).toBeNull();
    expect(markPlaybackLoading).toHaveBeenLastCalledWith(false);
    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke).toHaveBeenLastCalledWith("notify_track_resumed");
  });
  it("honors a user's pause during reconnection", async () => {
    const { store, result, emit } = setup();
    emit(true);
    await act(async () => result.current.togglePlayPause());
    expect(invoke).toHaveBeenCalledWith("pause_track");
    expect(store.get(userPausedAtom)).toBe(true);
    emit(false);
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(invoke).not.toHaveBeenCalledWith("notify_track_resumed");
  });
  it("does not resume a track that was paused before the disconnect", () => {
    const { store, emit } = setup();
    store.set(isPlayingAtom, false);
    emit(true);
    emit(false);
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  });
  it("ignores old track events and a reconnect after replaying the same track", () => {
    const { store, emit } = setup();
    emit(true, 99);
    expect(store.get(isPlayingAtom)).toBe(true);
    emit(true);
    act(() =>
      store.set(currentTrackAtom, { id: 12, title: "Replay" } as Track),
    );
    expect(store.get(audioBufferingAtom)).toBeNull();
    emit(false);
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(invoke).not.toHaveBeenCalledWith("notify_track_resumed");
  });
  it("ignores a disconnect after native output takes over", () => {
    const { store, emit } = setup();
    const config = {
      ...store.get(configuredAudioOutputAtom),
      route: "native" as const,
    };
    act(() =>
      store.set(acceptAudioOutputAtom, {
        configured: config,
        active: config,
        playbackGeneration: 1,
        revision: 0,
        pending: false,
      }),
    );
    emit(true);
    expect(store.get(isPlayingAtom)).toBe(true);
    expect(store.get(audioBufferingAtom)).toBeNull();
  });
  it("waits for confirmed playing after manually resuming during a disconnect", async () => {
    const { store, result, emit } = setup();
    emit(true);
    await act(async () => result.current.pauseTrack());
    await act(async () => result.current.resumeTrack());
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(store.get(userPausedAtom)).toBe(false);
    emit(false);
    expect(store.get(isPlayingAtom)).toBe(true);
  });
});

it("does not let a late resume confirmation undo a newer pause during buffering", async () => {
  let finish!: () => void;
  vi.mocked(invoke).mockImplementation(async (command) =>
    command === "resume_track"
      ? new Promise<void>((resolve) => {
          finish = resolve;
        })
      : false,
  );
  const { store, result, emit } = setup();
  emit(true);
  await act(async () => result.current.pauseTrack());
  let resuming!: Promise<void>;
  act(() => {
    resuming = result.current.resumeTrack();
  });
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("resume_track"));
  await act(async () => result.current.pauseTrack());
  await act(async () => {
    finish();
    await resuming;
  });
  emit(false);
  expect(store.get(userPausedAtom)).toBe(true);
  expect(store.get(isPlayingAtom)).toBe(false);
});

it("does not let a late pause confirmation undo a newer resume", async () => {
  let finish!: () => void;
  vi.mocked(invoke).mockImplementation(async (command) =>
    command === "pause_track"
      ? new Promise<void>((resolve) => {
          finish = resolve;
        })
      : false,
  );
  const { store, result } = setup();
  let pausing!: Promise<void>;
  act(() => {
    pausing = result.current.pauseTrack();
  });
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("pause_track"));
  await act(async () => result.current.resumeTrack());
  await act(async () => {
    finish();
    await pausing;
  });
  expect(store.get(userPausedAtom)).toBe(false);
  expect(store.get(isPlayingAtom)).toBe(true);
});

it("rejects delayed buffering events from an earlier playback of the same track ID", () => {
  const { store, emit } = setup();
  const output = store.get(audioOutputStateAtom)!;
  act(() =>
    store.set(acceptAudioOutputAtom, {
      ...output,
      revision: 1,
      playbackGeneration: 2,
    }),
  );
  emit(true, 12, 1);
  expect(store.get(isPlayingAtom)).toBe(true);
  expect(store.get(audioBufferingAtom)).toBeNull();
  emit(true, 12, 2);
  expect(store.get(isPlayingAtom)).toBe(false);
  emit(false, 12, 1);
  expect(store.get(isPlayingAtom)).toBe(false);
  emit(false, 12, 2);
  expect(store.get(isPlayingAtom)).toBe(true);
});

it("keeps reconnect bookkeeping valid across pending output configuration changes", () => {
  const { store, emit } = setup();
  const output = store.get(audioOutputStateAtom)!;
  // A configured-only event can arrive ahead of a buffering event from the
  // same active playback, so its revision must not invalidate that event.
  act(() =>
    store.set(acceptAudioOutputAtom, {
      ...output,
      configured: { ...output.configured, route: "native" },
      pending: true,
      revision: 1,
    }),
  );
  emit(true, 12, 1);
  expect(store.get(isPlayingAtom)).toBe(false);
  expect(invoke).toHaveBeenCalledWith("notify_track_paused");
  act(() => store.set(acceptAudioOutputAtom, { ...output, revision: 2 }));
  expect(store.get(audioBufferingAtom)).not.toBeNull();
  emit(false, 12, 1);
  expect(store.get(isPlayingAtom)).toBe(true);
  expect(invoke).toHaveBeenCalledWith("notify_track_resumed");
});

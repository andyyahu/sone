import { act, cleanup, renderHook } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bitPerfectAtom,
  currentTrackAtom,
  historyAtom,
  isPlayingAtom,
  manualQueueAtom,
  queueAtom,
  streamInfoAtom,
} from "../atoms/playback";
import type { Track, StreamInfo } from "../types";
import { useAudioErrors } from "./useAudioErrors";

const { listen, invoke, toast, unlisten } = vi.hoisted(() => ({
  listen: vi.fn(),
  invoke: vi.fn(),
  toast: vi.fn(),
  unlisten: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("../contexts/ToastContext", () => ({
  useToast: () => ({ showToast: toast }),
}));

function setup() {
  const store = createStore();
  const current = { id: 1, title: "Current" } as Track;
  const queued = [{ id: 2, title: "Next" }] as Track[];
  store.set(currentTrackAtom, current);
  store.set(queueAtom, queued);
  store.set(manualQueueAtom, queued);
  store.set(historyAtom, queued);
  store.set(isPlayingAtom, true);
  store.set(bitPerfectAtom, true);
  store.set(streamInfoAtom, { sampleRate: 96000 } as StreamInfo);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  const hook = renderHook(() => useAudioErrors(), { wrapper });
  const emit = (kind: string, message?: string) =>
    act(() => {
      listen.mock.calls[listen.mock.calls.length - 1][1]({
        payload: { kind, message },
      });
    });
  return { store, current, queued, emit, ...hook };
}

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  listen.mockResolvedValue(unlisten);
  invoke.mockResolvedValue(undefined);
});
afterEach(cleanup);

describe("asynchronous audio failures", () => {
  it("stops unsupported bit-perfect playback and preserves the current track and queues", () => {
    const { store, current, queued, emit } = setup();
    emit("bit_perfect_unsupported");
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(store.get(streamInfoAtom)).toBeNull();
    expect(store.get(currentTrackAtom)).toBe(current);
    for (const queue of [queueAtom, manualQueueAtom, historyAtom])
      expect(store.get(queue)).toBe(queued);
    expect(store.get(bitPerfectAtom)).toBe(true);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("stop_track");
    expect(toast).toHaveBeenCalledWith(
      expect.stringContaining("Turn off bit-perfect manually"),
      "error",
    );
  });

  it("reports disconnection without advancing to another track", () => {
    const { store, current, emit } = setup();
    emit("device_disconnected", "DAC disconnected");
    expect(store.get(isPlayingAtom)).toBe(false);
    expect(store.get(streamInfoAtom)).toBeNull();
    expect(store.get(currentTrackAtom)).toBe(current);
    expect(toast).toHaveBeenCalledWith("DAC disconnected", "error");
    expect(invoke).not.toHaveBeenCalled();
  });

  it("ignores queued callbacks after unmount and releases a delayed listener", async () => {
    let finishListen!: (fn: () => void) => void;
    listen.mockReturnValueOnce(
      new Promise((resolve) => {
        finishListen = resolve;
      }),
    );
    const { emit, unmount } = setup();
    unmount();
    emit("bit_perfect_unsupported");
    expect(invoke).not.toHaveBeenCalled();
    expect(toast).not.toHaveBeenCalled();
    await act(async () => {
      finishListen(unlisten);
    });
    expect(unlisten).toHaveBeenCalledOnce();
  });
});

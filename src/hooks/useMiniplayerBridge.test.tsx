import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MiniplayerState } from "./useMiniplayerEmitter";
import { useMiniplayerBridge } from "./useMiniplayerBridge";

const mocks = vi.hoisted(() => ({
  listen: vi.fn(),
  unlisten: vi.fn(),
  emitTo: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({ emitTo: mocks.emitTo }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ listen: mocks.listen }),
}));

let receive: (event: { payload: MiniplayerState }) => void;

function payload(overrides: Partial<MiniplayerState> = {}): MiniplayerState {
  return {
    track: {
      id: 1,
      title: "Song",
      artist: { id: 2, name: "Artist" },
      artists: [{ id: 2, name: "Artist" }],
      album: { id: 3, cover: "cover" },
    },
    isPlaying: true,
    position: 10,
    duration: 180,
    isFavorite: false,
    shuffle: false,
    repeat: 0,
    volume: 1,
    playbackSourceLabel: { type: "album", id: 3, name: "Album" },
    bitPerfect: false,
    accentColor: "#A855F7",
    ...overrides,
  };
}

function update(overrides: Partial<MiniplayerState> = {}) {
  act(() => receive({ payload: payload(overrides) }));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  mocks.listen.mockImplementation((_event, callback) => {
    receive = callback;
    return Promise.resolve(mocks.unlisten);
  });
  mocks.emitTo.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("useMiniplayerBridge", () => {
  it("reanchors heartbeat positions without rendering the player", () => {
    const rendered = vi.fn();
    const { result } = renderHook(() => {
      rendered();
      return useMiniplayerBridge();
    });
    update();
    const display = result.current.state;
    const renderCount = rendered.mock.calls.length;

    for (let second = 1; second <= 10; second++) {
      act(() => vi.advanceTimersByTime(1000));
      // Each IPC heartbeat contains fresh, structurally equal nested objects.
      update({ position: 10 + second });
    }

    expect(rendered).toHaveBeenCalledTimes(renderCount);
    expect(result.current.state).toBe(display);
    expect(result.current.positionClock.getPosition()).toBe(20);
    expect(vi.getTimerCount()).toBe(0);

    update({ isFavorite: true });
    expect(rendered).toHaveBeenCalledTimes(renderCount + 1);
    expect(result.current.state.isFavorite).toBe(true);
  });

  it("updates visible metadata even when the track ID stays the same", () => {
    const { result } = renderHook(useMiniplayerBridge);
    update();
    const track = payload().track!;
    update({
      track: {
        ...track,
        title: "Corrected title",
        version: "Live",
        artists: [{ id: 2, name: "Corrected artist" }],
        album: { ...track.album, vibrantColor: "#123456" },
      },
      playbackSourceLabel: { type: "album", id: 3, name: "Renamed album" },
    });
    expect(result.current.state.track?.title).toBe("Corrected title");
    expect(result.current.state.track?.version).toBe("Live");
    expect(result.current.state.track?.artists?.[0].name).toBe(
      "Corrected artist",
    );
    expect(result.current.state.playbackSourceLabel?.name).toBe(
      "Renamed album",
    );
  });

  it("freezes optimistic pause immediately and reconciles rejected toggles on heartbeat", () => {
    const { result } = renderHook(useMiniplayerBridge);
    update();
    act(() => vi.advanceTimersByTime(500));
    act(() => result.current.sendCommand("toggle-play"));
    expect(result.current.isPlaying).toBe(false);
    expect(result.current.positionClock.getPosition()).toBe(10.5);
    act(() => vi.advanceTimersByTime(500));
    expect(result.current.positionClock.getPosition()).toBe(10.5);

    update({ position: 11 });
    expect(result.current.isPlaying).toBe(true);
    expect(result.current.positionClock.getPosition()).toBe(11);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("rolls back both the icon and clock if optimistic resume receives no response", () => {
    const { result } = renderHook(useMiniplayerBridge);
    update({ isPlaying: false, position: 8 });
    act(() => result.current.sendCommand("toggle-play"));
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.isPlaying).toBe(true);
    expect(result.current.positionClock.getPosition()).toBe(9);

    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.isPlaying).toBe(false);
    expect(result.current.positionClock.getPosition()).toBe(8);
  });

  it("handles rapid toggles without waiting for a React render", () => {
    const { result } = renderHook(useMiniplayerBridge);
    update();
    act(() => {
      result.current.sendCommand("toggle-play");
      result.current.sendCommand("toggle-play");
    });
    expect(result.current.isPlaying).toBe(true);
    expect(result.current.positionClock.getSnapshot().playing).toBe(true);
    expect(vi.getTimerCount()).toBe(1);
  });

  it("suppresses stale seek echoes for 500ms but immediately accepts a new track", () => {
    const { result } = renderHook(useMiniplayerBridge);
    update();
    act(() => result.current.sendCommand("seek", 70));
    act(() => vi.advanceTimersByTime(100));
    update({ position: 10.1 });
    expect(result.current.positionClock.getPosition()).toBeCloseTo(70.1);

    act(() => vi.advanceTimersByTime(500));
    update({ position: 70.6 });
    expect(result.current.positionClock.getPosition()).toBeCloseTo(70.6);

    act(() => result.current.sendCommand("seek", 90));
    update({ track: { ...payload().track!, id: 4 }, position: 2 });
    expect(result.current.positionClock.getPosition()).toBe(2);
    expect(mocks.emitTo).toHaveBeenCalledWith("main", "miniplayer-command", {
      action: "seek",
      value: 90,
    });
  });

  it("debounces volume and cancels pending work on unmount", async () => {
    const { result, unmount } = renderHook(useMiniplayerBridge);
    act(() => {
      result.current.sendVolume(0.2);
      result.current.sendVolume(0.4);
      vi.advanceTimersByTime(50);
    });
    expect(mocks.emitTo).toHaveBeenLastCalledWith(
      "main",
      "miniplayer-command",
      {
        action: "set-volume",
        value: 0.4,
      },
    );
    act(() => {
      result.current.sendVolume(0.6);
      result.current.sendCommand("toggle-play");
    });
    const sent = mocks.emitTo.mock.calls.length;
    unmount();
    await act(async () => {});
    expect(mocks.unlisten).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
    act(() => vi.advanceTimersByTime(3000));
    expect(mocks.emitTo).toHaveBeenCalledTimes(sent);
  });
});

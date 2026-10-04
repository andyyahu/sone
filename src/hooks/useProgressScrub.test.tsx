import {
  act,
  cleanup,
  fireEvent,
  render,
  renderHook,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { currentTrackAtom, isPlayingAtom } from "../atoms/playback";
import type { Track } from "../types";
import { useProgressScrub } from "./useProgressScrub";

const mocks = vi.hoisted(() => ({
  position: vi.fn(() => 10),
  seekTo: vi.fn(),
  notifySeek: vi.fn(),
}));
vi.mock("./usePlaybackActions", () => ({
  usePlaybackActions: () => ({ seekTo: mocks.seekTo }),
}));
vi.mock("../lib/playbackPosition", () => ({
  getInterpolatedPosition: mocks.position,
  notifySeek: mocks.notifySeek,
}));

const track = { id: 1, title: "Song", duration: 180 } as Track;

function setup(playing = true) {
  const store = createStore();
  store.set(currentTrackAtom, track);
  store.set(isPlayingAtom, playing);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  return { store, wrapper };
}

function visibility(state: DocumentVisibilityState) {
  vi.spyOn(document, "visibilityState", "get").mockReturnValue(state);
  fireEvent(document, new Event("visibilitychange"));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  mocks.position.mockReturnValue(10);
  mocks.seekTo.mockResolvedValue(undefined);
  mocks.notifySeek.mockImplementation((position: number) => {
    mocks.position.mockReturnValue(position);
    window.dispatchEvent(
      new CustomEvent("playback-seeked", { detail: position }),
    );
  });
  visibility("visible");
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("useProgressScrub activity", () => {
  it("stops polling while covered or the document is hidden and catches up on reveal", () => {
    const { wrapper } = setup();
    const { result, rerender, unmount } = renderHook(
      ({ active }) => useProgressScrub({ active }),
      { wrapper, initialProps: { active: true } },
    );
    expect(result.current.displayTime).toBe(10);
    expect(vi.getTimerCount()).toBe(1);
    rerender({ active: false });
    expect(vi.getTimerCount()).toBe(0);
    mocks.position.mockClear().mockReturnValue(20);
    act(() => vi.advanceTimersByTime(5000));
    fireEvent(window, new Event("playback-seeked"));
    expect(mocks.position).not.toHaveBeenCalled();
    rerender({ active: true });
    expect(result.current.displayTime).toBe(20);

    visibility("hidden");
    expect(vi.getTimerCount()).toBe(0);
    mocks.position.mockReturnValue(30);
    visibility("visible");
    expect(result.current.displayTime).toBe(30);
    expect(vi.getTimerCount()).toBe(1);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("shows paused positions and external seeks immediately without a timer", () => {
    const { store, wrapper } = setup(false);
    const { result } = renderHook(() => useProgressScrub(), { wrapper });
    expect(result.current.displayTime).toBe(10);
    expect(vi.getTimerCount()).toBe(0);
    mocks.position.mockReturnValue(45);
    fireEvent(window, new Event("playback-seeked"));
    expect(result.current.displayTime).toBe(45);
    act(() => store.set(isPlayingAtom, true));
    expect(vi.getTimerCount()).toBe(1);
    mocks.position.mockReturnValue(45.4);
    act(() => store.set(isPlayingAtom, false));
    expect(result.current.displayTime).toBe(45.4);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("resets for a new or empty track without keeping stale progress", () => {
    const { store, wrapper } = setup();
    const { result } = renderHook(() => useProgressScrub(), { wrapper });
    mocks.position.mockReturnValue(0);
    act(() => store.set(currentTrackAtom, { ...track, id: 2 }));
    expect(result.current.displayTime).toBe(0);
    act(() => store.set(currentTrackAtom, null));
    expect(result.current.displayTime).toBe(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps drag feedback immediate and seeks to the released position while paused", async () => {
    const { wrapper } = setup(false);
    const onDraggingChange = vi.fn();
    const onDragEnd = vi.fn();
    function Scrubber() {
      const { progressRef, handleProgressMouseDown, displayTime } =
        useProgressScrub({ onDraggingChange, onDragEnd });
      return (
        <div>
          <div
            data-testid="scrubber"
            ref={progressRef}
            onMouseDown={handleProgressMouseDown}
          />
          <span>{displayTime}</span>
        </div>
      );
    }
    const { getByTestId, getByText } = render(<Scrubber />, { wrapper });
    vi.spyOn(getByTestId("scrubber"), "getBoundingClientRect").mockReturnValue({
      left: 0,
      width: 180,
    } as DOMRect);
    fireEvent.mouseDown(getByTestId("scrubber"), { clientX: 30 });
    expect(onDraggingChange).toHaveBeenLastCalledWith(true);
    expect(getByText("30")).toBeTruthy();
    fireEvent.mouseMove(document, { clientX: 60 });
    expect(getByText("60")).toBeTruthy();
    await act(async () => {
      fireEvent.mouseUp(document, { clientX: 90 });
    });
    expect(getByText("90")).toBeTruthy();
    expect(onDraggingChange).toHaveBeenLastCalledWith(false);
    expect(onDragEnd).toHaveBeenCalledOnce();
    expect(mocks.seekTo).toHaveBeenCalledWith(90);
    expect(vi.getTimerCount()).toBe(0);
  });
});

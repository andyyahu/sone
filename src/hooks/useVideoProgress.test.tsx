import { act, cleanup, fireEvent, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  notifyVideoProgressSourceChanged,
  useVideoProgress,
} from "./useVideoProgress";

function media(paused = false) {
  const element = document.createElement("video");
  const state = { position: 10, duration: 180, paused, ended: false };
  const readPosition = vi.fn(() => state.position);
  Object.defineProperties(element, {
    currentTime: { configurable: true, get: readPosition },
    duration: { configurable: true, get: () => state.duration },
    paused: { configurable: true, get: () => state.paused },
    ended: { configurable: true, get: () => state.ended },
  });
  return { element, state, readPosition };
}

function visibility(state: DocumentVisibilityState) {
  vi.spyOn(document, "visibilityState", "get").mockReturnValue(state);
  fireEvent(document, new Event("visibilitychange"));
}

beforeEach(() => {
  vi.useFakeTimers();
  visibility("visible");
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("useVideoProgress", () => {
  it("reads the visible playing element every 150ms without animation frames", () => {
    const { element, state, readPosition } = media();
    const raf = vi.spyOn(window, "requestAnimationFrame");
    const ref = { current: element };
    const dragging = { current: false };
    const { result, unmount } = renderHook(() =>
      useVideoProgress(ref, true, dragging),
    );
    expect(result.current.position).toBe(10);
    expect(result.current.duration).toBe(180);
    expect(vi.getTimerCount()).toBe(1);
    readPosition.mockClear();
    act(() => vi.advanceTimersByTime(149));
    expect(readPosition).not.toHaveBeenCalled();
    state.position = 10.15;
    act(() => vi.advanceTimersByTime(1));
    expect(result.current.position).toBe(10.15);
    expect(readPosition).toHaveBeenCalledTimes(1);
    expect(raf).not.toHaveBeenCalled();
    unmount();
    expect(vi.getTimerCount()).toBe(0);
    fireEvent(element, new Event("playing"));
    expect(vi.getTimerCount()).toBe(0);
  });

  it("stops hidden or inactive consumers and catches up immediately on reveal", () => {
    const { element, state, readPosition } = media();
    const ref = { current: element };
    const dragging = { current: false };
    const { result, rerender } = renderHook(
      ({ active }) => useVideoProgress(ref, active, dragging),
      { initialProps: { active: true } },
    );
    rerender({ active: false });
    expect(vi.getTimerCount()).toBe(0);
    readPosition.mockClear();
    state.position = 20;
    fireEvent(element, new Event("seeked"));
    act(() => vi.advanceTimersByTime(10000));
    expect(readPosition).not.toHaveBeenCalled();
    rerender({ active: true });
    expect(result.current.position).toBe(20);
    expect(vi.getTimerCount()).toBe(1);

    visibility("hidden");
    expect(vi.getTimerCount()).toBe(0);
    state.position = 30;
    visibility("visible");
    expect(result.current.position).toBe(30);
    expect(vi.getTimerCount()).toBe(1);
  });

  it("handles paused external seeks and metadata updates without polling", () => {
    const { element, state, readPosition } = media(true);
    state.duration = Number.NaN;
    const ref = { current: element };
    const dragging = { current: false };
    const { result } = renderHook(() => useVideoProgress(ref, true, dragging));
    expect(result.current.duration).toBe(0);
    state.duration = 240;
    fireEvent(element, new Event("loadedmetadata"));
    expect(result.current.duration).toBe(240);
    state.position = 42;
    fireEvent(element, new Event("seeked"));
    expect(result.current.position).toBe(42);
    expect(vi.getTimerCount()).toBe(0);
    readPosition.mockClear();
    act(() => vi.advanceTimersByTime(10000));
    expect(readPosition).not.toHaveBeenCalled();
  });

  it("responds immediately to play, pause, buffering, and end events", () => {
    const { element, state } = media(true);
    const ref = { current: element };
    const dragging = { current: false };
    const { result } = renderHook(() => useVideoProgress(ref, true, dragging));
    state.paused = false;
    fireEvent(element, new Event("play"));
    expect(vi.getTimerCount()).toBe(1);
    fireEvent(element, new Event("waiting"));
    expect(vi.getTimerCount()).toBe(0);
    fireEvent(element, new Event("playing"));
    expect(vi.getTimerCount()).toBe(1);
    state.paused = true;
    state.position = 11.2;
    fireEvent(element, new Event("pause"));
    expect(result.current.position).toBe(11.2);
    expect(vi.getTimerCount()).toBe(0);
    state.paused = false;
    fireEvent(element, new Event("play"));
    state.ended = true;
    state.position = 180;
    fireEvent(element, new Event("ended"));
    expect(result.current.position).toBe(180);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("attaches when the sibling publishes its element and detaches old sources", () => {
    const first = media();
    const second = media(true);
    second.state.position = 50;
    const ref = { current: null as HTMLVideoElement | null };
    const dragging = { current: false };
    const { result } = renderHook(() => useVideoProgress(ref, true, dragging));
    expect(vi.getTimerCount()).toBe(0);
    act(() => {
      ref.current = first.element;
      notifyVideoProgressSourceChanged();
    });
    expect(result.current.position).toBe(10);
    expect(vi.getTimerCount()).toBe(1);
    act(() => {
      ref.current = second.element;
      notifyVideoProgressSourceChanged();
    });
    expect(result.current.position).toBe(50);
    expect(vi.getTimerCount()).toBe(0);
    fireEvent(first.element, new Event("playing"));
    expect(vi.getTimerCount()).toBe(0);
    act(() => {
      ref.current = null;
      notifyVideoProgressSourceChanged();
    });
    expect(result.current.position).toBe(0);
    expect(result.current.duration).toBe(0);
  });

  it("does not overwrite optimistic drag feedback with stale media positions", () => {
    const { element, state } = media();
    const ref = { current: element };
    const dragging = { current: false };
    const { result } = renderHook(() => useVideoProgress(ref, true, dragging));
    act(() => {
      dragging.current = true;
      result.current.setPosition(60);
    });
    act(() => vi.advanceTimersByTime(500));
    fireEvent(element, new Event("seeked"));
    expect(result.current.position).toBe(60);
    dragging.current = false;
    state.position = 60.1;
    fireEvent(element, new Event("seeked"));
    expect(result.current.position).toBe(60.1);
  });

  it("clears old metadata when the same element switches video sources", () => {
    const { element, state } = media();
    const ref = { current: element };
    const dragging = { current: false };
    const { result } = renderHook(() => useVideoProgress(ref, true, dragging));
    state.position = 0;
    state.duration = Number.NaN;
    state.paused = true;
    fireEvent(element, new Event("emptied"));
    expect(result.current.position).toBe(0);
    expect(result.current.duration).toBe(0);
    expect(vi.getTimerCount()).toBe(0);

    state.duration = 60;
    fireEvent(element, new Event("loadedmetadata"));
    expect(result.current.duration).toBe(60);
    expect(vi.getTimerCount()).toBe(0);
    state.paused = false;
    fireEvent(element, new Event("playing"));
    expect(vi.getTimerCount()).toBe(1);
  });

  it("catches a paused seek that occurs while its controls are hidden", () => {
    const { element, state, readPosition } = media(true);
    const ref = { current: element };
    const dragging = { current: false };
    const { result, rerender } = renderHook(
      ({ active }) => useVideoProgress(ref, active, dragging),
      { initialProps: { active: false } },
    );
    state.position = 90;
    fireEvent(element, new Event("seeked"));
    expect(readPosition).not.toHaveBeenCalled();
    rerender({ active: true });
    expect(result.current.position).toBe(90);
    expect(result.current.duration).toBe(180);
    expect(vi.getTimerCount()).toBe(0);
  });
});

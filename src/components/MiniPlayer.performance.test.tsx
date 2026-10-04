import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MiniplayerState } from "../hooks/useMiniplayerEmitter";
import MiniPlayer from "./MiniPlayer";

const mocks = vi.hoisted(() => ({
  listen: vi.fn(),
  imageRender: vi.fn(),
  emitTo: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({ emitTo: mocks.emitTo }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ listen: mocks.listen }),
}));
vi.mock("./TidalImage", () => ({
  default: (props: { alt: string }) => {
    mocks.imageRender();
    return <img alt={props.alt} />;
  },
}));
vi.mock("./ResizeEdges", () => ({ default: () => null }));

let receive: (event: { payload: MiniplayerState }) => void;
let resize: ResizeObserverCallback;

function update(overrides: Partial<MiniplayerState> = {}) {
  act(() =>
    receive({
      payload: {
        track: {
          id: 1,
          title: "Song",
          artist: { id: 2, name: "Artist" },
          album: { id: 3, cover: "cover" },
        },
        isPlaying: true,
        position: 10,
        duration: 180,
        isFavorite: false,
        shuffle: false,
        repeat: 0,
        volume: 1,
        playbackSourceLabel: null,
        bitPerfect: false,
        accentColor: "#A855F7",
        ...overrides,
      },
    }),
  );
}

function setHeight(height: number) {
  act(() => {
    resize(
      [{ contentRect: { width: 300, height } }] as ResizeObserverEntry[],
      {} as ResizeObserver,
    );
  });
  act(() => vi.advanceTimersByTime(30));
}

function setVisibility(visibility: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: visibility,
  });
  fireEvent(document, new Event("visibilitychange"));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  mocks.listen.mockImplementation((_event, callback) => {
    receive = callback;
    return Promise.resolve(vi.fn());
  });
  mocks.emitTo.mockResolvedValue(undefined);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(callback: ResizeObserverCallback) {
        resize = callback;
      }
      observe() {}
      disconnect() {}
    },
  );
  setVisibility("visible");
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  setVisibility("visible");
});

describe("MiniPlayer progress work", () => {
  it("updates only visible progress at 4Hz while artwork and controls stay idle", () => {
    const { container, getByText, unmount } = render(<MiniPlayer />);
    update();
    const imageRenders = mocks.imageRender.mock.calls.length;
    expect(vi.getTimerCount()).toBe(0);

    fireEvent.mouseEnter(container.querySelector(".group")!);
    expect(vi.getTimerCount()).toBe(1);
    const progressFill = getByText("0:10").nextElementSibling!
      .firstElementChild!.firstElementChild as HTMLElement;
    act(() => vi.advanceTimersByTime(249));
    expect(progressFill.style.width).toBe(`${(10 / 180) * 100}%`);
    act(() => vi.advanceTimersByTime(1));
    expect(progressFill.style.width).toBe(`${(10.25 / 180) * 100}%`);
    act(() => vi.advanceTimersByTime(750));
    expect(getByText("0:11")).toBeTruthy();
    update({ position: 11 });
    expect(mocks.imageRender).toHaveBeenCalledTimes(imageRenders);

    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("stops while hidden or unhovered and catches up immediately on return", () => {
    const { container, getByText } = render(<MiniPlayer />);
    update();
    const window = container.querySelector(".group")!;
    fireEvent.mouseEnter(window);
    setVisibility("hidden");
    expect(vi.getTimerCount()).toBe(0);
    act(() => vi.advanceTimersByTime(5000));
    update({ position: 15 });
    // Hidden progress is also unsubscribed from the 1Hz IPC heartbeat.
    expect(getByText("0:10")).toBeTruthy();
    setVisibility("visible");
    expect(getByText("0:15")).toBeTruthy();
    expect(vi.getTimerCount()).toBe(1);

    fireEvent.mouseLeave(window);
    expect(vi.getTimerCount()).toBe(0);
    act(() => vi.advanceTimersByTime(3000));
    fireEvent.mouseEnter(window);
    expect(getByText("0:18")).toBeTruthy();
  });

  it("runs no position timer for compact/narrow layouts or paused playback", () => {
    const { container } = render(<MiniPlayer />);
    update();
    fireEvent.mouseEnter(container.querySelector(".group")!);
    expect(vi.getTimerCount()).toBe(1);
    update({ isPlaying: false });
    expect(vi.getTimerCount()).toBe(0);
    update();
    expect(vi.getTimerCount()).toBe(1);
    update({ duration: 0 });
    expect(vi.getTimerCount()).toBe(0);
    update();
    expect(vi.getTimerCount()).toBe(1);

    setHeight(150);
    expect(vi.getTimerCount()).toBe(0);
    const imageRenders = mocks.imageRender.mock.calls.length;
    act(() => vi.advanceTimersByTime(3000));
    update({ position: 13 });
    expect(mocks.imageRender).toHaveBeenCalledTimes(imageRenders);
    setHeight(80);
    expect(vi.getTimerCount()).toBe(0);
  });
});

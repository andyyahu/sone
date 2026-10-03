import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { Provider, createStore } from "jotai";
import { currentTrackAtom, isPlayingAtom } from "../atoms/playback";
import {
  drawerOpenAtom,
  drawerTabAtom,
  maximizedPlayerAtom,
} from "../atoms/ui";
import { currentVideoAtom, videoExpandedAtom } from "../atoms/video";
import { ToastProvider } from "../contexts/ToastContext";
import type { Track, TidalVideo } from "../types";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => Promise.resolve(undefined)),
}));

const { position, getLyrics } = vi.hoisted(() => ({
  position: vi.fn(() => 1),
  getLyrics: vi.fn(async () => ({
    lyrics: "First line\nSecond line",
    subtitles: "[00:00.00]First line\n[00:10.00]Second line",
  })),
}));

vi.mock("../lib/playbackPosition", () => ({
  getInterpolatedPosition: position,
}));
vi.mock("../api/tidal", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api/tidal")>()),
  getTrackLyrics: getLyrics,
}));

import NowPlayingDrawer from "./NowPlayingDrawer";

const track = {
  id: 1,
  title: "Song",
  duration: 100,
  artist: { id: 2, name: "Artist" },
  album: { id: 3, title: "Album", cover: "cover", videoCover: "11-22" },
} as Track;

const pendingFrames = new Map<number, FrameRequestCallback>();
let frameId = 0;

function tick() {
  act(() => {
    const frames = [...pendingFrames.values()];
    pendingFrames.clear();
    frames.forEach((callback) => callback(0));
  });
}

async function renderDrawer() {
  const store = createStore();
  store.set(currentTrackAtom, track);
  store.set(isPlayingAtom, true);
  store.set(drawerTabAtom, "lyrics");
  store.set(drawerOpenAtom, true);
  const result = render(
    <Provider store={store}>
      <ToastProvider>
        <NowPlayingDrawer />
      </ToastProvider>
    </Provider>,
  );
  await screen.findByText("First line");
  return { ...result, store };
}

describe("hidden drawer visual work", () => {
  beforeEach(() => {
    pendingFrames.clear();
    position.mockReturnValue(1);
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
    vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
    vi.stubGlobal("IntersectionObserver", undefined);
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      const id = ++frameId;
      pendingFrames.set(id, callback);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) =>
      pendingFrames.delete(id),
    );
    HTMLElement.prototype.scrollIntoView = vi.fn();
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("stops hidden lyrics and video, preserving mounted content and manual lyric scroll", async () => {
    const { container, store } = await renderDrawer();
    const line = screen.getByText("First line");
    fireEvent.scroll(line.parentElement!);
    expect(screen.getByText("Sync lyrics")).toBeTruthy();
    tick();
    const video = container.querySelector("video")!;
    expect(pendingFrames.size).toBe(1);
    vi.mocked(video.pause).mockClear();

    act(() => store.set(drawerOpenAtom, false));
    expect(pendingFrames.size).toBe(0);
    expect(video.pause).toHaveBeenCalled();
    expect(screen.getByText("First line")).toBe(line);
    expect(container.firstElementChild?.className).toContain("invisible");

    position.mockReturnValue(11);
    act(() => store.set(drawerOpenAtom, true));
    tick();
    expect(pendingFrames.size).toBe(1);
    expect(screen.getByText("Sync lyrics")).toBeTruthy();
    expect(screen.getByText("Second line").className).toContain(
      "text-th-text-primary",
    );
    expect(getLyrics).toHaveBeenCalledTimes(1);
    expect(video.play).toHaveBeenCalledTimes(2);
  });

  it("suspends work beneath fullscreen players and while the document is hidden", async () => {
    const { container, store } = await renderDrawer();
    const video = container.querySelector("video")!;
    expect(pendingFrames.size).toBe(1);

    act(() => store.set(maximizedPlayerAtom, true));
    expect(pendingFrames.size).toBe(0);
    act(() => store.set(maximizedPlayerAtom, false));
    expect(pendingFrames.size).toBe(1);
    act(() => {
      store.set(currentVideoAtom, { id: 42 } as TidalVideo);
      store.set(videoExpandedAtom, true);
    });
    expect(pendingFrames.size).toBe(0);
    act(() => store.set(videoExpandedAtom, false));
    expect(pendingFrames.size).toBe(1);

    act(() => {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(pendingFrames.size).toBe(0);
    const plays = vi.mocked(video.play).mock.calls.length;
    act(() => {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(pendingFrames.size).toBe(1);
    expect(video.play).toHaveBeenCalledTimes(plays + 1);
  });
});

import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import { Provider, createStore } from "jotai";
import type { PropsWithChildren } from "react";
import { videoCoversAtom } from "../atoms/ui";

// TidalImage proxies image bytes via Tauri — stub the bridge.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(new ArrayBuffer(0)),
}));

import TidalVideoCover from "./TidalVideoCover";

function renderWith(
  enabled: boolean,
  props: { cover?: string; videoCover?: string; active?: boolean },
  size: number | "origin" = 1280,
) {
  const store = createStore();
  store.set(videoCoversAtom, enabled);
  const wrapper = ({ children }: PropsWithChildren) => (
    <Provider store={store}>{children}</Provider>
  );
  return render(<TidalVideoCover size={size} alt="cover" {...props} />, {
    wrapper,
  });
}

describe("TidalVideoCover", () => {
  beforeEach(() => {
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
    vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
    vi.stubGlobal("IntersectionObserver", undefined);
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders a direct-streaming <video> with the CDN url when enabled and a videoCover exists", () => {
    const { container } = renderWith(true, {
      cover: "c-1",
      videoCover: "11-22",
    });
    expect(container.querySelector("video")?.getAttribute("src")).toBe(
      "https://resources.tidal.com/videos/11/22/1280x1280.mp4",
    );
    expect(container.querySelector("video")?.getAttribute("poster")).toBe(
      "https://resources.tidal.com/images/c/1/1280x1280.jpg",
    );
  });

  it("uses the origin url when size is 'origin'", () => {
    const { container } = renderWith(
      true,
      { cover: "c-1", videoCover: "11-22" },
      "origin",
    );
    expect(container.querySelector("video")?.getAttribute("src")).toBe(
      "https://resources.tidal.com/videos/11/22/origin.mp4",
    );
  });

  it("renders no <video> when the setting is disabled", () => {
    const { container } = renderWith(false, {
      cover: "c-1",
      videoCover: "11-22",
    });
    expect(container.querySelector("video")).toBeNull();
  });

  it("renders no <video> when there is no videoCover", () => {
    const { container } = renderWith(true, { cover: "c-1" });
    expect(container.querySelector("video")).toBeNull();
  });

  it("defers a closed cover's download and preserves playback time across close/open", () => {
    const props = {
      cover: "c-1",
      videoCover: "11-22",
      size: 1280,
      alt: "cover",
    };
    const { container, rerender } = renderWith(true, {
      ...props,
      active: false,
    });
    const video = container.querySelector("video")!;
    expect(video.getAttribute("src")).toBeNull();
    expect(video.autoplay).toBe(false);
    expect(video.preload).toBe("none");
    expect(video.play).not.toHaveBeenCalled();

    rerender(<TidalVideoCover {...props} active />);
    expect(video.play).toHaveBeenCalledTimes(1);
    video.currentTime = 8;

    rerender(<TidalVideoCover {...props} active={false} />);
    expect(video.pause).toHaveBeenCalled();
    rerender(<TidalVideoCover {...props} active />);
    expect(container.querySelector("video")).toBe(video);
    expect(video.currentTime).toBe(8);
    expect(video.play).toHaveBeenCalledTimes(2);

    rerender(<TidalVideoCover {...props} videoCover="33-44" active={false} />);
    expect(container.querySelector("video")?.getAttribute("src")).toBeNull();
    expect(video.play).toHaveBeenCalledTimes(2);
  });

  it("pauses on document hide and resumes only if its parent is still active", () => {
    const props = {
      cover: "c-1",
      videoCover: "11-22",
      size: 1280,
      alt: "cover",
    };
    const { container, rerender } = renderWith(true, props);
    const video = container.querySelector("video")!;
    vi.mocked(video.pause).mockClear();

    act(() => {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(video.pause).toHaveBeenCalled();
    expect(video.play).toHaveBeenCalledTimes(1);

    rerender(<TidalVideoCover {...props} active={false} />);
    act(() => {
      vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(video.play).toHaveBeenCalledTimes(1);

    rerender(<TidalVideoCover {...props} active />);
    expect(video.play).toHaveBeenCalledTimes(2);
  });

  it("starts on viewport entry, pauses offscreen, and disconnects its observer", () => {
    let onIntersect: IntersectionObserverCallback;
    const observe = vi.fn();
    const disconnect = vi.fn();
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(callback: IntersectionObserverCallback) {
          onIntersect = callback;
        }
        observe = observe;
        disconnect = disconnect;
      },
    );
    const { container, unmount } = renderWith(true, {
      cover: "c-1",
      videoCover: "11-22",
    });
    const video = container.querySelector("video")!;
    const intersect = (isIntersecting: boolean) =>
      act(() => {
        onIntersect(
          [{ isIntersecting } as IntersectionObserverEntry],
          {} as IntersectionObserver,
        );
      });

    expect(observe).toHaveBeenCalledWith(video);
    expect(video.getAttribute("src")).toBeNull();
    intersect(true);
    expect(video.play).toHaveBeenCalledTimes(1);
    vi.mocked(video.pause).mockClear();
    intersect(false);
    expect(video.pause).toHaveBeenCalled();
    intersect(true);
    expect(video.play).toHaveBeenCalledTimes(2);
    unmount();
    expect(disconnect).toHaveBeenCalledTimes(1);
  });
});

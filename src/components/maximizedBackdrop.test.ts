/// <reference types="vite/client" />
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import playerSource from "./MaximizedPlayer.tsx?raw";
import { fetchCachedImageUrl } from "../lib/imageCache";
import {
  admitImage,
  isImageReady,
  rememberReadyImage,
} from "../lib/imageAdmission";
import {
  backdropDimensions,
  bakeMaximizedBackdrop,
  finishBakedBackdrop,
  gradeLightBackdrop,
  maximizedWashStyle,
} from "./maximizedBackdrop";

vi.mock("../lib/imageCache", () => ({ fetchCachedImageUrl: vi.fn() }));
vi.mock("../lib/imageAdmission", () => ({
  admitImage: vi.fn(),
  isImageReady: vi.fn(() => false),
  rememberReadyImage: vi.fn(),
}));

describe("maximized player backdrop", () => {
  it("lifts a neutral pixel by the old brightness and pulls chroma inward", () => {
    const pixels = new Uint8ClampedArray([100, 100, 100, 255, 200, 0, 0, 128]);
    gradeLightBackdrop(pixels);

    expect(Array.from(pixels.slice(0, 4))).toEqual([160, 160, 160, 255]);
    expect(pixels[5]).toBeGreaterThan(0);
    expect(pixels[6]).toBeGreaterThan(0);
    expect(pixels[4]).toBeGreaterThan(pixels[5]);
    expect(pixels[7]).toBe(128);
    for (const channel of pixels) {
      expect(channel).toBeGreaterThanOrEqual(0);
      expect(channel).toBeLessThanOrEqual(255);
    }
  });

  it("grades only the light-theme bake", () => {
    const light = new Uint8ClampedArray([100, 100, 100, 255]);
    const dark = new Uint8ClampedArray([100, 100, 100, 255]);
    finishBakedBackdrop(light, false);
    finishBakedBackdrop(dark, true);

    expect(Array.from(light)).toEqual([160, 160, 160, 255]);
    expect(Array.from(dark)).toEqual([100, 100, 100, 255]);
  });

  it("washes the window with a solid color", () => {
    for (const isDark of [true, false]) {
      const wash = maximizedWashStyle(isDark, "240,240,240");
      expect(wash.className).not.toMatch(/backdrop-/);
      expect(wash.backgroundColor).not.toMatch(/backdrop/);
    }
    expect(maximizedWashStyle(true, "0,0,0").backgroundColor).toBe(
      "rgba(0,0,0,0.6)",
    );
    expect(maximizedWashStyle(false, "240,240,240").backgroundColor).toBe(
      "rgba(240,240,240,0.45)",
    );
  });

  it("keeps the maximized player off a live backdrop filter", () => {
    expect(playerSource).not.toMatch(/backdrop-brightness|backdrop-saturate/);
    expect(playerSource).toContain("bakeMaximizedBackdrop(");
    expect(playerSource).toContain("maximizedWashStyle(");
  });
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

function context() {
  return {
    drawImage: vi.fn(),
    getImageData: vi.fn(
      (_x: number, _y: number, width: number, height: number) => ({
        data: new Uint8ClampedArray(width * height * 4),
      }),
    ),
    putImageData: vi.fn(),
  };
}

describe("bounded backdrop loading", () => {
  const images: HTMLImageElement[] = [];
  const cleanups: Array<() => void> = [];
  let visible: ReturnType<typeof context>;
  let offscreen: ReturnType<typeof context>;
  let canvas: HTMLCanvasElement;
  let release: ReturnType<typeof vi.fn<() => void>>;
  let start: ((done: () => void) => void) | undefined;

  const flush = async () => {
    for (let i = 0; i < 6; i++) await Promise.resolve();
  };
  const bake = (src = "cover") => {
    const cancel = bakeMaximizedBackdrop(canvas, src, true, 1920, 1080);
    cleanups.push(cancel);
    return cancel;
  };

  beforeEach(() => {
    vi.clearAllMocks();
    images.length = 0;
    cleanups.length = 0;
    start = undefined;
    canvas = document.createElement("canvas");
    canvas.width = 12;
    canvas.height = 8;
    visible = context();
    offscreen = context();
    release = vi.fn();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(
      function (this: HTMLCanvasElement) {
        return (this === canvas
          ? visible
          : offscreen) as unknown as CanvasRenderingContext2D;
      },
    );
    vi.stubGlobal(
      "Image",
      vi.fn(function () {
        const img = document.createElement("img");
        img.width = img.height = 320;
        img.decode = vi.fn().mockResolvedValue(undefined);
        images.push(img);
        return img;
      }),
    );
    vi.mocked(fetchCachedImageUrl).mockResolvedValue("blob:cover");
    vi.mocked(isImageReady).mockReturnValue(false);
    vi.mocked(admitImage).mockImplementation((callback) => {
      start = callback;
      return release;
    });
  });

  afterEach(() => {
    cleanups.forEach((cancel) => cancel());
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it.each([
    [1920, 1080, 480, 270],
    [1080, 1920, 180, 320],
    [4000, 1000, 480, 120],
    [200, 100, 200, 100],
  ])(
    "bounds a %d×%d viewport to %d×%d without changing its aspect ratio",
    (width, height, expectedWidth, expectedHeight) => {
      expect(backdropDimensions(width, height)).toEqual({
        width: expectedWidth,
        height: expectedHeight,
      });
    },
  );

  it("preserves signed URLs and waits for admission and decode before publishing", async () => {
    const signed = "https://art.example/cover?token=a%2Bb&expires=123#v1";
    vi.mocked(isImageReady).mockReturnValue(true);
    bake(signed);
    expect(fetchCachedImageUrl).toHaveBeenCalledWith(signed, {
      signal: expect.any(AbortSignal),
    });
    await flush();
    expect(admitImage).toHaveBeenCalledWith(expect.any(Function), true);
    expect(isImageReady).toHaveBeenCalledWith("blob:cover");
    expect(images[0].getAttribute("src")).toBeNull();
    expect(canvas.width).toBe(12);
    expect(visible.drawImage).not.toHaveBeenCalled();
    start!(release);
    const decoded = deferred<void>();
    vi.mocked(images[0].decode).mockReturnValue(decoded.promise);
    images[0].dispatchEvent(new Event("load"));
    await flush();
    expect(visible.drawImage).not.toHaveBeenCalled();
    decoded.resolve();
    await flush();
    expect(canvas.width).toBe(480);
    expect(canvas.height).toBe(270);
    expect(visible.drawImage).toHaveBeenCalledTimes(1);
    expect(rememberReadyImage).toHaveBeenCalledWith("blob:cover");
    expect(release).toHaveBeenCalledTimes(1);
  });

  it("aborts a superseded fetch without admitting it or clearing the prior canvas", async () => {
    const fetch = deferred<string>();
    vi.mocked(fetchCachedImageUrl).mockReturnValue(fetch.promise);
    const cancel = bake();
    const signal = vi.mocked(fetchCachedImageUrl).mock.calls[0][1]!.signal!;
    cancel();
    expect(signal.aborted).toBe(true);
    fetch.resolve("blob:stale");
    await flush();
    expect(admitImage).not.toHaveBeenCalled();
    expect(visible.drawImage).not.toHaveBeenCalled();
    expect(canvas.width).toBe(12);
  });

  it("releases queued admission on cancellation", async () => {
    const cancel = bake();
    await flush();
    cancel();
    expect(release).toHaveBeenCalledTimes(1);
    expect(images[0].getAttribute("src")).toBeNull();
  });

  it("cannot overwrite a newer backdrop after an old decode completes late", async () => {
    const cancelOld = bake("old");
    await flush();
    start!(release);
    const oldDecode = deferred<void>();
    vi.mocked(images[0].decode).mockReturnValue(oldDecode.promise);
    images[0].dispatchEvent(new Event("load"));
    cancelOld();
    bake("new");
    await flush();
    start!(release);
    images[1].dispatchEvent(new Event("load"));
    await flush();
    expect(visible.drawImage).toHaveBeenCalledTimes(1);
    oldDecode.resolve();
    await flush();
    expect(visible.drawImage).toHaveBeenCalledTimes(1);
  });

  it.each(["fetch", "image", "canvas"])(
    "retains the previous bitmap after a %s failure",
    async (failure) => {
      if (failure === "fetch")
        vi.mocked(fetchCachedImageUrl).mockRejectedValue(new Error("offline"));
      if (failure === "canvas")
        offscreen.getImageData.mockImplementation(() => {
          throw new Error("canvas failed");
        });
      bake();
      await flush();
      if (failure !== "fetch") {
        start!(release);
        images[0].dispatchEvent(
          new Event(failure === "image" ? "error" : "load"),
        );
        await flush();
        expect(release).toHaveBeenCalledTimes(1);
      }
      expect(visible.drawImage).not.toHaveBeenCalled();
      expect(canvas.width).toBe(12);
      expect(canvas.height).toBe(8);
    },
  );
});

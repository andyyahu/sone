import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

import TidalImage, { fetchCachedImageUrl } from "./TidalImage";
import { PageScrollProvider } from "../contexts/PageScrollContext";

const observers: ViewportObserver[] = [];
class ViewportObserver {
  targets = new Set<Element>();
  constructor(
    private callback: IntersectionObserverCallback,
    readonly options: IntersectionObserverInit,
  ) {
    observers.push(this);
  }
  observe(target: Element) {
    this.targets.add(target);
  }
  unobserve(target: Element) {
    this.targets.delete(target);
  }
  disconnect() {
    this.targets.clear();
  }
  reveal() {
    this.callback(
      [...this.targets].map(
        (target) =>
          ({ target, isIntersecting: true }) as IntersectionObserverEntry,
      ),
      this as unknown as IntersectionObserver,
    );
  }
}

let nextBlob = 0;
let frames: Map<number, FrameRequestCallback>;
let nextFrameId = 0;

async function nextFrame() {
  await act(async () => {
    const callbacks = [...frames.values()];
    frames.clear();
    callbacks.forEach((callback) => callback(nextFrameId));
  });
}

beforeEach(() => {
  frames = new Map();
  observers.length = 0;
  invoke.mockReset().mockResolvedValue(new ArrayBuffer(8));
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrameId, callback);
    return nextFrameId;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("IntersectionObserver", ViewportObserver);
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => `blob:image-${++nextBlob}`),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(async () => {
  cleanup();
  while (frames.size) await nextFrame();
  vi.unstubAllGlobals();
});

describe("viewport-aware image loading", () => {
  it("waits for visibility, sharing both the observer and duplicate requests", async () => {
    const { container } = render(
      <>
        <TidalImage src="lazy-shared" alt="First" />
        <TidalImage src="lazy-shared" alt="Second" />
      </>,
    );
    expect(observers).toHaveLength(1);
    expect(invoke).not.toHaveBeenCalled();
    await act(async () => observers[0].reveal());
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(container.querySelectorAll("img")).toHaveLength(2);
    expect(observers[0].targets.size).toBe(0);
  });

  it("does not fetch an offscreen image that unmounts", async () => {
    const { unmount } = render(<TidalImage src="unmounted" alt="Unused" />);
    unmount();
    await act(async () => observers[0].reveal());
    expect(invoke).not.toHaveBeenCalled();
    expect(observers[0].targets.size).toBe(0);
  });

  it("prefetches IPC covers relative to the page scroll root", async () => {
    const root = document.createElement("div");
    document.body.append(root);
    render(
      <PageScrollProvider element={root}>
        <TidalImage src="page-scroll-ipc" alt="Page cover" />
      </PageScrollProvider>,
      { container: root },
    );
    expect(observers[0].options).toEqual({ root, rootMargin: "400px" });
    expect(invoke).not.toHaveBeenCalled();
    await act(async () => observers[0].reveal());
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("supports eager covers and cached images without waiting for intersection", async () => {
    let first: ReturnType<typeof render>;
    await act(async () => {
      first = render(
        <TidalImage src="eager-cached" alt="Hero" loading="eager" />,
      );
    });
    expect(invoke).toHaveBeenCalledTimes(1);
    first!.unmount();
    const { container } = render(
      <TidalImage src="eager-cached" alt="Cached" />,
    );
    expect(container.querySelector("img")).not.toBeNull();
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(observers).toHaveLength(0);
  });

  it("ignores a late response from a previous source", async () => {
    let resolveOld!: (buffer: ArrayBuffer) => void;
    invoke.mockImplementation((_command, args: { url: string }) =>
      args.url === "old-source"
        ? new Promise<ArrayBuffer>((resolve) => {
            resolveOld = resolve;
          })
        : Promise.resolve(new ArrayBuffer(8)),
    );
    const { container, rerender } = render(
      <TidalImage src="old-source" alt="Cover" loading="eager" />,
    );
    await act(async () => {
      rerender(<TidalImage src="new-source" alt="Cover" loading="eager" />);
    });
    try {
      await nextFrame();
      const newBlob = container.querySelector("img")?.getAttribute("src");
      expect(newBlob).toBeTruthy();
      await act(async () => resolveOld(new ArrayBuffer(8)));
      await nextFrame();
      expect(container.querySelector("img")?.getAttribute("src")).toBe(newBlob);
    } finally {
      // Do not strand an IPC slot if an assertion above fails.
      await act(async () => resolveOld(new ArrayBuffer(8)));
    }
  });
});

describe("image request backpressure", () => {
  it("bounds concurrent IPC requests and continues after a failure", async () => {
    let active = 0;
    let peak = 0;
    const finish: Array<(fail?: boolean) => void> = [];
    invoke.mockImplementation(
      () =>
        new Promise<ArrayBuffer>((resolve, reject) => {
          peak = Math.max(peak, ++active);
          finish.push((fail = false) => {
            active--;
            if (fail) reject(new Error("Image unavailable"));
            else resolve(new ArrayBuffer(8));
          });
        }),
    );
    const requests = Array.from({ length: 20 }, (_, i) =>
      fetchCachedImageUrl(`bounded-${i}`).catch(() => null),
    );
    expect(invoke).toHaveBeenCalledTimes(6);
    const duplicate = fetchCachedImageUrl("bounded-1");
    expect(fetchCachedImageUrl("bounded-1")).toBe(duplicate);
    // Complete each wave, including a failed request in the first one.
    for (let wave = 0; wave < 4; wave++) {
      await act(async () => {
        const current = finish.splice(0);
        current.forEach((done, i) => done(wave === 0 && i === 0));
      });
    }
    const results = await Promise.all(requests);
    expect(peak).toBe(6);
    expect(invoke).toHaveBeenCalledTimes(20);
    expect(results.filter(Boolean)).toHaveLength(19);
    expect(active).toBe(0);
    invoke.mockResolvedValue(new ArrayBuffer(8));
    await expect(fetchCachedImageUrl("bounded-0")).resolves.toMatch(/^blob:/);
  });
});

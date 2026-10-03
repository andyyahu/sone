import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => Promise.resolve(undefined)),
}));

vi.mock("../lib/imageCache", () => ({
  fetchCachedImageUrl: vi.fn(() => Promise.resolve("blob:source")),
}));

import CoverBanner from "./CoverBanner";

let frames: Map<number, FrameRequestCallback>;
let sequence = 0;
const bakedSizes: Array<{ width: number; height: number }> = [];
const originalGetContext = HTMLCanvasElement.prototype.getContext;
const originalToBlob = HTMLCanvasElement.prototype.toBlob;
const originalClientWidth = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "clientWidth",
);
const originalClientHeight = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "clientHeight",
);

class StubImage {
  naturalWidth = 640;
  naturalHeight = 640;
  onload: null | (() => void) = null;
  onerror: null | (() => void) = null;
  set src(_value: string) {
    this.onload?.();
  }
}

async function nextFrame() {
  await act(async () => {
    const callbacks = [...frames.values()];
    frames.clear();
    callbacks.forEach((callback) => callback(sequence));
  });
}

beforeEach(() => {
  frames = new Map();
  sequence = 0;
  bakedSizes.length = 0;
  vi.stubGlobal("Image", StubImage);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++sequence, callback);
    return sequence;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("URL", {
    createObjectURL: () => "blob:baked",
    revokeObjectURL: () => {},
  });
  Object.defineProperty(HTMLElement.prototype, "clientWidth", {
    configurable: true,
    get: () => 1600,
  });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", {
    configurable: true,
    get: () => 420,
  });
  HTMLCanvasElement.prototype.getContext = vi.fn(() => ({
    filter: "",
    drawImage: vi.fn(),
  })) as unknown as typeof HTMLCanvasElement.prototype.getContext;
  HTMLCanvasElement.prototype.toBlob = function (callback) {
    bakedSizes.push({ width: this.width, height: this.height });
    callback?.(new Blob(["baked"], { type: "image/jpeg" }));
  };
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  HTMLCanvasElement.prototype.getContext = originalGetContext;
  HTMLCanvasElement.prototype.toBlob = originalToBlob;
  if (originalClientWidth) {
    Object.defineProperty(
      HTMLElement.prototype,
      "clientWidth",
      originalClientWidth,
    );
  } else {
    Reflect.deleteProperty(HTMLElement.prototype, "clientWidth");
  }
  if (originalClientHeight) {
    Object.defineProperty(
      HTMLElement.prototype,
      "clientHeight",
      originalClientHeight,
    );
  } else {
    Reflect.deleteProperty(HTMLElement.prototype, "clientHeight");
  }
});

describe("CoverBanner", () => {
  it("bakes the playlist blur into a capped bitmap with no live filter", async () => {
    const { container } = render(
      <CoverBanner src="https://resources.tidal.com/cover/640.jpg" />,
    );

    expect(container.querySelector("[class*='blur-']")).toBeNull();
    expect(container.innerHTML).not.toContain("blur(");

    await act(async () => {
      await Promise.resolve();
    });
    await nextFrame();

    expect(container.querySelector("[data-cover-blur='baked']")).not.toBeNull();
    expect(container.querySelector("[class*='blur-']")).toBeNull();
    expect(bakedSizes).toEqual([{ width: 480, height: 126 }]);
  });
});

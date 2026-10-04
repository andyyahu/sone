/// <reference types="vite/client" />
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import profilePageSource from "./ProfilePage.tsx?raw";
import {
  ProfileHeroFrost,
  bakeProfileFrost,
  profileFrostClass,
} from "./profileHeroFrost";

let frames: Map<number, FrameRequestCallback>;
let sequence = 0;
const filters: string[] = [];
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
  filters.length = 0;
  vi.stubGlobal("Image", StubImage);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++sequence, callback);
    return sequence;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("URL", {
    createObjectURL: () => "blob:profile-frost",
    revokeObjectURL: () => {},
  });
  Object.defineProperty(HTMLElement.prototype, "clientWidth", {
    configurable: true,
    get: () => 1600,
  });
  Object.defineProperty(HTMLElement.prototype, "clientHeight", {
    configurable: true,
    get: () => 480,
  });
  HTMLCanvasElement.prototype.getContext = vi.fn(() => {
    const ctx = { filter: "", drawImage: vi.fn() };
    return new Proxy(ctx, {
      set(target, prop, value) {
        if (prop === "filter") filters.push(String(value));
        target.filter = String(value);
        return true;
      },
    });
  }) as unknown as typeof HTMLCanvasElement.prototype.getContext;
  HTMLCanvasElement.prototype.toBlob = function (callback) {
    callback?.(new Blob(["frost"], { type: "image/jpeg" }));
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

describe("profile hero frost", () => {
  it("bakes the profile hero frost once and does not keep a live blur", async () => {
    expect(profileFrostClass()).not.toMatch(/blur|filter|backdrop/);

    const { container } = render(<ProfileHeroFrost src="blob:profile-hero" />);
    expect(container.querySelector("[class*='blur-']")).toBeNull();
    expect(container.innerHTML).not.toContain("blur(");

    await nextFrame();

    const baked = container.querySelector("[data-hero-frost='baked']");
    expect(baked).not.toBeNull();
    expect(baked?.className).toBe(profileFrostClass());
    expect(container.querySelector("[class*='blur-']")).toBeNull();
    expect(filters.some((filter) => filter.startsWith("blur("))).toBe(true);

    const direct = bakeProfileFrost(
      new StubImage() as CanvasImageSource,
      800,
      480,
    );
    expect(direct).not.toBeNull();
    expect(direct?.width).toBeLessThanOrEqual(480);
    expect(direct?.height).toBeLessThanOrEqual(320);
  });

  it("keeps the profile page off a live hero blur", () => {
    expect(profilePageSource).not.toMatch(/blur-3xl|blur-\[|backdrop-blur/);
    expect(profilePageSource).toContain("<ProfileHeroFrost ");
  });
});

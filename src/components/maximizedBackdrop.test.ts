/// <reference types="vite/client" />
import { describe, expect, it } from "vitest";
import playerSource from "./MaximizedPlayer.tsx?raw";
import {
  finishBakedBackdrop,
  gradeLightBackdrop,
  maximizedWashStyle,
} from "./maximizedBackdrop";

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
    expect(playerSource).toContain("finishBakedBackdrop(");
    expect(playerSource).toContain("maximizedWashStyle(");
  });
});

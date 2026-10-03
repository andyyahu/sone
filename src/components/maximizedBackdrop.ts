/** One-time light-theme grade for the maximized player's baked bitmap.
 *  backdrop-brightness(1.6) then backdrop-saturate(0.5) would resample the
 *  whole window every frame. Brightness is applied first, matching that
 *  order. Rec. 709 weights are the ones CSS saturate() uses. */

const BRIGHTNESS = 1.6;
const SATURATION = 0.5;

function clamp8(v: number): number {
  if (v <= 0) return 0;
  if (v >= 255) return 255;
  return Math.round(v);
}

export function gradeLightBackdrop(data: Uint8ClampedArray): void {
  const keep = SATURATION;
  const gray = 1 - keep;
  for (let i = 0; i < data.length; i += 4) {
    const r = data[i] * BRIGHTNESS;
    const g = data[i + 1] * BRIGHTNESS;
    const b = data[i + 2] * BRIGHTNESS;
    const y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    data[i] = clamp8(y * gray + r * keep);
    data[i + 1] = clamp8(y * gray + g * keep);
    data[i + 2] = clamp8(y * gray + b * keep);
  }
}

/** Dark themes never used a backdrop grade. Light themes take it once. */
export function finishBakedBackdrop(
  data: Uint8ClampedArray,
  isDark: boolean,
): void {
  if (!isDark) gradeLightBackdrop(data);
}

/** Solid wash over the baked bitmap. No backdrop-filter. */
export function maximizedWashStyle(
  isDark: boolean,
  bgBaseRgb: string,
): { className: string; backgroundColor: string } {
  return {
    className: "absolute inset-0",
    backgroundColor: isDark ? "rgba(0,0,0,0.6)" : `rgba(${bgBaseRgb},0.45)`,
  };
}
